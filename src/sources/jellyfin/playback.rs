use super::types::MediaSource;

/// Hardware decode capabilities for the target device (Surface RT Tegra 3 / Windows RT 8.1).
#[derive(Clone, Debug)]
pub struct DeviceCaps {
    pub max_width: u32,
    pub max_height: u32,
    pub max_video_bit_depth: u32,
    pub max_h264_level: i32,
    pub video_bitrate: u32,
    pub audio_bitrate: u32,
    pub allow_ac3: bool,
}

impl Default for DeviceCaps {
    fn default() -> Self {
        Self {
            max_width: 1280,
            max_height: 720,
            max_video_bit_depth: 8,
            max_h264_level: 41, // Level 4.1
            video_bitrate: 4_000_000,
            audio_bitrate: 128_000,
            allow_ac3: false,
        }
    }
}

impl DeviceCaps {
    pub fn from_max_height(max_height: u32) -> Self {
        let mut caps = Self::default();
        caps.max_height = max_height.max(360);
        caps.max_width = match max_height {
            h if h >= 1080 => 1920,
            h if h >= 720 => 1280,
            _ => 854,
        };
        caps
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscodeParams {
    pub container: String,
    pub video_codec: String,
    pub audio_codec: String,
    pub max_width: u32,
    pub max_height: u32,
    pub video_bitrate: u32,
    pub audio_bitrate: u32,
    pub max_audio_channels: u32,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackDecision {
    Direct { reason: String },
    Transcode(TranscodeParams),
}

impl PlaybackDecision {
    pub fn mode_label(&self) -> &'static str {
        match self {
            PlaybackDecision::Direct { .. } => "Direct",
            PlaybackDecision::Transcode(_) => "Server transcode",
        }
    }

    pub fn reason_summary(&self) -> String {
        match self {
            PlaybackDecision::Direct { reason } => reason.clone(),
            PlaybackDecision::Transcode(p) => p.reasons.join("; "),
        }
    }
}

fn normalize_h264_level(lvl: f64) -> i32 {
    // Jellyfin reports level either as 40/41 (4.0/4.1 * 10) or 4.0/4.1
    if lvl > 10.0 {
        lvl.round() as i32
    } else {
        (lvl * 10.0).round() as i32
    }
}

/// Pure playback decision for a Jellyfin `MediaSource` against `DeviceCaps`.
pub fn choose_playback(media_source: &MediaSource, caps: &DeviceCaps) -> PlaybackDecision {
    let mut reasons = Vec::new();

    // 1. Container check: MFPlay native direct play supports MP4 / MOV
    let container = media_source
        .container
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let container_ok = container.split(',').any(|c| matches!(c.trim(), "mp4" | "mov" | "m4v"));
    if !container_ok {
        let c_disp = if container.is_empty() { "unknown" } else { &container };
        reasons.push(format!("Container '{}' requires MP4 transcode", c_disp));
    }

    // 2. Video stream check
    let video = media_source
        .media_streams
        .iter()
        .find(|s| s.stream_type.eq_ignore_ascii_case("Video"));
    if let Some(v) = video {
        let codec = v.codec.to_ascii_lowercase();
        if codec != "h264" && codec != "avc" && codec != "avc1" {
            reasons.push(format!("Video codec '{}' not supported natively (need H.264)", v.codec));
        }
        if let Some(bd) = v.bit_depth {
            if bd > caps.max_video_bit_depth {
                reasons.push(format!("{}-bit video exceeds {}-bit limit", bd, caps.max_video_bit_depth));
            }
        }
        if let Some(lvl) = v.level {
            let norm = normalize_h264_level(lvl);
            if norm > caps.max_h264_level {
                reasons.push(format!("H.264 level {} exceeds 4.1", norm));
            }
        }
        if let Some(w) = v.width {
            if w > caps.max_width {
                reasons.push(format!("Width {}px exceeds {}px cap", w, caps.max_width));
            }
        }
        if let Some(h) = v.height {
            if h > caps.max_height {
                reasons.push(format!("Height {}px exceeds {}px cap", h, caps.max_height));
            }
        }
    } else {
        reasons.push("Missing video stream metadata".to_string());
    }

    // 3. Audio stream check (require at least one compatible audio stream if audio streams exist)
    let audio_streams: Vec<_> = media_source
        .media_streams
        .iter()
        .filter(|s| s.stream_type.eq_ignore_ascii_case("Audio"))
        .collect();
    if !audio_streams.is_empty() {
        let any_audio_ok = audio_streams.iter().any(|a| {
            let ac = a.codec.to_ascii_lowercase();
            matches!(ac.as_str(), "aac" | "mp3" | "mp4a")
                || (caps.allow_ac3 && ac == "ac3")
        });
        if !any_audio_ok {
            let first_codec = audio_streams[0].codec.as_str();
            reasons.push(format!("Audio codec '{}' requires AAC transcode", first_codec));
        }
    }

    if reasons.is_empty() {
        PlaybackDecision::Direct {
            reason: "MP4 H.264/AAC compatible with Tegra 3 DXVA2".to_string(),
        }
    } else {
        PlaybackDecision::Transcode(TranscodeParams {
            container: "mp4".to_string(),
            video_codec: "h264".to_string(),
            audio_codec: "aac".to_string(),
            max_width: caps.max_width,
            max_height: caps.max_height,
            video_bitrate: caps.video_bitrate,
            audio_bitrate: caps.audio_bitrate,
            max_audio_channels: 2,
            reasons,
        })
    }
}

/// Builds the Jellyfin 12.4.0 stream URL for either Direct Play or Server Transcode.
pub fn build_playback_url(
    server_url: &str,
    item_id: &str,
    media_source_id: &str,
    decision: &PlaybackDecision,
    token: &str,
    start_time_ticks: Option<u64>,
) -> String {
    let base = server_url.trim_end_matches('/');
    match decision {
        PlaybackDecision::Direct { .. } => {
            format!(
                "{}/Videos/{}/stream?static=true&MediaSourceId={}&api_key={}",
                base, item_id, media_source_id, token
            )
        }
        PlaybackDecision::Transcode(p) => {
            let mut url = format!(
                "{}/Videos/{}/stream.{}?MediaSourceId={}&VideoCodec={}&AudioCodec={}&MaxWidth={}&MaxHeight={}&VideoBitRate={}&AudioBitRate={}&TranscodingMaxAudioChannels={}&MaxVideoBitDepth=8&Profile=high&Level=41&api_key={}",
                base,
                item_id,
                p.container,
                media_source_id,
                p.video_codec,
                p.audio_codec,
                p.max_width,
                p.max_height,
                p.video_bitrate,
                p.audio_bitrate,
                p.max_audio_channels,
                token
            );
            if let Some(ticks) = start_time_ticks {
                if ticks > 0 {
                    url.push_str(&format!("&StartTimeTicks={}", ticks));
                }
            }
            url
        }
    }
}

