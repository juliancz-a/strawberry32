use super::types::{JellyfinItem, MediaStream};
use crate::addons::MetaItem;
use crate::state::{MetaDetail, MetaVideo, SubtitleTrack};

/// Formats a Jellyfin item ID into our internal `jellyfin:<server_id>:<item_id>` scheme.
pub fn make_jellyfin_id(server_id: &str, item_id: &str) -> String {
    format!("jellyfin:{}:{}", server_id, item_id)
}

/// Parses `jellyfin:<server_id>:<item_id>` into `(server_id, item_id)`.
pub fn parse_jellyfin_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix("jellyfin:")?;
    let colon = rest.find(':')?;
    let server_id = &rest[..colon];
    let item_id = &rest[colon + 1..];
    if server_id.is_empty() || item_id.is_empty() {
        return None;
    }
    Some((server_id, item_id))
}

/// Builds a sized Jellyfin image URL (never requests originals).
pub fn build_image_url(
    server_url: &str,
    item_id: &str,
    image_type: &str,
    max_width: u32,
    token: &str,
) -> String {
    let base = server_url.trim_end_matches('/');
    format!(
        "{}/Items/{}/Images/{}?maxWidth={}&quality=80&api_key={}",
        base, item_id, image_type, max_width, token
    )
}

/// Builds a Jellyfin subtitle download URL.
pub fn build_subtitle_url(
    server_url: &str,
    item_id: &str,
    media_source_id: &str,
    index: i32,
    format: &str,
    token: &str,
) -> String {
    let base = server_url.trim_end_matches('/');
    format!(
        "{}/Videos/{}/{}/Subtitles/{}/Stream.{}?api_key={}",
        base, item_id, media_source_id, index, format, token
    )
}

fn map_item_type(jf_type: &str) -> String {
    match jf_type.to_ascii_lowercase().as_str() {
        "series" => "series".to_string(),
        _ => "movie".to_string(),
    }
}

fn format_runtime(ticks: Option<u64>) -> Option<String> {
    let t = ticks?;
    let mins = t / 600_000_000;
    if mins > 0 {
        Some(format!("{} min", mins))
    } else {
        None
    }
}

/// Maps Jellyfin `/Items` results to the app's `MetaItem` catalog cards.
pub fn map_items_to_meta_items(
    server_id: &str,
    server_url: &str,
    token: &str,
    items: &[JellyfinItem],
    poster_max_width: u32,
) -> Vec<MetaItem> {
    items
        .iter()
        .map(|item| {
            let has_primary = item.image_tags.contains_key("Primary");
            let poster = if has_primary {
                Some(build_image_url(server_url, &item.id, "Primary", poster_max_width, token))
            } else {
                // Still provide Primary URL as fallback if Jellyfin has inherited/parent art
                Some(build_image_url(server_url, &item.id, "Primary", poster_max_width, token))
            };
            let background = if !item.backdrop_image_tags.is_empty() {
                Some(build_image_url(server_url, &item.id, "Backdrop", 640, token))
            } else {
                None
            };
            let year = item.production_year.map(|y| y.to_string());
            MetaItem {
                id: make_jellyfin_id(server_id, &item.id),
                name: item.name.clone(),
                item_type: Some(map_item_type(&item.item_type)),
                year: year.clone(),
                poster,
                background,
                description: item.overview.clone(),
                release_info: year,
                imdb_rating: item.community_rating.map(|r| format!("{:.1}", r)),
                runtime: format_runtime(item.run_time_ticks),
                genres: item.genres.clone(),
            }
        })
        .collect()
}

/// Maps Jellyfin episode items to `MetaVideo`.
pub fn map_episodes_to_meta_videos(
    server_id: &str,
    server_url: &str,
    token: &str,
    episodes: &[JellyfinItem],
) -> Vec<MetaVideo> {
    episodes
        .iter()
        .map(|ep| MetaVideo {
            id: make_jellyfin_id(server_id, &ep.id),
            title: ep.name.clone(),
            season: ep.parent_index_number.unwrap_or(1),
            episode: ep.index_number.unwrap_or(1),
            released: ep.premiere_date.clone(),
            overview: ep.overview.clone(),
            thumbnail: Some(build_image_url(server_url, &ep.id, "Primary", 320, token)),
        })
        .collect()
}

/// Maps a Jellyfin item (and optional series episodes) to `MetaDetail`.
pub fn map_item_to_meta_detail(
    server_id: &str,
    server_url: &str,
    token: &str,
    item: &JellyfinItem,
    episodes: &[JellyfinItem],
) -> MetaDetail {
    let year = item.production_year.map(|y| y.to_string());
    let poster = Some(build_image_url(server_url, &item.id, "Primary", 400, token));
    let background = if !item.backdrop_image_tags.is_empty() {
        Some(build_image_url(server_url, &item.id, "Backdrop", 640, token))
    } else {
        None
    };

    MetaDetail {
        id: make_jellyfin_id(server_id, &item.id),
        name: item.name.clone(),
        item_type: map_item_type(&item.item_type),
        year: year.clone(),
        poster,
        background,
        logo: None,
        description: item.overview.clone(),
        release_info: year,
        imdb_rating: item.community_rating.map(|r| format!("{:.1}", r)),
        runtime: format_runtime(item.run_time_ticks),
        cast: Vec::new(),
        director: Vec::new(),
        genres: item.genres.clone(),
        videos: map_episodes_to_meta_videos(server_id, server_url, token, episodes),
    }
}

/// Extracts text-based subtitle tracks (`subrip`/`srt`, `vtt`, `ass`, `ssa`) from Jellyfin `MediaStreams`.
pub fn map_media_streams_to_subtitles(
    server_url: &str,
    token: &str,
    item_id: &str,
    media_source_id: &str,
    streams: &[MediaStream],
) -> Vec<SubtitleTrack> {
    let mut tracks = Vec::new();
    for s in streams {
        if !s.stream_type.eq_ignore_ascii_case("Subtitle") {
            continue;
        }
        let codec_lower = s.codec.to_ascii_lowercase();
        let format = match codec_lower.as_str() {
            "subrip" | "srt" => "srt",
            "webvtt" | "vtt" => "vtt",
            "ass" | "ssa" => "ass",
            _ => continue, // Skip bitmap subtitles (PGSSUB, DVDSUB) that text renderer cannot draw
        };
        let lang = s
            .language
            .as_deref()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or("und")
            .to_string();
        let label = s
            .display_title
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|t| format!("{} [Jellyfin {}]", t, format.to_uppercase()))
            .unwrap_or_else(|| format!("{} [Jellyfin {}]", lang.to_uppercase(), format.to_uppercase()));
        let url = build_subtitle_url(server_url, item_id, media_source_id, s.index, format, token);
        tracks.push(SubtitleTrack {
            id: format!("jf_sub_{}_{}", item_id, s.index),
            lang,
            label,
            source: "jellyfin".to_string(),
            url,
        });
    }
    tracks
}

