pub mod client;
pub mod config;
pub mod mapping;
pub mod playback;
pub mod types;

use crate::addons::{CompatibleStream, MetaItem};
use crate::logger::{log_err, log_info};
use crate::state::{load_settings, MetaDetail, SubtitleTrack};
use playback::{build_playback_url, choose_playback, DeviceCaps};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

#[derive(Default)]
struct JellyfinAvailabilityIndex {
    /// Lowercased IMDb ID (e.g. "tt1234567") -> "jellyfin:<server_id>:<item_id>"
    by_imdb: HashMap<String, String>,
    /// (normalized_title, 4-digit year, "movie"|"series") -> "jellyfin:<server_id>:<item_id>"
    by_title_year: HashMap<(String, String, String), String>,
    /// (normalized_title, "movie"|"series") -> "jellyfin:<server_id>:<item_id>"
    by_title: HashMap<(String, String), String>,
    /// "jellyfin:<server_id>:<item_id>" -> lowercased IMDb ID
    jf_to_imdb: HashMap<String, String>,
}

static JELLYFIN_INDEX: OnceLock<Mutex<JellyfinAvailabilityIndex>> = OnceLock::new();

fn index_guard() -> std::sync::MutexGuard<'static, JellyfinAvailabilityIndex> {
    JELLYFIN_INDEX
        .get_or_init(|| Mutex::new(JellyfinAvailabilityIndex::default()))
        .lock()
        .unwrap()
}

pub fn normalize_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn extract_year_4(s: Option<&str>) -> String {
    s.unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(4)
        .collect()
}

fn norm_item_type(t: Option<&str>) -> String {
    match t.unwrap_or("movie").to_ascii_lowercase().as_str() {
        "series" | "tvshows" => "series".to_string(),
        _ => "movie".to_string(),
    }
}

fn index_jellyfin_items(server_id: &str, items: &[types::JellyfinItem]) {
    let mut idx = index_guard();
    for item in items {
        let jf_id = mapping::make_jellyfin_id(server_id, &item.id);
        let item_type = norm_item_type(Some(&item.item_type));
        let title_norm = normalize_title(&item.name);
        let year_str = item.production_year.map(|y| y.to_string()).unwrap_or_default();

        if let Some(imdb) = item
            .provider_ids
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Imdb"))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
        {
            idx.by_imdb.insert(imdb.clone(), jf_id.clone());
            idx.jf_to_imdb.insert(jf_id.clone(), imdb);
        }

        if !title_norm.is_empty() {
            if !year_str.is_empty() {
                idx.by_title_year
                    .insert((title_norm.clone(), year_str, item_type.clone()), jf_id.clone());
            }
            idx.by_title.entry((title_norm, item_type)).or_insert(jf_id);
        }
    }
}

fn lookup_in_index(
    idx: &JellyfinAvailabilityIndex,
    id: &str,
    name: &str,
    year: Option<&str>,
    item_type: &str,
) -> Option<String> {
    if id.starts_with("jellyfin:") {
        return Some(id.to_string());
    }
    let id_lower = id.trim().to_ascii_lowercase();
    if id_lower.starts_with("tt") {
        if let Some(jf) = idx.by_imdb.get(&id_lower) {
            return Some(jf.clone());
        }
    }
    let norm_t = normalize_title(name);
    let ty = norm_item_type(Some(item_type));
    let y4 = extract_year_4(year);
    if !norm_t.is_empty() {
        if !y4.is_empty() {
            if let Some(jf) = idx.by_title_year.get(&(norm_t.clone(), y4, ty.clone())) {
                return Some(jf.clone());
            }
        }
        if let Some(jf) = idx.by_title.get(&(norm_t, ty)) {
            return Some(jf.clone());
        }
    }
    None
}

/// Finds a matching `jellyfin:<server>:<item>` ID for a Cinemeta/external title, querying
/// connected Jellyfin servers on demand if not yet cached in the index.
pub fn find_jellyfin_id_for_meta(
    id: &str,
    name: &str,
    year: Option<&str>,
    item_type: &str,
) -> Option<String> {
    if id.starts_with("jellyfin:") {
        return Some(id.to_string());
    }
    {
        let idx = index_guard();
        if let Some(found) = lookup_in_index(&idx, id, name, year, item_type) {
            return Some(found);
        }
    }

    let servers = config::load_jellyfin_servers();
    if servers.is_empty() || name.trim().is_empty() {
        return None;
    }

    for server in servers {
        if let Ok(res) = client::fetch_items(&server, None, 0, 15, Some(name.trim()), None, None, None) {
            index_jellyfin_items(&server.id, &res.items);
        }
    }

    let idx = index_guard();
    lookup_in_index(&idx, id, name, year, item_type)
}

/// Deduplicates Cinemeta and Jellyfin search results (preferring Cinemeta metadata when matched
/// by IMDb ID or normalized title + year + type). When `jellyfin_only` is true, returns only
/// items available on connected Jellyfin servers.
pub fn deduplicate_search_results(
    cm_results: Vec<MetaItem>,
    jf_results: Vec<MetaItem>,
    jellyfin_only: bool,
) -> Vec<MetaItem> {
    let idx = index_guard();

    let mut jf_imdb_map: HashMap<String, String> = HashMap::new();
    let mut jf_title_year_map: HashMap<(String, String, String), String> = HashMap::new();
    let mut jf_title_no_year_map: HashMap<(String, String), String> = HashMap::new();

    for jf in &jf_results {
        let imdb_opt = idx.jf_to_imdb.get(&jf.id).cloned();
        if let Some(ref imdb) = imdb_opt {
            jf_imdb_map.insert(imdb.clone(), jf.id.clone());
        }
        let nt = normalize_title(&jf.name);
        let ty = norm_item_type(jf.item_type.as_deref());
        let y4 = extract_year_4(jf.year.as_deref().or(jf.release_info.as_deref()));
        if !nt.is_empty() {
            if !y4.is_empty() {
                jf_title_year_map.insert((nt.clone(), y4, ty.clone()), jf.id.clone());
            } else if imdb_opt.is_none() {
                // Only allow title-only fallback for Jellyfin items that lack both year and IMDb ID
                jf_title_no_year_map.entry((nt, ty)).or_insert_with(|| jf.id.clone());
            }
        }
    }

    let mut out = Vec::new();
    let mut matched_jf_ids: HashSet<String> = HashSet::new();
    let mut seen_cm_ids: HashSet<String> = HashSet::new();

    for cm in cm_results {
        if !seen_cm_ids.insert(cm.id.clone()) {
            continue;
        }
        let cm_ty = norm_item_type(cm.item_type.as_deref());
        let cm_nt = normalize_title(&cm.name);
        let cm_y4 = extract_year_4(cm.year.as_deref().or(cm.release_info.as_deref()));
        let cm_imdb = cm.id.trim().to_ascii_lowercase();

        let matched_jf = if cm_imdb.starts_with("tt") {
            jf_imdb_map.get(&cm_imdb).filter(|id| !matched_jf_ids.contains(*id)).cloned()
        } else {
            None
        }
        .or_else(|| {
            if !cm_nt.is_empty() && !cm_y4.is_empty() {
                jf_title_year_map
                    .get(&(cm_nt.clone(), cm_y4.clone(), cm_ty.clone()))
                    .filter(|id| !matched_jf_ids.contains(*id))
                    .cloned()
            } else {
                None
            }
        })
        .or_else(|| {
            if !cm_nt.is_empty() && cm_y4.is_empty() {
                jf_title_no_year_map
                    .get(&(cm_nt, cm_ty))
                    .filter(|id| !matched_jf_ids.contains(*id))
                    .cloned()
            } else {
                None
            }
        });

        if let Some(jf_id) = matched_jf {
            matched_jf_ids.insert(jf_id);
            out.push(cm);
        } else if !jellyfin_only {
            out.push(cm);
        }
    }

    for jf in jf_results {
        if !matched_jf_ids.contains(&jf.id) {
            out.push(jf);
        }
    }

    out
}

/// Fetches up to 40 items per Jellyfin library view across all connected servers for Home rows.
pub fn fetch_home_rows() -> Vec<(String, Vec<MetaItem>)> {
    let servers = config::load_jellyfin_servers();
    let mut rows = Vec::new();
    for server in servers {
        match client::fetch_user_views(&server) {
            Ok(views) => {
                for view in views {
                    let ct = view.collection_type.as_deref().unwrap_or("");
                    if !matches!(ct, "" | "movies" | "tvshows" | "homevideos" | "mixed") {
                        continue;
                    }
                    match client::fetch_items(
                        &server,
                        Some(&view.id),
                        0,
                        40,
                        None,
                        None,
                        Some("DateCreated"),
                        Some("Descending"),
                    ) {
                        Ok(res) if !res.items.is_empty() => {
                            index_jellyfin_items(&server.id, &res.items);
                            let mapped = mapping::map_items_to_meta_items(
                                &server.id,
                                &server.server_url,
                                &server.access_token,
                                &res.items,
                                280,
                            );
                            let title = format!("{} — {}", view.name, server.name);
                            rows.push((title, mapped));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log_err(&format!("Jellyfin library '{}' fetch failed: {}", view.name, e));
                        }
                    }
                }
            }
            Err(e) => {
                log_err(&format!("Jellyfin views fetch failed for '{}': {}", server.name, e));
            }
        }
    }
    rows
}

/// Searches all connected Jellyfin servers, mapping Discover/Search filters to Jellyfin `/Items`.
pub fn search_all_servers(
    query: &str,
    genre: Option<&str>,
    sort: Option<&str>,
    asc: bool,
) -> Vec<MetaItem> {
    let servers = config::load_jellyfin_servers();
    let mut all = Vec::new();
    let sort_by = match sort {
        Some("rating") => "CommunityRating",
        Some("year") => "ProductionYear",
        Some("title") => "SortName",
        _ => "SortName",
    };
    let sort_order = if asc { "Ascending" } else { "Descending" };

    for server in servers {
        let q_opt = if query.trim().is_empty() { None } else { Some(query.trim()) };
        match client::fetch_items(
            &server,
            None,
            0,
            40,
            q_opt,
            genre,
            Some(sort_by),
            Some(sort_order),
        ) {
            Ok(res) => {
                index_jellyfin_items(&server.id, &res.items);
                let mapped = mapping::map_items_to_meta_items(
                    &server.id,
                    &server.server_url,
                    &server.access_token,
                    &res.items,
                    280,
                );
                all.extend(mapped);
            }
            Err(e) => {
                log_err(&format!("Jellyfin search failed on '{}': {}", server.name, e));
            }
        }
    }
    all
}

/// Fetches full metadata details (including series seasons & episodes) for a `jellyfin:<server>:<item>` ID.
pub fn fetch_meta_detail(_item_type: &str, jellyfin_id: &str) -> Result<MetaDetail, String> {
    let (server_id, item_id) = mapping::parse_jellyfin_id(jellyfin_id)
        .ok_or_else(|| "Invalid Jellyfin item ID".to_string())?;
    let server = config::get_jellyfin_server(server_id)
        .ok_or_else(|| "Connected Jellyfin server not found".to_string())?;

    let item = client::fetch_item_detail(&server, item_id)?;
    let episodes = if item.item_type.eq_ignore_ascii_case("Series") {
        client::fetch_episodes(&server, item_id, None).unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(mapping::map_item_to_meta_detail(
        &server.id,
        &server.server_url,
        &server.access_token,
        &item,
        &episodes,
    ))
}

/// Builds playable `CompatibleStream` entries for a `jellyfin:<server>:<item>` ID (movie or episode),
/// using `choose_playback` to decide between Direct Play and Server Transcode.
pub fn fetch_streams(
    item_type: &str,
    jellyfin_id: &str,
    season: u32,
    episode: u32,
) -> Result<Vec<CompatibleStream>, String> {
    let (server_id, item_id) = mapping::parse_jellyfin_id(jellyfin_id)
        .ok_or_else(|| "Invalid Jellyfin item ID".to_string())?;
    let server = config::get_jellyfin_server(server_id)
        .ok_or_else(|| "Connected Jellyfin server not found".to_string())?;

    let target_item = if item_type == "series" {
        let eps = client::fetch_episodes(&server, item_id, None)?;
        eps.into_iter()
            .find(|e| e.parent_index_number.unwrap_or(1) == season && e.index_number.unwrap_or(1) == episode)
            .ok_or_else(|| format!("Episode S{}E{} not found on Jellyfin", season, episode))?
    } else {
        client::fetch_item_detail(&server, item_id)?
    };

    let settings = load_settings();
    let caps = DeviceCaps::from_max_height(settings.playback.max_height);
    let mut streams = Vec::new();

    for ms in &target_item.media_sources {
        let decision = choose_playback(ms, &caps);
        let url = build_playback_url(
            &server.server_url,
            &target_item.id,
            &ms.id,
            &decision,
            &server.access_token,
            None,
        );

        let mode_label = decision.mode_label().to_string();
        let reason = decision.reason_summary();

        let video = ms.media_streams.iter().find(|s| s.stream_type.eq_ignore_ascii_case("Video"));
        let audio = ms.media_streams.iter().find(|s| s.stream_type.eq_ignore_ascii_case("Audio"));

        let res_str = video
            .and_then(|v| v.height.map(|h| format!("{}p", h)))
            .unwrap_or_else(|| "HD".to_string());
        let v_codec = video.map(|v| v.codec.to_uppercase()).unwrap_or_else(|| "H264".to_string());
        let a_codec = audio.map(|a| a.codec.to_uppercase()).unwrap_or_else(|| "AAC".to_string());
        let container_disp = ms
            .container
            .as_deref()
            .unwrap_or("mp4")
            .to_uppercase();
        let size_str = ms
            .size
            .map(|b| format!("{:.2} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)))
            .unwrap_or_else(|| "Stream".to_string());

        let is_direct = matches!(decision, playback::PlaybackDecision::Direct { .. });
        let display_title = format!(
            "[{}] {} • {}",
            mode_label,
            ms.name.as_deref().unwrap_or(&target_item.name),
            reason
        );

        streams.push(CompatibleStream {
            display_title,
            quality: format!("{} {}", res_str, v_codec),
            size: size_str,
            seeders: 0,
            info_hash: String::new(),
            file_idx: None,
            trackers: Vec::new(),
            container: format!("[{}]", container_disp),
            audio_codec: a_codec,
            is_mp4: is_direct,
            is_pack: false,
            score: if is_direct { 20000 } else { 15000 },
            raw_title: reason.clone(),
            stream_url: Some(url),
            playback_mode: Some(mode_label),
            playback_reason: Some(reason),
        });
    }

    log_info(&format!(
        "Jellyfin returned {} playable source(s) for '{}'",
        streams.len(),
        target_item.name
    ));
    Ok(streams)
}

/// Fetches text subtitle tracks from Jellyfin `MediaStreams` for the given `jellyfin:` ID.
pub fn fetch_subtitles_for_item(
    jellyfin_id: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> Vec<SubtitleTrack> {
    let Some((server_id, item_id)) = mapping::parse_jellyfin_id(jellyfin_id) else {
        return Vec::new();
    };
    let Some(server) = config::get_jellyfin_server(server_id) else {
        return Vec::new();
    };

    let target = if let (Some(s), Some(e)) = (season, episode) {
        client::fetch_episodes(&server, item_id, None)
            .ok()
            .and_then(|eps| {
                eps.into_iter()
                    .find(|ep| ep.parent_index_number.unwrap_or(1) == s && ep.index_number.unwrap_or(1) == e)
            })
    } else {
        client::fetch_item_detail(&server, item_id).ok()
    };

    let Some(item) = target else {
        return Vec::new();
    };
    let Some(ms) = item.media_sources.first() else {
        return Vec::new();
    };
    mapping::map_media_streams_to_subtitles(
        &server.server_url,
        &server.access_token,
        &item.id,
        &ms.id,
        &ms.media_streams,
    )
}

