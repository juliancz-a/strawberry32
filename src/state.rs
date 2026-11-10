use crate::logger::{get_temp_dir, log_info};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static STORAGE_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Returns the root storage directory for Strawberry32.
/// Tries %LOCALAPPDATA%\Strawberry32 first, verifies writability,
/// and falls back to %TEMP%\Strawberry32 if needed.
pub fn get_app_storage_root() -> PathBuf {
    STORAGE_ROOT
        .get_or_init(|| {
            let candidate = if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
                PathBuf::from(local_appdata).join("Strawberry32")
            } else {
                get_temp_dir().join("Strawberry32")
            };

            // Probe writability
            let is_writable = (|| {
                fs::create_dir_all(&candidate).ok()?;
                let probe_file = candidate.join(".write_probe");
                fs::write(&probe_file, b"ok").ok()?;
                fs::remove_file(&probe_file).ok()?;
                Some(())
            })()
            .is_some();

            let chosen = if is_writable {
                log_info(&format!("Storage root verified in %LOCALAPPDATA%: {}", candidate.display()));
                candidate
            } else {
                let fallback = get_temp_dir().join("Strawberry32");
                let _ = fs::create_dir_all(&fallback);
                log_info(&format!("Storage root falling back to %TEMP%: {}", fallback.display()));
                fallback
            };

            // Initialize canonical folder layout
            let _ = fs::create_dir_all(chosen.join("data").join("profiles").join("default"));
            let _ = fs::create_dir_all(chosen.join("cache").join("http"));
            let _ = fs::create_dir_all(chosen.join("cache").join("images"));
            let _ = fs::create_dir_all(chosen.join("torrents"));
            let _ = fs::create_dir_all(chosen.join("logs"));

            chosen
        })
        .clone()
}

/// Returns the persistent data directory: <storage_root>\data\profiles\default\
pub fn get_data_dir() -> PathBuf {
    let dir = get_app_storage_root()
        .join("data")
        .join("profiles")
        .join("default");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Returns the logs directory: <storage_root>\logs\
pub fn get_logs_dir() -> PathBuf {
    let dir = get_app_storage_root().join("logs");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Returns the poster thumbnail cache directory: <storage_root>\cache\images\
#[allow(dead_code)]
pub fn get_posters_dir() -> PathBuf {
    let dir = get_app_storage_root().join("cache").join("images");
    let _ = fs::create_dir_all(&dir);
    dir
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestCatalog {
    #[serde(rename = "type")]
    pub catalog_type: String,
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub catalogs: Vec<ManifestCatalog>,
    #[serde(default)]
    pub resources: Vec<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddonDescriptor {
    pub transport_url: String,
    pub manifest: Manifest,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AddonsConfig {
    pub addons: Vec<AddonDescriptor>,
}

pub fn deserialize_string_or_number<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrNumberVisitor;
    impl<'de> serde::de::Visitor<'de> for StringOrNumberVisitor {
        type Value = Option<String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string or number")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
        fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D2>(self, deserializer: D2) -> Result<Self::Value, D2::Error>
        where
            D2: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(StringOrNumberVisitor)
        }
    }
    deserializer.deserialize_any(StringOrNumberVisitor)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaVideo {
    pub id: String,
    #[serde(default, alias = "name")]
    pub title: String,
    #[serde(default)]
    pub season: u32,
    #[serde(default)]
    pub episode: u32,
    #[serde(default)]
    pub released: Option<String>,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaDetail {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default, deserialize_with = "deserialize_string_or_number")]
    pub year: Option<String>,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "releaseInfo", deserialize_with = "deserialize_string_or_number")]
    pub release_info: Option<String>,
    #[serde(default, rename = "imdbRating", deserialize_with = "deserialize_string_or_number")]
    pub imdb_rating: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_number")]
    pub runtime: Option<String>,
    #[serde(default)]
    pub cast: Vec<String>,
    #[serde(default)]
    pub director: Vec<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub videos: Vec<MetaVideo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub id: String,
    pub lang: String,
    pub label: String,
    pub source: String,
    pub url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchHistoryItem {
    pub meta_id: String,
    pub item_type: String,
    pub title: String,
    pub poster: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub pos_sec: u64,
    pub dur_sec: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct WatchHistory {
    pub items: Vec<WatchHistoryItem>,
}

static WATCH_HISTORY: Mutex<Option<WatchHistory>> = Mutex::new(None);
static ADDONS_CONFIG: Mutex<Option<AddonsConfig>> = Mutex::new(None);

pub fn load_watch_history() -> WatchHistory {
    let mut guard = WATCH_HISTORY.lock().unwrap();
    if let Some(h) = guard.as_ref() {
        return h.clone();
    }

    let file_path = get_data_dir().join("watch_history.json");
    let history: WatchHistory = if file_path.is_file() {
        match fs::read_to_string(&file_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => WatchHistory::default(),
        }
    } else {
        WatchHistory::default()
    };

    *guard = Some(history.clone());
    history
}

pub fn save_watch_history(history: &WatchHistory) {
    let file_path = get_data_dir().join("watch_history.json");
    if let Ok(json) = serde_json::to_string_pretty(history) {
        let _ = fs::write(file_path, json);
    }
    let mut guard = WATCH_HISTORY.lock().unwrap();
    *guard = Some(history.clone());
}

pub fn record_watch_progress(
    meta_id: &str,
    item_type: &str,
    title: &str,
    poster: Option<&str>,
    season: Option<u32>,
    episode: Option<u32>,
    pos_sec: u64,
    dur_sec: u64,
) {
    let mut history = load_watch_history();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Remove existing entry for same meta_id / season / episode
    history.items.retain(|item| {
        !(item.meta_id == meta_id && item.season == season && item.episode == episode)
    });

    history.items.insert(
        0,
        WatchHistoryItem {
            meta_id: meta_id.to_string(),
            item_type: item_type.to_string(),
            title: title.to_string(),
            poster: poster.map(|s| s.to_string()),
            season,
            episode,
            pos_sec,
            dur_sec,
            updated_at: now,
        },
    );

    // Keep top 50 entries
    if history.items.len() > 50 {
        history.items.truncate(50);
    }

    save_watch_history(&history);
}

pub fn clear_watch_history() {
    let history = WatchHistory::default();
    save_watch_history(&history);
}

pub fn get_default_addons_config() -> AddonsConfig {
    let cinemeta = AddonDescriptor {
        transport_url: "https://v3-cinemeta.strem.io/manifest.json".to_string(),
        manifest: Manifest {
            id: "com.linvo.cinemeta".to_string(),
            name: "Cinemeta".to_string(),
            version: "3.0.12".to_string(),
            description: Some("Official movies and series catalog & metadata".to_string()),
            types: vec!["movie".to_string(), "series".to_string()],
            catalogs: vec![
                ManifestCatalog {
                    catalog_type: "movie".to_string(),
                    id: "top".to_string(),
                    name: Some("Popular Movies".to_string()),
                },
                ManifestCatalog {
                    catalog_type: "series".to_string(),
                    id: "top".to_string(),
                    name: Some("Popular Series".to_string()),
                },
            ],
            resources: vec![serde_json::json!("catalog"), serde_json::json!("meta")],
        },
        enabled: true,
        priority: 0,
    };

    let opensubtitles = AddonDescriptor {
        transport_url: "https://opensubtitles-v3.strem.io/manifest.json".to_string(),
        manifest: Manifest {
            id: "org.stremio.opensubtitlesv3".to_string(),
            name: "OpenSubtitles v3".to_string(),
            version: "1.0.0".to_string(),
            description: Some("Official OpenSubtitles v3 subtitles provider".to_string()),
            types: vec!["movie".to_string(), "series".to_string()],
            catalogs: vec![],
            resources: vec![serde_json::json!("subtitles")],
        },
        enabled: true,
        priority: 1,
    };

    AddonsConfig {
        addons: vec![cinemeta, opensubtitles],
    }
}

pub fn load_addons_config() -> AddonsConfig {
    let mut guard = ADDONS_CONFIG.lock().unwrap();
    if let Some(cfg) = guard.as_ref() {
        return cfg.clone();
    }

    let file_path = get_data_dir().join("addons.json");
    let config: AddonsConfig = if file_path.is_file() {
        match fs::read_to_string(&file_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| get_default_addons_config()),
            Err(_) => get_default_addons_config(),
        }
    } else {
        let def = get_default_addons_config();
        save_addons_config(&def);
        def
    };

    *guard = Some(config.clone());
    config
}

// --- App Settings Schema (UI_UX_SPEC B.4.7 / CACHE_AND_ADDONS § 5) ---

fn default_start_page() -> String { "home".to_string() }
fn default_card_size() -> String { "m".to_string() }
fn default_lang_en() -> String { "en".to_string() }
fn default_max_height() -> u32 { 720 }
fn default_seek_step() -> u32 { 15 }
fn default_volume() -> f32 { 1.0 }
fn default_sub_size_pct() -> i32 { 100 }
fn default_max_bytes() -> u64 { 2 * 1024 * 1024 * 1024 } // 2 GB
fn default_retention_back() -> u64 { 64 * 1024 * 1024 }  // 64 MB
fn default_lookahead() -> u64 { 48 * 1024 * 1024 }       // 48 MB
fn default_image_cache_bytes() -> u64 { 48 * 1024 * 1024 } // 48 MB
fn default_max_peers() -> u32 { 40 }
fn default_max_connections() -> u32 { 40 }
fn default_version() -> u32 { 1 }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UiSettings {
    #[serde(default = "default_start_page")]
    pub start_page: String,
    #[serde(default)]
    pub reduce_animations: bool,
    #[serde(default = "default_card_size")]
    pub card_size: String,
    #[serde(default = "default_true")]
    pub hero_autorotate: bool,
    #[serde(default = "default_lang_en")]
    pub language: String,
    #[serde(default = "default_true")]
    pub start_maximized: bool,
    #[serde(default)]
    pub start_fullscreen: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            start_page: default_start_page(),
            reduce_animations: false,
            card_size: default_card_size(),
            hero_autorotate: true,
            language: default_lang_en(),
            start_maximized: true,
            start_fullscreen: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaybackSettings {
    #[serde(default = "default_max_height")]
    pub max_height: u32,
    #[serde(default = "default_true")]
    pub prefer_mp4: bool,
    #[serde(default = "default_lang_en")]
    pub sub_lang: String,
    #[serde(default = "default_lang_en")]
    pub audio_lang: String,
    #[serde(default = "default_true")]
    pub autoplay_next: bool,
    #[serde(default)]
    pub vlc_path: Option<String>,
    #[serde(default = "default_true")]
    pub auto_vlc_fallback: bool,
    #[serde(default = "default_seek_step")]
    pub seek_step_s: u32,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default = "default_sub_size_pct")]
    pub subtitle_size_pct: i32,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            max_height: default_max_height(),
            prefer_mp4: true,
            sub_lang: default_lang_en(),
            audio_lang: default_lang_en(),
            autoplay_next: true,
            vlc_path: None,
            auto_vlc_fallback: true,
            seek_step_s: default_seek_step(),
            volume: default_volume(),
            subtitle_size_pct: default_sub_size_pct(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheSettings {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default)]
    pub keep_after_playback: bool,
    #[serde(default = "default_retention_back")]
    pub retention_back_bytes: u64,
    #[serde(default = "default_lookahead")]
    pub lookahead_bytes: u64,
    #[serde(default = "default_image_cache_bytes")]
    pub image_cache_bytes: u64,
    #[serde(default = "default_max_peers")]
    pub max_peers: u32,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default)]
    pub download_limit_kbps: u32,
}

impl Default for CacheSettings {
    fn default() -> Self {
        Self {
            root: None,
            max_bytes: default_max_bytes(),
            keep_after_playback: false,
            retention_back_bytes: default_retention_back(),
            lookahead_bytes: default_lookahead(),
            image_cache_bytes: default_image_cache_bytes(),
            max_peers: default_max_peers(),
            max_connections: default_max_connections(),
            download_limit_kbps: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub ui: UiSettings,
    #[serde(default)]
    pub playback: PlaybackSettings,
    #[serde(default)]
    pub cache: CacheSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: default_version(),
            ui: UiSettings::default(),
            playback: PlaybackSettings::default(),
            cache: CacheSettings::default(),
        }
    }
}

static APP_SETTINGS: Mutex<Option<AppSettings>> = Mutex::new(None);

pub fn load_settings() -> AppSettings {
    let mut guard = APP_SETTINGS.lock().unwrap();
    if let Some(s) = guard.as_ref() {
        return s.clone();
    }

    let file_path = get_data_dir().join("settings.json");
    let settings: AppSettings = if file_path.is_file() {
        match fs::read_to_string(&file_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => AppSettings::default(),
        }
    } else {
        let def = AppSettings::default();
        let _ = save_settings_internal(&def);
        def
    };

    *guard = Some(settings.clone());
    settings
}

fn save_settings_internal(settings: &AppSettings) -> Result<(), String> {
    let file_path = get_data_dir().join("settings.json");
    let tmp_path = file_path.with_extension("tmp");
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    fs::write(&tmp_path, json).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(&file_path);
    fs::rename(&tmp_path, &file_path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn save_settings(settings: &AppSettings) {
    let _ = save_settings_internal(settings);
    let mut guard = APP_SETTINGS.lock().unwrap();
    *guard = Some(settings.clone());
}

fn get_dir_size_recursive(path: &Path) -> u64 {
    let mut total: u64 = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += get_dir_size_recursive(&p);
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

pub fn get_torrent_cache_size() -> u64 {
    let dir = crate::engine::get_cache_dir();
    get_dir_size_recursive(&dir)
}

pub fn clear_torrent_cache() -> std::io::Result<()> {
    let _ = crate::engine::clear_cache();
    let dir = crate::engine::get_cache_dir();
    if dir.is_dir() {
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
    }
    let legacy = get_app_storage_root().join("torrents");
    if legacy.is_dir() {
        let _ = fs::remove_dir_all(&legacy);
        let _ = fs::create_dir_all(&legacy);
    }
    Ok(())
}

pub fn get_image_cache_size() -> u64 {
    let dir = get_app_storage_root().join("cache").join("images");
    get_dir_size_recursive(&dir)
}

pub fn clear_image_cache() -> std::io::Result<()> {
    let dir = get_app_storage_root().join("cache").join("images");
    if dir.is_dir() {
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
    }
    Ok(())
}

pub fn save_addons_config(config: &AddonsConfig) {
    let file_path = get_data_dir().join("addons.json");
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = fs::write(file_path, json);
    }
    let mut guard = ADDONS_CONFIG.lock().unwrap();
    *guard = Some(config.clone());
}

pub fn add_or_update_addon(descriptor: AddonDescriptor) {
    let mut cfg = load_addons_config();
    cfg.addons
        .retain(|a| a.manifest.id != descriptor.manifest.id && a.transport_url != descriptor.transport_url);
    cfg.addons.push(descriptor);
    save_addons_config(&cfg);
}
