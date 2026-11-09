use crate::logger::{log_err, log_net, log_ureq_err};
use crate::state::{self, MetaDetail};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct CinemetaCatalogResponse {
    #[serde(default)]
    pub metas: Vec<MetaItem>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct CinemetaMetaResponse {
    pub meta: MetaDetail,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct MetaItem {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub item_type: Option<String>,
    #[serde(default, deserialize_with = "crate::state::deserialize_string_or_number")]
    pub year: Option<String>,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "releaseInfo", deserialize_with = "crate::state::deserialize_string_or_number")]
    pub release_info: Option<String>,
    #[serde(default, rename = "imdbRating", deserialize_with = "crate::state::deserialize_string_or_number")]
    pub imdb_rating: Option<String>,
    #[serde(default, deserialize_with = "crate::state::deserialize_string_or_number")]
    pub runtime: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
struct TorrentioResponse {
    #[serde(default)]
    pub streams: Vec<RawTorrentioStream>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
struct RawTorrentioStream {
    pub name: Option<String>,
    pub title: Option<String>,
    #[serde(rename = "infoHash")]
    pub info_hash: Option<String>,
    #[serde(rename = "fileIdx")]
    pub file_idx: Option<usize>,
    pub sources: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct CompatibleStream {
    pub display_title: String,
    pub quality: String,
    pub size: String,
    pub seeders: u32,
    pub info_hash: String,
    pub file_idx: Option<usize>,
    pub trackers: Vec<String>,
    pub container: String,
    pub audio_codec: String,
    pub is_mp4: bool,
    pub is_pack: bool,
    pub score: i32,
    pub raw_title: String,
}

/// Creates a ureq client configured with timeouts and Windows RT Schannel TLS connector.
pub fn create_agent() -> Result<ureq::Agent, String> {
    let tls_connector = native_tls::TlsConnector::new()
        .map_err(|e| format!("Schannel TlsConnector init error: {}", e))?;

    let agent = ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(tls_connector))
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Windows NT 6.3; ARM; Trident/7.0; rv:11.0) like Gecko")
        .build();

    Ok(agent)
}

static SHARED_AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();

/// Returns a shared, pooled ureq agent reusing Schannel TLS sessions and keep-alive TCP connections
pub fn get_shared_agent() -> &'static ureq::Agent {
    SHARED_AGENT.get_or_init(|| {
        create_agent().unwrap_or_else(|e| {
            log_err(&format!("Falling back in get_shared_agent: {}", e));
            ureq::AgentBuilder::new().build()
        })
    })
}

/// Simple percent-encoder for search terms and tracker URLs
pub fn percent_encode(s: &str) -> String {
    let mut res = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                res.push(b as char);
            }
            b' ' => {
                res.push_str("%20");
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(res, "%{:02X}", b);
            }
        }
    }
    res
}

/// Resolves the base URL for Torrentio.
pub fn get_torrentio_base_url() -> String {
    let cfg = state::load_addons_config();
    for addon in &cfg.addons {
        if addon.manifest.id == "com.stremio.torrentio" {
            let tr = addon.transport_url.trim_end_matches("/manifest.json").trim_end_matches('/');
            return tr.to_string();
        }
    }

    if let Ok(val) = std::env::var("RT_STREAM_TORRENTIO_URL") {
        let trimmed = val.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }

    "https://torrentio.strem.fun".to_string()
}

/// Fetches a specific Cinemeta catalog (e.g. ("movie", "top", None) or ("series", "top", Some("genre=Action")))
pub fn fetch_cinemeta_catalog(
    catalog_type: &str,
    catalog_id: &str,
    extra: Option<&str>,
) -> Result<Vec<MetaItem>, String> {
    let agent = get_shared_agent();
    let url = if let Some(ext) = extra {
        format!(
            "https://v3-cinemeta.strem.io/catalog/{}/{}/{}.json",
            catalog_type, catalog_id, ext
        )
    } else {
        format!(
            "https://v3-cinemeta.strem.io/catalog/{}/{}.json",
            catalog_type, catalog_id
        )
    };

    log_net(&format!("Loading Cinemeta catalog: {}", url));
    let resp = agent.get(&url).call().map_err(|e| format!("Cinemeta HTTP error: {}", e))?;
    let parsed: CinemetaCatalogResponse = resp.into_json().map_err(|e| format!("Cinemeta JSON error: {}", e))?;
    Ok(parsed.metas)
}

/// Loads initial Popular (Top) catalog from Cinemeta
pub fn load_popular_catalog(filter_type: Option<&str>) -> Result<Vec<MetaItem>, String> {
    let mut all = Vec::new();

    let fetch_movies = filter_type.is_none() || filter_type == Some("all") || filter_type == Some("movie");
    let fetch_series = filter_type.is_none() || filter_type == Some("all") || filter_type == Some("series");

    if fetch_movies {
        if let Ok(movies) = fetch_cinemeta_catalog("movie", "top", None) {
            all.extend(movies);
        }
    }

    if fetch_series {
        if let Ok(series) = fetch_cinemeta_catalog("series", "top", None) {
            all.extend(series);
        }
    }

    log_net(&format!("Popular catalog loaded: {} titles.", all.len()));
    Ok(all)
}

/// Searches Cinemeta for movies and series matching the query with relevance scoring.
pub fn search_cinemeta(query: &str, filter_type: Option<&str>) -> Result<Vec<MetaItem>, String> {
    let query_clean = query.trim();
    if query_clean.is_empty() {
        return load_popular_catalog(filter_type);
    }

    let encoded = percent_encode(query_clean);
    let mut all_results = Vec::new();

    let search_movies = filter_type.is_none() || filter_type == Some("all") || filter_type == Some("movie");
    let search_series = filter_type.is_none() || filter_type == Some("all") || filter_type == Some("series");

    let q_lower = query_clean.to_lowercase();
    let q_words: Vec<&str> = q_lower.split_whitespace().collect();

    if search_movies {
        let extra = format!("search={}", encoded);
        if let Ok(movies) = fetch_cinemeta_catalog("movie", "top", Some(&extra)) {
            all_results.extend(movies);
        }
    }

    if search_series {
        let extra = format!("search={}", encoded);
        if let Ok(series) = fetch_cinemeta_catalog("series", "top", Some(&extra)) {
            all_results.extend(series);
        }
    }

    // Relevance scoring
    all_results.sort_by_cached_key(|item| {
        let title_lower = item.name.to_lowercase();
        let mut score: i32 = 0;

        if title_lower == q_lower {
            score += 10000;
        } else if title_lower.starts_with(&q_lower) {
            score += 5000;
        } else if title_lower.contains(&q_lower) {
            score += 2000;
        }

        for word in &q_words {
            if title_lower.contains(word) {
                score += 500;
            }
        }

        if let Some(r_str) = &item.imdb_rating {
            if let Ok(r) = r_str.parse::<f32>() {
                score += (r * 100.0) as i32;
            }
        }

        // Descending order sort key
        -score
    });

    Ok(all_results)
}

/// Fetches full metadata details including series episode list from Cinemeta
pub fn fetch_meta_detail(item_type: &str, id: &str) -> Result<MetaDetail, String> {
    let agent = get_shared_agent();
    let url = format!("https://v3-cinemeta.strem.io/meta/{}/{}.json", item_type, id);
    log_net(&format!("Fetching metadata details: {}", url));

    let resp = agent.get(&url).call().map_err(|e| e.to_string())?;
    let parsed: CinemetaMetaResponse = resp.into_json().map_err(|e| e.to_string())?;
    Ok(parsed.meta)
}

/// Fetches, scores and sorts streams from Torrentio for Surface RT hardware (Tegra 3 DXVA2)
pub fn fetch_streams(
    item_type: &str,
    imdb_id: &str,
    season: u32,
    episode: u32,
) -> Result<Vec<CompatibleStream>, String> {
    let base_url = get_torrentio_base_url();
    let stream_url = if item_type == "series" {
        format!("{}/stream/series/{}:{}:{}.json", base_url, imdb_id, season, episode)
    } else {
        format!("{}/stream/movie/{}.json", base_url, imdb_id)
    };

    log_net(&format!("Querying streams from Torrentio: {}", stream_url));

    let agent = match create_agent() {
        Ok(a) => a,
        Err(e) => {
            log_err(&e);
            return Err(e);
        }
    };
    let resp = match agent.get(&stream_url).call() {
        Ok(r) => r,
        Err(e) => {
            log_ureq_err("Failed to query streams from Torrentio", &e);
            return Err(format!("Torrentio request error: {}", e));
        }
    };

    let parsed: TorrentioResponse = match resp.into_json() {
        Ok(p) => p,
        Err(e) => {
            log_err(&format!("Error parsing Torrentio JSON: {}", e));
            return Err(format!("Invalid Torrentio JSON: {}", e));
        }
    };

    log_net(&format!(
        "Torrentio reported {} total streams before filtering.",
        parsed.streams.len()
    ));

    let settings = crate::state::load_settings();
    let max_height = settings.playback.max_height;
    let prefer_mp4 = settings.playback.prefer_mp4;
    let audio_lang = settings.playback.audio_lang.trim().to_lowercase();

    let mut compatible = Vec::new();

    for item in parsed.streams {
        let info_hash = match item.info_hash {
            Some(h) if h.len() == 40 => h.to_lowercase(),
            _ => continue,
        };

        // Extract trackers
        let mut trackers = Vec::new();
        if let Some(srcs) = item.sources {
            for s in srcs {
                let clean_tracker = if let Some(stripped) = s.strip_prefix("tracker:") {
                    stripped.to_string()
                } else {
                    s
                };
                if clean_tracker.starts_with("http://")
                    || clean_tracker.starts_with("https://")
                    || clean_tracker.starts_with("udp://")
                {
                    trackers.push(clean_tracker);
                }
            }
        }

        let raw_title = item.title.unwrap_or_default();
        let upper = raw_title.to_uppercase();

        // STRICT HARDWARE LIMIT REJECTION FOR TEGRA 3 (Windows RT 8.1):
        // 4K, HEVC/x265, 10-bit and HDR cause 100% CPU lockup and lack hardware DXVA2 decode.
        let is_rejected = upper.contains("2160P")
            || upper.contains("4K")
            || upper.contains("UHD")
            || upper.contains("HEVC")
            || upper.contains("X265")
            || upper.contains("265")
            || upper.contains("H265")
            || upper.contains("H.265")
            || upper.contains("10BIT")
            || upper.contains("10-BIT")
            || upper.contains("10 BIT")
            || upper.contains("HI10P")
            || upper.contains("HDR")
            || upper.contains("HDR10")
            || upper.contains("HLG")
            || upper.contains("DV")
            || upper.contains("DOVI")
            || upper.contains("AV1")
            || upper.contains("AV01")
            || upper.contains("VP9");

        if is_rejected {
            continue;
        }

        let is_1080p = upper.contains("1080P") || upper.contains("FHD");
        let is_720p = (upper.contains("720P") || upper.contains("HD")) && !is_1080p;
        let is_480p = upper.contains("480P");
        let is_sd = upper.contains("576P")
            || upper.contains("BDRIP")
            || upper.contains("BRRIP")
            || upper.contains("DVDRIP")
            || upper.contains("DVD-RIP");

        // Resolution limit filter based on settings.playback.max_height
        if max_height < 1080 && is_1080p {
            continue;
        }
        if max_height <= 480 && (is_720p || is_1080p) {
            continue;
        }

        // For 1080p, strictly ensure H.264 compatibility (Tegra 3 only decodes 8-bit H.264 up to 1080p)
        if is_1080p && !upper.contains("X264") && !upper.contains("H264") && !upper.contains("H.264") && !upper.contains("AVC") && !upper.contains(".MP4") {
            continue;
        }

        if !is_1080p && !is_720p && !is_480p && !is_sd {
            if !upper.contains("X264") && !upper.contains("H264") && !upper.contains("H.264") {
                continue;
            }
        }

        let quality_tag = if is_1080p {
            "1080p"
        } else if is_720p {
            "720p"
        } else if is_480p {
            "480p"
        } else {
            "SD"
        };

        // Determine Container: MP4 vs MKV
        let is_mp4 = upper.contains(".MP4")
            || upper.contains(" MP4")
            || upper.contains("-MP4")
            || upper.contains("[MP4]");

        let container_tag = if is_mp4 { "MP4" } else { "MKV" };

        // Determine Audio Codec: AAC / MP3 vs AC3 / EAC3 / DTS
        let is_aac = upper.contains("AAC") || upper.contains("MP3");
        let is_ac3 = upper.contains("AC3") || upper.contains("AC-3") || upper.contains("DD5.1");
        let is_eac3 = upper.contains("EAC3") || upper.contains("E-AC-3") || upper.contains("DDP") || upper.contains("DD+");
        let is_dts = upper.contains("DTS");

        let audio_tag = if is_aac {
            "AAC"
        } else if is_eac3 {
            "EAC3"
        } else if is_ac3 {
            "AC3"
        } else if is_dts {
            "DTS"
        } else {
            "Standard"
        };

        let lines: Vec<&str> = raw_title.lines().collect();
        let release_name = lines.get(0).copied().unwrap_or("Torrent Stream").trim();

        let mut size = "N/A".to_string();
        let mut seeders_num: u32 = 0;

        for line in lines.iter().skip(1) {
            let mut prev_word = "";
            for part in line.split_whitespace() {
                let p_upper = part.to_uppercase();
                if p_upper == "GB" || p_upper == "MB" || p_upper == "GIB" || p_upper == "MIB" {
                    if !prev_word.is_empty() && prev_word.chars().any(|c| c.is_ascii_digit()) {
                        size = format!("{} {}", prev_word, part);
                    } else {
                        size = part.to_string();
                    }
                } else if p_upper.ends_with("GB") || p_upper.ends_with("MB") || p_upper.ends_with("GIB") || p_upper.ends_with("MIB") {
                    if part.chars().any(|c| c.is_ascii_digit()) {
                        size = part.to_string();
                    }
                }
                prev_word = part;
            }
            if let Some(pos) = line.find('👤') {
                let after = &line[pos + '👤'.len_utf8()..];
                if let Some(num_str) = after.split_whitespace().next() {
                    let digits: String = num_str.chars().filter(|c| c.is_ascii_digit()).collect();
                    seeders_num = digits.parse::<u32>().unwrap_or(0);
                }
            } else if let Some(pos) = line.find('👥') {
                let after = &line[pos + '👥'.len_utf8()..];
                if let Some(num_str) = after.split_whitespace().next() {
                    let digits: String = num_str.chars().filter(|c| c.is_ascii_digit()).collect();
                    seeders_num = digits.parse::<u32>().unwrap_or(0);
                }
            }
        }

        // Detect Torrent Pack (Season Pack / Complete Series / Disc Collection)
        let is_pack = upper.contains("COMPLETE")
            || upper.contains("SEASON")
            || upper.contains("S01-")
            || upper.contains("S01 -")
            || upper.contains("S02-")
            || upper.contains("S03-")
            || upper.contains("PACK")
            || upper.contains("COLLECTION");

        // SURFACE RT STREAM SCORING ALGORITHM:
        let mut score: i32 = (seeders_num.min(500) as i32) * 2;

        // Container scoring based on settings.playback.prefer_mp4
        if prefer_mp4 {
            if is_mp4 {
                score += 10000; // Heavily prioritize MP4 for 100% native Surface RT hardware decoding
            } else if is_aac {
                score += 7500;
            } else {
                score += 5000;
            }

            if is_aac {
                score += 1000;
            } else if is_ac3 || is_eac3 || is_dts {
                score -= 300; // Demote non-native audio formats
            }
        } else {
            // Equal container tier (+5000) so streams rank primarily by resolution and active seeders
            score += 5000;
            if is_aac {
                score += 1500;
            } else if is_ac3 || is_eac3 || is_dts {
                score -= 300;
            }
        }

        // Resolution score based on settings.playback.max_height
        if max_height >= 1080 {
            if is_1080p {
                score += 300;
            } else if is_720p {
                score += 200;
            } else {
                score += 100;
            }
        } else if max_height >= 720 {
            if is_720p {
                score += 300;
            } else {
                score += 100;
            }
        } else {
            score += 300; // 480p / SD
        }

        // Default Audio Language preference bonus (+500)
        let matches_audio_lang = if audio_lang.is_empty() || audio_lang == "en" || audio_lang == "eng" {
            upper.contains("ENG") || upper.contains("ENGLISH") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "es" || audio_lang == "spa" {
            upper.contains("ESP") || upper.contains("SPA") || upper.contains("SPANISH") || upper.contains("CASTELLANO") || upper.contains("LATINO") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "por" || audio_lang == "pt" || audio_lang == "pob" {
            upper.contains("POR") || upper.contains("PT-BR") || upper.contains("PORTUGUESE") || upper.contains("BRAZILIAN") || upper.contains("DUB") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "fre" || audio_lang == "fr" {
            upper.contains("FRE") || upper.contains("FRENCH") || upper.contains("FRANCAIS") || upper.contains("VF") || upper.contains("VFF") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "jpn" || audio_lang == "ja" {
            upper.contains("JPN") || upper.contains("JAPANESE") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "ger" || audio_lang == "de" {
            upper.contains("GER") || upper.contains("GERMAN") || upper.contains("DEUTSCH") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "ita" || audio_lang == "it" {
            upper.contains("ITA") || upper.contains("ITALIAN") || upper.contains("ITALIANO") || upper.contains("MULTI") || upper.contains("DUAL")
        } else if audio_lang == "rus" || audio_lang == "ru" {
            upper.contains("RUS") || upper.contains("RUSSIAN") || upper.contains("MULTI") || upper.contains("DUAL")
        } else {
            let code_up = audio_lang.to_uppercase();
            upper.contains(&code_up) || upper.contains("MULTI") || upper.contains("DUAL")
        };

        if matches_audio_lang {
            score += 500;
        }

        // Torrent pack adjustment: if it's a pack without specific episode file index, demote heavily
        if is_pack && item.file_idx.is_none() {
            score -= 5000;
        }

        // COMPATIBILITY BADGE
        let badge = if is_mp4 {
            "[★ NATIVE MP4]"
        } else if is_aac {
            "[★ MKV H264+AAC]"
        } else if is_eac3 {
            "[MKV H264 EAC3]"
        } else if is_ac3 {
            "[MKV H264 AC3]"
        } else if is_dts {
            "[MKV H264 DTS]"
        } else {
            "[MKV H264]"
        };

        // Clean release name for display
        let display_title = if !release_name.is_empty() {
            release_name.to_string()
        } else {
            format!("{} {}", badge, quality_tag)
        };

        compatible.push(CompatibleStream {
            display_title,
            quality: format!("{} H.264", quality_tag),
            size,
            seeders: seeders_num,
            info_hash,
            file_idx: item.file_idx,
            trackers,
            container: format!("[{}]", container_tag),
            audio_codec: audio_tag.to_string(),
            is_mp4,
            is_pack,
            score,
            raw_title,
        });
    }

    // Sort by calculated score (highest score first)
    compatible.sort_by(|a, b| b.score.cmp(&a.score));

    log_net(&format!(
        "Compatible streams found: {} ({} native MP4/AAC on top).",
        compatible.len(),
        compatible.iter().filter(|s| s.score >= 5000).count()
    ));

    Ok(compatible)
}

#[derive(Deserialize)]
struct ManifestResponse {
    pub id: Option<String>,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub catalogs: Vec<state::ManifestCatalog>,
    #[serde(default)]
    pub resources: Vec<serde_json::Value>,
}

/// Installs or updates an add-on from a manifest URL (supports stremio:// and auto-manifest.json appending)
pub fn install_addon_from_url(input_url: &str) -> Result<state::AddonDescriptor, String> {
    let mut url = input_url.trim().to_string();
    if url.starts_with("stremio://") {
        url = url.replacen("stremio://", "https://", 1);
    }
    if !url.ends_with("/manifest.json") {
        if url.ends_with('/') {
            url.push_str("manifest.json");
        } else {
            url.push_str("/manifest.json");
        }
    }

    log_net(&format!("Fetching add-on manifest from: {}", url));
    let agent = get_shared_agent();
    let resp = agent.get(&url).call().map_err(|e| format!("Network error fetching manifest: {}", e))?;
    let parsed: ManifestResponse = resp.into_json().map_err(|e| format!("Invalid manifest JSON: {}", e))?;

    let id = parsed.id.ok_or_else(|| "Manifest missing 'id' field".to_string())?;
    let name = parsed.name.unwrap_or_else(|| id.clone());
    let version = parsed.version.unwrap_or_else(|| "1.0.0".to_string());

    let descriptor = state::AddonDescriptor {
        transport_url: url,
        manifest: state::Manifest {
            id,
            name,
            version,
            description: parsed.description,
            types: parsed.types,
            catalogs: parsed.catalogs,
            resources: parsed.resources,
        },
        enabled: true,
        priority: 10,
    };

    state::add_or_update_addon(descriptor.clone());
    log_net(&format!("Add-on '{}' installed successfully.", descriptor.manifest.name));
    Ok(descriptor)
}

/// Removes an installed add-on by ID
pub fn remove_addon(id: &str) -> Result<(), String> {
    let mut cfg = state::load_addons_config();
    let initial_len = cfg.addons.len();
    cfg.addons.retain(|a| a.manifest.id != id);
    if cfg.addons.len() < initial_len {
        state::save_addons_config(&cfg);
        log_net(&format!("Add-on '{}' removed.", id));
        Ok(())
    } else {
        Err(format!("Add-on '{}' not found", id))
    }
}

/// Toggles enabled state of an installed add-on
pub fn toggle_addon(id: &str) -> Result<bool, String> {
    let mut cfg = state::load_addons_config();
    if let Some(addon) = cfg.addons.iter_mut().find(|a| a.manifest.id == id) {
        addon.enabled = !addon.enabled;
        let new_state = addon.enabled;
        state::save_addons_config(&cfg);
        log_net(&format!("Add-on '{}' toggled to enabled={}.", id, new_state));
        Ok(new_state)
    } else {
        Err(format!("Add-on '{}' not found", id))
    }
}

/// Moves an add-on's priority up or down
pub fn move_addon_priority(id: &str, move_up: bool) -> Result<(), String> {
    let mut cfg = state::load_addons_config();
    let idx = cfg.addons.iter().position(|a| a.manifest.id == id).ok_or_else(|| format!("Add-on '{}' not found", id))?;
    if move_up && idx > 0 {
        cfg.addons.swap(idx, idx - 1);
        state::save_addons_config(&cfg);
        Ok(())
    } else if !move_up && idx + 1 < cfg.addons.len() {
        cfg.addons.swap(idx, idx + 1);
        state::save_addons_config(&cfg);
        Ok(())
    } else {
        Ok(())
    }
}
