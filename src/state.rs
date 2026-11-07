use crate::logger::{get_temp_dir, log_info};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddonDescriptor {
    pub transport_url: String,
    pub manifest: Manifest,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AddonsConfig {
    pub addons: Vec<AddonDescriptor>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaVideo {
    pub id: String,
    pub title: String,
    pub season: u32,
    pub episode: u32,
    #[serde(default)]
    pub released: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetaDetail {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub poster: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub videos: Vec<MetaVideo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub id: String,
    pub lang: String,
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

pub fn load_addons_config() -> AddonsConfig {
    let mut guard = ADDONS_CONFIG.lock().unwrap();
    if let Some(cfg) = guard.as_ref() {
        return cfg.clone();
    }

    let file_path = get_data_dir().join("addons.json");
    let config: AddonsConfig = if file_path.is_file() {
        match fs::read_to_string(&file_path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => AddonsConfig::default(),
        }
    } else {
        AddonsConfig::default()
    };

    *guard = Some(config.clone());
    config
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

/// Setup defaults for Cinemeta and Torrentio
pub fn setup_dev_default_addons() -> Result<(AddonDescriptor, AddonDescriptor), String> {
    log_info("Registering Cinemeta and Torrentio in addons.json...");

    let cinemeta = AddonDescriptor {
        transport_url: "https://v3-cinemeta.strem.io/manifest.json".to_string(),
        manifest: Manifest {
            id: "com.linvo.cinemeta".to_string(),
            name: "Cinemeta".to_string(),
            version: "3.0.12".to_string(),
            description: Some("Official movies and series catalog".to_string()),
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
        },
    };

    let torrentio = AddonDescriptor {
        transport_url: "https://torrentio.strem.fun/manifest.json".to_string(),
        manifest: Manifest {
            id: "com.stremio.torrentio".to_string(),
            name: "Torrentio".to_string(),
            version: "1.0.14".to_string(),
            description: Some("Torrent streams from multiple providers".to_string()),
            types: vec!["movie".to_string(), "series".to_string()],
            catalogs: vec![],
        },
    };

    add_or_update_addon(cinemeta.clone());
    add_or_update_addon(torrentio.clone());

    log_info("Cinemeta and Torrentio configured successfully.");
    Ok((cinemeta, torrentio))
}
