use crate::logger::{log_err, log_info, log_net, log_ureq_err};
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

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct StreamBehaviorHints {
    #[serde(default)]
    pub not_web_ready: bool,
    #[serde(default)]
    pub binge_group: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default, rename = "videoSize")]
    pub video_size: Option<u64>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RawStremioStream {
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "infoHash")]
    pub info_hash: Option<String>,
    #[serde(rename = "fileIdx")]
    pub file_idx: Option<usize>,
    pub sources: Option<Vec<String>>,
    pub url: Option<String>,
    #[serde(rename = "behaviorHints")]
    pub behavior_hints: Option<StreamBehaviorHints>,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default)]
pub struct StremioStreamResponse {
    #[serde(default)]
    pub streams: Vec<RawStremioStream>,
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
    pub stream_url: Option<String>,
    pub playback_mode: Option<String>,
    pub playback_reason: Option<String>,
}

/// Creates a ureq client configured with dual-stack DNS resolution, short connect timeout,
/// overall request timeout, and Windows RT Schannel TLS connector.
pub fn create_agent() -> Result<ureq::Agent, String> {
    let tls_connector = native_tls::TlsConnector::new()
        .map_err(|e| format!("Schannel TlsConnector init error: {}", e))?;

    let agent = ureq::AgentBuilder::new()
        .resolver(crate::net::DualStackResolver)
        .tls_connector(std::sync::Arc::new(tls_connector))
        .timeout_connect(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
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

/// Builds a Stremio catalog URL with optional `extra` parameters in path form (`/catalog/{type}/{id}/k=v&...json`).
pub fn build_catalog_url(
    base_url: &str,
    catalog_type: &str,
    catalog_id: &str,
    extras: &[(&str, &str)],
) -> String {
    let base = base_url
        .trim()
        .trim_end_matches("/manifest.json")
        .trim_end_matches('/');
    if extras.is_empty() {
        format!("{}/catalog/{}/{}.json", base, catalog_type, catalog_id)
    } else {
        let extra_str: Vec<String> = extras
            .iter()
            .map(|(k, v)| format!("{}={}", k, percent_encode(v)))
            .collect();
        format!(
            "{}/catalog/{}/{}/{}.json",
            base,
            catalog_type,
            catalog_id,
            extra_str.join("&")
        )
    }
}

/// Checks whether a manifest catalog declares support for a given `extra` parameter name.
pub fn catalog_supports_extra(catalog: &state::ManifestCatalog, extra_name: &str) -> bool {
    catalog.extra.iter().any(|e| e.name == extra_name)
}

/// Fetches a catalog from a specific Stremio add-on, honoring `skip` and `search` extras.
pub fn fetch_addon_catalog(
    addon: &state::AddonDescriptor,
    catalog_type: &str,
    catalog_id: &str,
    skip: Option<u32>,
    search: Option<&str>,
) -> Result<Vec<MetaItem>, String> {
    let mut extras_owned: Vec<(&str, String)> = Vec::new();
    if let Some(s) = skip {
        if s > 0 {
            extras_owned.push(("skip", s.to_string()));
        }
    }
    if let Some(q) = search {
        let q_trim = q.trim();
        if !q_trim.is_empty() {
            extras_owned.push(("search", q_trim.to_string()));
        }
    }
    let extras_ref: Vec<(&str, &str)> = extras_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let url = build_catalog_url(&addon.transport_url, catalog_type, catalog_id, &extras_ref);
    log_net(&format!(
        "Fetching catalog from add-on '{}': {}",
        addon.manifest.name,
        crate::redact::redact_url(&url)
    ));
    let agent = get_shared_agent();
    let resp = agent.get(&url).call().map_err(|e| {
        let ce = crate::net::classify_ureq_error(&e);
        log_err(&format!("Add-on catalog error ({}): {}", addon.manifest.name, ce.detail));
        ce.user_message
    })?;
    let bytes = crate::net::read_body_capped(resp.into_reader(), 4 * 1024 * 1024)
        .map_err(|e| e.user_message)?;
    let parsed: CinemetaCatalogResponse = serde_json::from_slice(&bytes)
        .map_err(|e| format!("Invalid catalog JSON from '{}': {}", addon.manifest.name, e))?;
    Ok(parsed.metas)
}

/// Fetches full metadata details including series episode list, routing by id prefix
pub fn fetch_meta_detail(item_type: &str, id: &str) -> Result<MetaDetail, String> {
    if id.starts_with("jellyfin:") {
        return crate::sources::jellyfin::fetch_meta_detail(item_type, id);
    }

    let cfg = state::load_addons_config();
    let mut meta_addons: Vec<&state::AddonDescriptor> = cfg
        .addons
        .iter()
        .filter(|a| a.enabled && addon_supports_resource(&a.manifest, "meta", item_type, Some(id)))
        .collect();
    meta_addons.sort_by_key(|a| a.priority);

    let agent = get_shared_agent();
    for addon in meta_addons {
        let base_url = addon
            .transport_url
            .trim_end_matches("/manifest.json")
            .trim_end_matches('/');
        let url = format!("{}/meta/{}/{}.json", base_url, item_type, percent_encode(id));
        log_net(&format!(
            "Fetching metadata details from '{}': {}",
            addon.manifest.name,
            crate::redact::redact_url(&url)
        ));
        match agent.get(&url).call() {
            Ok(resp) => {
                if let Ok(bytes) = crate::net::read_body_capped(resp.into_reader(), 4 * 1024 * 1024) {
                    if let Ok(parsed) = serde_json::from_slice::<CinemetaMetaResponse>(&bytes) {
                        return Ok(parsed.meta);
                    }
                }
            }
            Err(e) => {
                log_ureq_err(&format!("Meta fetch failed for '{}'", addon.manifest.name), &e);
            }
        }
    }

    let url = format!("https://v3-cinemeta.strem.io/meta/{}/{}.json", item_type, id);
    log_net(&format!("Fetching metadata details: {}", url));
    let resp = agent.get(&url).call().map_err(|e| crate::net::classify_ureq_error(&e).user_message)?;
    let parsed: CinemetaMetaResponse = resp.into_json().map_err(|e| e.to_string())?;
    Ok(parsed.meta)
}

/// Checks whether a manifest declares support for `resource` ("catalog", "meta", "stream", "subtitles")
/// matching `item_type` and optional `item_id` prefix.
/// Uses resource-level `types` / `idPrefixes` when present, otherwise falls back to manifest-level ones.
pub fn addon_supports_resource(
    manifest: &state::Manifest,
    resource: &str,
    item_type: &str,
    item_id: Option<&str>,
) -> bool {
    for r in &manifest.resources {
        if let Some(s) = r.as_str() {
            if s != resource {
                continue;
            }
            let type_ok = manifest.types.is_empty() || manifest.types.iter().any(|t| t == item_type);
            if !type_ok {
                continue;
            }
            if let Some(id) = item_id {
                if !manifest.id_prefixes.is_empty()
                    && !manifest.id_prefixes.iter().any(|p| id.starts_with(p))
                {
                    continue;
                }
            }
            return true;
        } else if let Some(obj) = r.as_object() {
            if obj.get("name").and_then(|v| v.as_str()) != Some(resource) {
                continue;
            }
            let res_types: Vec<&str> = obj
                .get("types")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|t| t.as_str()).collect())
                .unwrap_or_default();
            let type_ok = if !res_types.is_empty() {
                res_types.iter().any(|t| *t == item_type)
            } else {
                manifest.types.is_empty() || manifest.types.iter().any(|t| t == item_type)
            };
            if !type_ok {
                continue;
            }
            if let Some(id) = item_id {
                let res_prefixes: Vec<&str> = obj
                    .get("idPrefixes")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|p| p.as_str()).collect())
                    .unwrap_or_default();
                if !res_prefixes.is_empty() {
                    if !res_prefixes.iter().any(|p| id.starts_with(p)) {
                        continue;
                    }
                } else if !manifest.id_prefixes.is_empty()
                    && !manifest.id_prefixes.iter().any(|p| id.starts_with(p))
                {
                    continue;
                }
            }
            return true;
        }
    }
    false
}

/// Checks whether an installed add-on is enabled and declares support for stream resources for `item_type`.
fn addon_supports_stream(addon: &state::AddonDescriptor, item_type: &str, item_id: &str) -> bool {
    addon.enabled && addon_supports_resource(&addon.manifest, "stream", item_type, Some(item_id))
}

/// Fetches, scores and sorts streams from all installed & enabled stream add-ons for Surface RT hardware (Tegra 3 DXVA2)
pub fn fetch_streams(
    item_type: &str,
    imdb_id: &str,
    season: u32,
    episode: u32,
) -> Result<Vec<CompatibleStream>, String> {
    if imdb_id.starts_with("jellyfin:") {
        return crate::sources::jellyfin::fetch_streams(item_type, imdb_id, season, episode);
    }

    let cfg = state::load_addons_config();
    let mut stream_addons: Vec<&state::AddonDescriptor> = cfg
        .addons
        .iter()
        .filter(|a| addon_supports_stream(a, item_type, imdb_id))
        .collect();

    if stream_addons.is_empty() {
        log_info("No enabled stream add-ons installed. Returning empty stream list.");
        return Ok(Vec::new());
    }

    stream_addons.sort_by_key(|a| a.priority);

    let settings = crate::state::load_settings();
    let max_height = settings.playback.max_height;
    let prefer_mp4 = settings.playback.prefer_mp4;
    let audio_lang = settings.playback.audio_lang.trim().to_lowercase();

    let mut compatible = Vec::new();

    for addon in stream_addons {
        let base_url = addon
            .transport_url
            .trim_end_matches("/manifest.json")
            .trim_end_matches('/');

        let stream_url = if item_type == "series" {
            format!("{}/stream/series/{}:{}:{}.json", base_url, percent_encode(imdb_id), season, episode)
        } else {
            format!("{}/stream/movie/{}.json", base_url, percent_encode(imdb_id))
        };

        log_net(&format!(
            "Querying streams from add-on '{}': {}",
            addon.manifest.name,
            crate::redact::redact_url(&stream_url)
        ));

        let agent = match create_agent() {
            Ok(a) => a,
            Err(e) => {
                log_err(&e);
                continue;
            }
        };

        let resp = match agent.get(&stream_url).call() {
            Ok(r) => r,
            Err(e) => {
                log_ureq_err(
                    &format!("Failed to query streams from add-on '{}'", addon.manifest.name),
                    &e,
                );
                continue;
            }
        };

        let parsed: StremioStreamResponse = match resp.into_json() {
            Ok(p) => p,
            Err(e) => {
                log_err(&format!(
                    "Error parsing stream JSON from add-on '{}': {}",
                    addon.manifest.name, e
                ));
                continue;
            }
        };

        log_net(&format!(
            "Add-on '{}' reported {} total streams before filtering.",
            addon.manifest.name,
            parsed.streams.len()
        ));

        for item in parsed.streams {
            if let Some(direct_url) = item.url.as_ref().filter(|u| {
                let lower = u.to_ascii_lowercase();
                lower.starts_with("http://") || lower.starts_with("https://")
            }) {
                let raw_title = item
                    .title
                    .clone()
                    .or_else(|| item.name.clone())
                    .or_else(|| item.description.clone())
                    .unwrap_or_else(|| format!("{} Direct Stream", addon.manifest.name));
                let display_title = raw_title
                    .lines()
                    .next()
                    .unwrap_or("Direct Stream")
                    .trim()
                    .to_string();
                let size_str = item
                    .behavior_hints
                    .as_ref()
                    .and_then(|h| h.video_size)
                    .map(|b| format!("{:.2} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)))
                    .unwrap_or_else(|| "HTTP".to_string());
                compatible.push(CompatibleStream {
                    display_title,
                    quality: item.name.clone().unwrap_or_else(|| "Direct".to_string()),
                    size: size_str,
                    seeders: 0,
                    info_hash: String::new(),
                    file_idx: None,
                    trackers: Vec::new(),
                    container: "[HTTP]".to_string(),
                    audio_codec: "Stream".to_string(),
                    is_mp4: true,
                    is_pack: false,
                    score: 12000,
                    raw_title,
                    stream_url: Some(direct_url.clone()),
                    playback_mode: Some("Direct".to_string()),
                    playback_reason: Some(format!("HTTP stream from {}", addon.manifest.name)),
                });
                continue;
            }

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

            let mut raw_title = item.title.or(item.name).or(item.description).unwrap_or_default();
            if let Some(hints) = item.behavior_hints {
                if let Some(fname) = hints.filename {
                    if !raw_title.contains(&fname) {
                        raw_title = format!("{}\n{}", fname, raw_title);
                    }
                }
            }
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
                stream_url: None,
                playback_mode: None,
                playback_reason: None,
            });
        }
    }

    // Sort by calculated score (highest score first)
    compatible.sort_by(|a, b| b.score.cmp(&a.score));

    log_net(&format!(
        "Compatible streams found across all add-ons: {} ({} native MP4/AAC on top).",
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
    #[serde(default, rename = "idPrefixes")]
    pub id_prefixes: Vec<String>,
    #[serde(default)]
    pub catalogs: Vec<state::ManifestCatalog>,
    #[serde(default)]
    pub resources: Vec<serde_json::Value>,
}

/// Parses and validates a Stremio add-on manifest JSON payload.
/// Validates required fields (`id`, `name`, `version`, `resources`, `types`) and tolerates unknown fields.
pub fn parse_manifest_bytes(bytes: &[u8]) -> Result<state::Manifest, crate::net::ClassifiedError> {
    let parsed: ManifestResponse = serde_json::from_slice(bytes).map_err(|e| crate::net::ClassifiedError {
        kind: crate::net::NetErrorKind::InvalidJson,
        user_message: "Invalid JSON response from add-on URL.".to_string(),
        detail: crate::redact::redact_secrets(&e.to_string()),
    })?;

    let mut missing = Vec::new();
    let id_ok = parsed.id.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    let name_ok = parsed.name.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    let ver_ok = parsed.version.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false);
    if !id_ok {
        missing.push("id");
    }
    if !name_ok {
        missing.push("name");
    }
    if !ver_ok {
        missing.push("version");
    }
    if parsed.resources.is_empty() {
        missing.push("resources");
    }
    if parsed.types.is_empty() {
        missing.push("types");
    }
    if !missing.is_empty() {
        let msg = format!(
            "Invalid add-on manifest: missing required field(s): {}",
            missing.join(", ")
        );
        return Err(crate::net::ClassifiedError {
            kind: crate::net::NetErrorKind::InvalidManifest,
            user_message: msg.clone(),
            detail: msg,
        });
    }

    Ok(state::Manifest {
        id: parsed.id.unwrap(),
        name: parsed.name.unwrap(),
        version: parsed.version.unwrap(),
        description: parsed.description,
        types: parsed.types,
        id_prefixes: parsed.id_prefixes,
        catalogs: parsed.catalogs,
        resources: parsed.resources,
    })
}

/// Normalizes an input add-on URL (handling `stremio://` and `/manifest.json`),
/// enforces transport policy, and returns `(normalized_url, optional_loopback_warning)`.
pub fn normalize_addon_url(input_url: &str) -> Result<(String, Option<String>), crate::net::ClassifiedError> {
    let mut url = input_url.trim().to_string();
    if url.is_empty() {
        return Err(crate::net::ClassifiedError {
            kind: crate::net::NetErrorKind::Other,
            user_message: "Please enter an add-on manifest URL.".to_string(),
            detail: "Empty URL".to_string(),
        });
    }
    if url.starts_with("stremio://") {
        let rest = &url["stremio://".len()..];
        let host = crate::redact::extract_host(rest);
        let scheme = if crate::redact::is_private_host(&host) {
            "http://"
        } else {
            "https://"
        };
        url = format!("{}{}", scheme, rest);
    }
    if !url.ends_with("/manifest.json") {
        if url.ends_with('/') {
            url.push_str("manifest.json");
        } else {
            url.push_str("/manifest.json");
        }
    }

    crate::net::validate_transport_policy(&url)?;

    let host = crate::redact::extract_host(&url);
    let warning = if crate::redact::is_loopback_host(&host) {
        Some("localhost only works on the same computer as the server; use its LAN IP on this tablet.".to_string())
    } else {
        None
    };

    Ok((url, warning))
}

/// Installs or updates an add-on from a manifest URL, returning the descriptor and an optional non-blocking warning.
pub fn install_addon_from_url_with_warning(
    input_url: &str,
) -> Result<(state::AddonDescriptor, Option<String>), String> {
    let (url, warning) = normalize_addon_url(input_url).map_err(|e| {
        log_err(&format!("Add-on URL rejected: {}", e.detail));
        e.user_message
    })?;

    let redacted = crate::redact::redact_url(&url);
    log_net(&format!("Fetching add-on manifest from: {}", redacted));

    let agent = get_shared_agent();
    let resp = agent.get(&url).call().map_err(|e| {
        let ce = crate::net::classify_ureq_error(&e);
        log_err(&format!(
            "Failed fetching manifest from {}: {} ({})",
            redacted, ce.user_message, ce.detail
        ));
        ce.user_message
    })?;

    let bytes = crate::net::read_body_capped(resp.into_reader(), 2 * 1024 * 1024).map_err(|e| {
        log_err(&format!("Manifest size/read error from {}: {}", redacted, e.detail));
        e.user_message
    })?;

    let manifest = parse_manifest_bytes(&bytes).map_err(|e| {
        log_err(&format!("Manifest parse error from {}: {}", redacted, e.detail));
        e.user_message
    })?;

    let descriptor = state::AddonDescriptor {
        transport_url: url,
        manifest,
        enabled: true,
        priority: 10,
    };

    state::add_or_update_addon(descriptor.clone());
    log_net(&format!("Add-on '{}' installed successfully.", descriptor.manifest.name));
    Ok((descriptor, warning))
}

/// Installs or updates an add-on from a manifest URL (supports stremio:// and auto-manifest.json appending)
pub fn install_addon_from_url(input_url: &str) -> Result<state::AddonDescriptor, String> {
    install_addon_from_url_with_warning(input_url).map(|(d, _)| d)
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
