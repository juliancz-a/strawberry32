use crate::logger::{log_err, log_info};
use crate::state::SubtitleTrack;
use serde::Deserialize;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct SubtitleEntry {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Deserialize, Debug)]
struct SubtitleResponse {
    #[serde(default)]
    pub subtitles: Vec<RawSubItem>,
}

#[derive(Deserialize, Debug)]
struct RawSubItem {
    pub id: Option<String>,
    pub lang: Option<String>,
    pub url: Option<String>,
}

static ACTIVE_SUBTITLES: Mutex<Option<Vec<SubtitleEntry>>> = Mutex::new(None);
static ACTIVE_SUB_LANG: Mutex<String> = Mutex::new(String::new());

pub fn set_active_subtitles(entries: Vec<SubtitleEntry>, lang: &str) {
    let mut guard = ACTIVE_SUBTITLES.lock().unwrap();
    *guard = Some(entries);
    let mut lang_guard = ACTIVE_SUB_LANG.lock().unwrap();
    *lang_guard = lang.to_string();
}

pub fn clear_active_subtitles() {
    let mut guard = ACTIVE_SUBTITLES.lock().unwrap();
    *guard = None;
    let mut lang_guard = ACTIVE_SUB_LANG.lock().unwrap();
    lang_guard.clear();
}

pub fn get_active_sub_lang() -> String {
    ACTIVE_SUB_LANG.lock().unwrap().clone()
}

pub fn get_current_subtitle_text(pos_ms: u64) -> Option<String> {
    let guard = ACTIVE_SUBTITLES.lock().unwrap();
    if let Some(entries) = guard.as_ref() {
        for entry in entries {
            if pos_ms >= entry.start_ms && pos_ms <= entry.end_ms {
                return Some(entry.text.clone());
            }
        }
    }
    None
}

/// Fetches available subtitle tracks for a movie or episode from OpenSubtitles v3 Stremio addon
pub fn fetch_subtitle_tracks(
    item_type: &str,
    imdb_id: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> Result<Vec<SubtitleTrack>, String> {
    let query_id = if item_type == "series" {
        format!("{}:{}:{}", imdb_id, season.unwrap_or(1), episode.unwrap_or(1))
    } else {
        imdb_id.to_string()
    };

    let url = format!(
        "https://opensubtitles-v3.strem.io/subtitles/{}/{}.json",
        item_type, query_id
    );

    log_info(&format!("Searching subtitles in OpenSubtitles: {}", url));

    let client = match crate::addons::create_agent() {
        Ok(a) => a,
        Err(e) => {
            log_err(&format!("TLS agent creation failed for subtitles: {}", e));
            return Err(e);
        }
    };

    let resp = match client.get(&url).call() {
        Ok(r) => r,
        Err(e) => {
            log_err(&format!("Failed searching subtitles at {}: {}", url, e));
            return Err(e.to_string());
        }
    };

    let parsed: SubtitleResponse = match resp.into_json() {
        Ok(p) => p,
        Err(e) => {
            log_err(&format!("Error parsing subtitle JSON: {}", e));
            return Err(e.to_string());
        }
    };

    let mut tracks = Vec::new();
    for sub in parsed.subtitles {
        if let (Some(url), Some(lang)) = (sub.url, sub.lang) {
            let id = sub.id.unwrap_or_else(|| lang.clone());
            tracks.push(SubtitleTrack { id, lang, url });
        }
    }

    log_info(&format!("Found {} subtitle tracks.", tracks.len()));
    Ok(tracks)
}

/// Downloads and parses an SRT subtitle file from URL with encoding handling (UTF-8 / Latin-1)
pub fn download_and_parse_srt(url: &str) -> Result<Vec<SubtitleEntry>, String> {
    log_info(&format!("Downloading SRT subtitle: {}", url));

    let client = crate::addons::create_agent()?;

    let resp = client.get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;

    // Try decoding UTF-8 first; if invalid, fall back to Windows-1252 / ISO-8859-1 for Spanish accents
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(&bytes);
            cow.into_owned()
        }
    };

    parse_srt(&text)
}

fn parse_srt_timestamp(ts_str: &str) -> Option<u64> {
    // Format: 00:01:23,456 or 00:01:23.456
    let ts_str = ts_str.trim().replace('.', ",");
    let parts: Vec<&str> = ts_str.split(':').collect();
    if parts.len() != 3 {
        return None;
    }

    let hours: u64 = parts[0].trim().parse().ok()?;
    let minutes: u64 = parts[1].trim().parse().ok()?;

    let sec_parts: Vec<&str> = parts[2].split(',').collect();
    let seconds: u64 = sec_parts[0].trim().parse().ok()?;
    let millis: u64 = if sec_parts.len() > 1 {
        let mut m_str = sec_parts[1].trim().to_string();
        while m_str.len() < 3 {
            m_str.push('0');
        }
        if m_str.len() > 3 {
            m_str.truncate(3);
        }
        m_str.parse().unwrap_or(0)
    } else {
        0
    };

    Some(hours * 3_600_000 + minutes * 60_000 + seconds * 1_000 + millis)
}

fn clean_sub_text(raw: &str) -> String {
    // Strip simple HTML tags like <i>, </i>, <font ...>, </font>, <b>, </b>
    let mut out = String::with_capacity(raw.len());
    let mut inside_tag = false;
    for c in raw.chars() {
        if c == '<' {
            inside_tag = true;
        } else if c == '>' {
            inside_tag = false;
        } else if !inside_tag {
            out.push(c);
        }
    }
    out.trim().to_string()
}

pub fn parse_srt(content: &str) -> Result<Vec<SubtitleEntry>, String> {
    let mut entries = Vec::new();
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let blocks: Vec<&str> = normalized.split("\n\n").collect();

    for block in blocks {
        let lines: Vec<&str> = block.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        if lines.len() < 2 {
            continue;
        }

        // Line 0 is usually index, Line 1 is timestamps: "00:00:10,000 --> 00:00:15,000"
        let (ts_line_idx, text_start_idx) = if lines[0].contains("-->") {
            (0, 1)
        } else if lines.len() >= 2 && lines[1].contains("-->") {
            (1, 2)
        } else {
            continue;
        };

        let ts_parts: Vec<&str> = lines[ts_line_idx].split("-->").collect();
        if ts_parts.len() != 2 {
            continue;
        }

        let start_ms = match parse_srt_timestamp(ts_parts[0]) {
            Some(t) => t,
            None => continue,
        };
        let end_ms = match parse_srt_timestamp(ts_parts[1]) {
            Some(t) => t,
            None => continue,
        };

        let text_lines: Vec<&str> = lines[text_start_idx..].to_vec();
        let full_text = text_lines.join("\n");
        let cleaned = clean_sub_text(&full_text);

        if !cleaned.is_empty() {
            entries.push(SubtitleEntry {
                start_ms,
                end_ms,
                text: cleaned,
            });
        }
    }

    log_info(&format!("SRT subtitle parsed with {} cues.", entries.len()));
    Ok(entries)
}

