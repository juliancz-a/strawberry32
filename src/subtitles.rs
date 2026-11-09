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

#[derive(Clone, Debug)]
pub struct BundledSubtitle {
    pub file_idx: usize,
    pub filename: String,
    pub lang: String,
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
static SUBTITLE_DELAY_MS: Mutex<i64> = Mutex::new(0);

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
    let mut delay_guard = SUBTITLE_DELAY_MS.lock().unwrap();
    *delay_guard = 0;
}

pub fn get_active_sub_lang() -> String {
    ACTIVE_SUB_LANG.lock().unwrap().clone()
}

pub fn set_subtitle_delay_ms(delay: i64) {
    let mut guard = SUBTITLE_DELAY_MS.lock().unwrap();
    *guard = delay;
}

pub fn get_subtitle_delay_ms() -> i64 {
    *SUBTITLE_DELAY_MS.lock().unwrap()
}

pub fn adjust_subtitle_delay_ms(delta: i64) -> i64 {
    let mut guard = SUBTITLE_DELAY_MS.lock().unwrap();
    *guard = (*guard + delta).clamp(-10_000, 10_000);
    *guard
}

pub fn get_current_subtitle_text(pos_ms: u64) -> Option<String> {
    let guard = ACTIVE_SUBTITLES.lock().unwrap();
    if let Some(entries) = guard.as_ref() {
        let delay = *SUBTITLE_DELAY_MS.lock().unwrap();
        let effective_pos_ms = if delay >= 0 {
            pos_ms.saturating_add(delay as u64)
        } else {
            pos_ms.saturating_sub((-delay) as u64)
        };

        for (i, entry) in entries.iter().enumerate() {
            // Provide a minimum readability hold of 2200ms unless the next cue begins sooner
            let next_start = entries.get(i + 1).map(|e| e.start_ms).unwrap_or(u64::MAX);
            let min_end = (entry.start_ms + 2200).min(next_start.saturating_sub(100));
            let effective_end = entry.end_ms.max(min_end);

            if effective_pos_ms >= entry.start_ms && effective_pos_ms <= effective_end {
                return Some(entry.text.clone());
            }
        }
    }
    None
}

/// Computes OpenSubtitles 64-bit checksum hash over file length + head 64KB + tail 64KB.
pub fn calculate_opensub_hash(file_len: u64, head: &[u8], tail: &[u8]) -> String {
    let mut hash = file_len;

    for chunk in head.chunks(8) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        hash = hash.wrapping_add(u64::from_le_bytes(buf));
    }

    for chunk in tail.chunks(8) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        hash = hash.wrapping_add(u64::from_le_bytes(buf));
    }

    format!("{:016x}", hash)
}

/// Scans torrent files for bundled external subtitle files (.srt, .vtt, .ass, .ssa)
pub fn find_bundled_subtitles(files: &[(usize, String, u64)]) -> Vec<BundledSubtitle> {
    let mut list = Vec::new();
    let sub_exts = [".srt", ".vtt", ".ass", ".ssa"];

    for (idx, filename, _) in files {
        let lower = filename.to_lowercase();
        if sub_exts.iter().any(|ext| lower.ends_with(ext)) {
            // Extract clean language from filename tokens
            let clean_path = lower.replace('\\', "/");
            let file_part = clean_path.split('/').last().unwrap_or(&lower);
            let tokens: Vec<&str> = file_part
                .split(|c: char| !c.is_alphanumeric())
                .filter(|s| !s.is_empty())
                .collect();

            let mut lang = "eng".to_string();
            let mut matched = false;

            for t in &tokens {
                match *t {
                    "es" | "spa" | "spanish" | "esp" | "castellano" => { lang = "spa".to_string(); matched = true; break; }
                    "pb" | "pob" | "ptbr" | "brazilian" => { lang = "pob".to_string(); matched = true; break; }
                    "pt" | "por" | "portuguese" => { lang = "por".to_string(); matched = true; break; }
                    "fr" | "fre" | "fra" | "french" | "francais" => { lang = "fre".to_string(); matched = true; break; }
                    "de" | "ger" | "deu" | "german" | "deutsch" => { lang = "ger".to_string(); matched = true; break; }
                    "it" | "ita" | "italian" | "italiano" => { lang = "ita".to_string(); matched = true; break; }
                    "ru" | "rus" | "russian" => { lang = "rus".to_string(); matched = true; break; }
                    "nl" | "dut" | "nld" | "dutch" => { lang = "dut".to_string(); matched = true; break; }
                    "pl" | "pol" | "polish" => { lang = "pol".to_string(); matched = true; break; }
                    "tr" | "tur" | "turkish" => { lang = "tur".to_string(); matched = true; break; }
                    "ar" | "ara" | "arabic" => { lang = "ara".to_string(); matched = true; break; }
                    "zh" | "chi" | "zho" | "chinese" => { lang = "chi".to_string(); matched = true; break; }
                    "ja" | "jpn" | "japanese" => { lang = "jpn".to_string(); matched = true; break; }
                    "ko" | "kor" | "korean" => { lang = "kor".to_string(); matched = true; break; }
                    "sv" | "swe" | "swedish" => { lang = "swe".to_string(); matched = true; break; }
                    "da" | "dan" | "danish" => { lang = "dan".to_string(); matched = true; break; }
                    "no" | "nor" | "norwegian" => { lang = "nor".to_string(); matched = true; break; }
                    "fi" | "fin" | "finnish" => { lang = "fin".to_string(); matched = true; break; }
                    "el" | "ell" | "greek" => { lang = "ell".to_string(); matched = true; break; }
                    "ro" | "rum" | "ron" | "romanian" => { lang = "rum".to_string(); matched = true; break; }
                    "cs" | "cze" | "ces" | "czech" => { lang = "cze".to_string(); matched = true; break; }
                    "hu" | "hun" | "hungarian" => { lang = "hun".to_string(); matched = true; break; }
                    "uk" | "ukr" | "ukrainian" => { lang = "ukr".to_string(); matched = true; break; }
                    "hi" | "hin" | "hindi" => { lang = "hin".to_string(); matched = true; break; }
                    "en" | "eng" | "english" => { lang = "eng".to_string(); matched = true; break; }
                    _ => {}
                }
            }

            if !matched {
                if file_part.contains("spanish") || file_part.contains("espa") {
                    lang = "spa".to_string();
                }
            }

            log_info(&format!("Found bundled torrent subtitle: {} (lang: {})", filename, lang));
            list.push(BundledSubtitle {
                file_idx: *idx,
                filename: filename.clone(),
                lang,
            });
        }
    }

    list
}

pub fn clean_language_name(code: &str) -> String {
    let lower = code.trim().to_lowercase();
    match lower.as_str() {
        "en" | "eng" | "english" => "English".to_string(),
        "es" | "spa" | "spanish" | "es-es" | "es-419" | "castellano" | "es-la" | "esp" => "Spanish".to_string(),
        "pt" | "por" | "portuguese" | "pt-pt" => "Portuguese".to_string(),
        "pb" | "pob" | "pt-br" | "brazilian" | "portuguese (brazil)" => "Portuguese (BR)".to_string(),
        "fr" | "fre" | "fra" | "french" | "français" => "French".to_string(),
        "de" | "ger" | "deu" | "german" | "deutsch" => "German".to_string(),
        "it" | "ita" | "italian" | "italiano" => "Italian".to_string(),
        "ru" | "rus" | "russian" | "русский" => "Russian".to_string(),
        "nl" | "dut" | "nld" | "dutch" | "nederlands" => "Dutch".to_string(),
        "pl" | "pol" | "polish" | "polski" => "Polish".to_string(),
        "tr" | "tur" | "turkish" | "türkçe" => "Turkish".to_string(),
        "ar" | "ara" | "arabic" | "العربية" => "Arabic".to_string(),
        "zh" | "chi" | "zho" | "chinese" | "mandarin" | "zh-cn" | "zh-tw" => "Chinese".to_string(),
        "ja" | "jpn" | "japanese" | "日本語" => "Japanese".to_string(),
        "ko" | "kor" | "korean" | "한국어" => "Korean".to_string(),
        "sv" | "swe" | "swedish" | "svenska" => "Swedish".to_string(),
        "da" | "dan" | "danish" | "dansk" => "Danish".to_string(),
        "no" | "nor" | "norwegian" | "norsk" => "Norwegian".to_string(),
        "fi" | "fin" | "finnish" | "suomi" => "Finnish".to_string(),
        "el" | "ell" | "greek" | "ελληνικά" => "Greek".to_string(),
        "ro" | "rum" | "ron" | "romanian" | "română" => "Romanian".to_string(),
        "cs" | "cze" | "ces" | "czech" | "čeština" => "Czech".to_string(),
        "hu" | "hun" | "hungarian" | "magyar" => "Hungarian".to_string(),
        "uk" | "ukr" | "ukrainian" | "українська" => "Ukrainian".to_string(),
        "hi" | "hin" | "hindi" | "हिन्दी" => "Hindi".to_string(),
        "id" | "ind" | "indonesian" => "Indonesian".to_string(),
        "vi" | "vie" | "vietnamese" => "Vietnamese".to_string(),
        "th" | "tha" | "thai" => "Thai".to_string(),
        "he" | "heb" | "hebrew" => "Hebrew".to_string(),
        "fa" | "fas" | "per" | "persian" | "farsi" => "Persian".to_string(),
        "tl" | "tgl" | "fil" | "tagalog" | "filipino" => "Tagalog".to_string(),
        "ms" | "msa" | "may" | "malay" => "Malay".to_string(),
        "hr" | "hrv" | "croatian" => "Croatian".to_string(),
        "sr" | "srp" | "serbian" => "Serbian".to_string(),
        "bg" | "bul" | "bulgarian" => "Bulgarian".to_string(),
        "sk" | "slk" | "slo" | "slovak" => "Slovak".to_string(),
        "sl" | "slv" | "slovenian" => "Slovenian".to_string(),
        "et" | "est" | "estonian" => "Estonian".to_string(),
        "lv" | "lav" | "latvian" => "Latvian".to_string(),
        "lt" | "lit" | "lithuanian" => "Lithuanian".to_string(),
        _ => {
            // Never return generic "Subtitle"! Capitalize the raw code or name
            if lower.is_empty() {
                "Unknown".to_string()
            } else if lower.len() <= 3 {
                lower.to_uppercase()
            } else {
                let mut c = lower.chars();
                match c.next() {
                    None => "Unknown".to_string(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            }
        }
    }
}

pub fn get_sub_format_badge(url: &str) -> &'static str {
    let lower = url.to_lowercase();
    if lower.contains(".vtt") {
        "[VTT]"
    } else if lower.contains(".ass") {
        "[ASS]"
    } else if lower.contains(".ssa") {
        "[SSA]"
    } else {
        "[SRT]"
    }
}

/// Fetches available subtitle tracks for a movie or episode from all enabled subtitle addons.
/// Supports frame-accurate lookup via videoHash and videoSize when available.
pub fn fetch_subtitle_tracks(
    item_type: &str,
    imdb_id: &str,
    season: Option<u32>,
    episode: Option<u32>,
    video_hash: Option<&str>,
    video_size: Option<u64>,
) -> Result<Vec<SubtitleTrack>, String> {
    let base_query_id = if item_type == "series" {
        format!("{}:{}:{}", imdb_id, season.unwrap_or(1), episode.unwrap_or(1))
    } else {
        imdb_id.to_string()
    };

    let client = match crate::addons::create_agent() {
        Ok(a) => a,
        Err(e) => {
            log_err(&format!("TLS agent creation failed for subtitles: {}", e));
            return Err(e);
        }
    };

    // Discover all enabled subtitle addons
    let cfg = crate::state::load_addons_config();
    let mut addon_bases = Vec::new();
    for addon in &cfg.addons {
        if !addon.enabled {
            continue;
        }
        let provides_subs = addon.manifest.resources.iter().any(|r| {
            if let Some(s) = r.as_str() {
                s == "subtitles"
            } else if let Some(obj) = r.as_object() {
                obj.get("name").and_then(|v| v.as_str()) == Some("subtitles")
            } else {
                false
            }
        }) || addon.manifest.id.contains("opensubtitles")
           || addon.transport_url.contains("opensubtitles");

        if provides_subs {
            let base = addon.transport_url.trim_end_matches("/manifest.json").trim_end_matches('/');
            if !addon_bases.contains(&base.to_string()) {
                addon_bases.push(base.to_string());
            }
        }
    }

    if addon_bases.is_empty() {
        addon_bases.push("https://opensubtitles-v3.strem.io".to_string());
    }

    let mut raw_tracks = Vec::new();
    let mut seen_urls = std::collections::HashSet::new();

    for base in addon_bases {
        // 1. Frame-accurate videoHash query if available
        if let (Some(hash), Some(size)) = (video_hash, video_size) {
            let hash_url = format!(
                "{}/subtitles/{}/{}/videoHash={}&videoSize={}.json",
                base, item_type, base_query_id, hash, size
            );
            log_info(&format!("Searching frame-accurate subtitles: {}", hash_url));

            if let Ok(resp) = client.get(&hash_url).call() {
                if let Ok(parsed) = resp.into_json::<SubtitleResponse>() {
                    for sub in parsed.subtitles {
                        if let (Some(url), Some(lang)) = (sub.url, sub.lang) {
                            if seen_urls.insert(url.clone()) {
                                let id = sub.id.unwrap_or_else(|| lang.clone());
                                raw_tracks.push((id, lang, url));
                            }
                        }
                    }
                }
            }
        }

        // 2. Standard ID query
        let std_url = format!("{}/subtitles/{}/{}.json", base, item_type, base_query_id);
        log_info(&format!("Searching standard metadata subtitles: {}", std_url));

        if let Ok(resp) = client.get(&std_url).call() {
            if let Ok(parsed) = resp.into_json::<SubtitleResponse>() {
                for sub in parsed.subtitles {
                    if let (Some(url), Some(lang)) = (sub.url, sub.lang) {
                        if seen_urls.insert(url.clone()) {
                            let id = sub.id.unwrap_or_else(|| lang.clone());
                            raw_tracks.push((id, lang, url));
                        }
                    }
                }
            }
        }
    }

    // Format clean language labels: "Spanish #1 [SRT]", "English #2 [VTT]", etc.
    let mut lang_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut tracks = Vec::new();

    for (id, lang_code, url) in raw_tracks {
        let clean_name = clean_language_name(&lang_code);
        let count = lang_counts.entry(clean_name.clone()).or_insert(0);
        *count += 1;
        let badge = get_sub_format_badge(&url);
        let formatted_label = format!("{} #{} {}", clean_name, count, badge);
        tracks.push(SubtitleTrack {
            id,
            lang: clean_name,
            label: formatted_label,
            source: "addon".to_string(),
            url,
        });
    }

    log_info(&format!("Discovered {} deduplicated addon subtitle tracks.", tracks.len()));
    Ok(tracks)
}

/// Downloads and parses a subtitle file from URL (auto-detecting SRT, WebVTT, ASS/SSA)
pub fn download_and_parse_subtitle(url: &str) -> Result<Vec<SubtitleEntry>, String> {
    log_info(&format!("Downloading subtitle: {}", url));

    let client = crate::addons::create_agent()?;
    let resp = client.get(url).call().map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;

    // Try decoding UTF-8 first; if invalid, fall back to Windows-1252 / ISO-8859-1 for Spanish/European accents
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            let (cow, _, _) = encoding_rs::WINDOWS_1252.decode(&bytes);
            cow.into_owned()
        }
    };

    parse_subtitle_content(&text)
}

/// Auto-detects subtitle format (SRT, WebVTT, ASS/SSA) and parses into standardized SubtitleEntry cues.
pub fn parse_subtitle_content(content: &str) -> Result<Vec<SubtitleEntry>, String> {
    let trimmed = content.trim_start();
    if trimmed.starts_with("WEBVTT") {
        parse_vtt(content)
    } else if content.contains("[Script Info]") || content.contains("Dialogue:") {
        parse_ass(content)
    } else {
        parse_srt(content)
    }
}

// =========================================================================
// SRT Parser
// =========================================================================
fn parse_srt_timestamp(ts_str: &str) -> Option<u64> {
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

    Ok(entries)
}

// =========================================================================
// WebVTT Parser
// =========================================================================
pub fn parse_vtt(content: &str) -> Result<Vec<SubtitleEntry>, String> {
    let mut entries = Vec::new();
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let blocks: Vec<&str> = normalized.split("\n\n").collect();

    for block in blocks {
        let lines: Vec<&str> = block.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
        let Some(ts_idx) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };

        let ts_parts: Vec<&str> = lines[ts_idx].split("-->").collect();
        if ts_parts.len() != 2 {
            continue;
        }

        let start_ms = match parse_vtt_timestamp(ts_parts[0]) {
            Some(t) => t,
            None => continue,
        };
        let end_ms = match parse_vtt_timestamp(ts_parts[1]) {
            Some(t) => t,
            None => continue,
        };

        let text = lines[ts_idx + 1..].join("\n");
        let cleaned = clean_sub_text(&text);
        if !cleaned.is_empty() {
            entries.push(SubtitleEntry {
                start_ms,
                end_ms,
                text: cleaned,
            });
        }
    }

    Ok(entries)
}

fn parse_vtt_timestamp(raw: &str) -> Option<u64> {
    // VTT timestamp: HH:MM:SS.mmm or MM:SS.mmm
    let token = raw.trim().split_whitespace().next()?;
    let parts: Vec<&str> = token.split(':').collect();

    if parts.len() == 3 {
        let h: u64 = parts[0].parse().ok()?;
        let m: u64 = parts[1].parse().ok()?;
        let sec_parts: Vec<&str> = parts[2].split('.').collect();
        let s: u64 = sec_parts[0].parse().ok()?;
        let ms: u64 = sec_parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        Some(h * 3_600_000 + m * 60_000 + s * 1_000 + ms)
    } else if parts.len() == 2 {
        let m: u64 = parts[0].parse().ok()?;
        let sec_parts: Vec<&str> = parts[1].split('.').collect();
        let s: u64 = sec_parts[0].parse().ok()?;
        let ms: u64 = sec_parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        Some(m * 60_000 + s * 1_000 + ms)
    } else {
        None
    }
}

// =========================================================================
// ASS / SSA Parser (with {\...} override tag stripping & \N newline handling)
// =========================================================================
pub fn parse_ass(content: &str) -> Result<Vec<SubtitleEntry>, String> {
    let mut entries = Vec::new();
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");

    for line in normalized.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("Dialogue:") {
            continue;
        }

        let payload = trimmed.strip_prefix("Dialogue:").unwrap().trim();
        // ASS Dialogue has at least 9 comma-separated fields:
        // Marked/Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
        let parts: Vec<&str> = payload.splitn(10, ',').collect();
        if parts.len() < 10 {
            continue;
        }

        let start_ms = match parse_ass_timestamp(parts[1]) {
            Some(t) => t,
            None => continue,
        };
        let end_ms = match parse_ass_timestamp(parts[2]) {
            Some(t) => t,
            None => continue,
        };

        let raw_text = parts[9];
        let cleaned = clean_ass_text(raw_text);

        if !cleaned.is_empty() {
            entries.push(SubtitleEntry {
                start_ms,
                end_ms,
                text: cleaned,
            });
        }
    }

    Ok(entries)
}

fn parse_ass_timestamp(raw: &str) -> Option<u64> {
    // Format: H:MM:SS.cc (centiseconds)
    let parts: Vec<&str> = raw.trim().split(':').collect();
    if parts.len() != 3 {
        return None;
    }

    let h: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let sec_parts: Vec<&str> = parts[2].split('.').collect();
    let s: u64 = sec_parts[0].parse().ok()?;
    let cs: u64 = sec_parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);

    Some(h * 3_600_000 + m * 60_000 + s * 1_000 + cs * 10)
}

fn clean_ass_text(raw: &str) -> String {
    // Strip ASS override tags like {\pos(100,200)}, {\c&HFFFFFF&}, {\b1}, {\i1}, etc.
    let mut out = String::with_capacity(raw.len());
    let mut inside_tag = false;

    let text_with_newlines = raw.replace(r"\N", "\n").replace(r"\n", "\n").replace(r"\h", " ");

    for c in text_with_newlines.chars() {
        if c == '{' {
            inside_tag = true;
        } else if c == '}' {
            inside_tag = false;
        } else if !inside_tag {
            out.push(c);
        }
    }

    out.trim().to_string()
}
