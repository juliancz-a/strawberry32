use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Mutex;

/// Persisted Jellyfin server connection (NEVER stores password).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JellyfinServerConfig {
    pub id: String,
    pub name: String,
    pub server_url: String,
    pub user_id: String,
    pub username: String,
    pub access_token: String,
    pub device_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct JellyfinConfigFile {
    pub servers: Vec<JellyfinServerConfig>,
}

static SERVERS_CACHE: Mutex<Option<Vec<JellyfinServerConfig>>> = Mutex::new(None);

pub fn load_jellyfin_servers() -> Vec<JellyfinServerConfig> {
    let mut guard = SERVERS_CACHE.lock().unwrap();
    if let Some(list) = guard.as_ref() {
        return list.clone();
    }
    let path = crate::state::get_data_dir().join("jellyfin_servers.json");
    let list = if path.is_file() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<JellyfinConfigFile>(&s).ok())
            .map(|c| c.servers)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    *guard = Some(list.clone());
    list
}

pub fn save_jellyfin_servers(servers: &[JellyfinServerConfig]) {
    let path = crate::state::get_data_dir().join("jellyfin_servers.json");
    let cfg = JellyfinConfigFile {
        servers: servers.to_vec(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&cfg) {
        let tmp = path.with_extension("tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::remove_file(&path);
            let _ = fs::rename(&tmp, &path);
        }
    }
    let mut guard = SERVERS_CACHE.lock().unwrap();
    *guard = Some(servers.to_vec());
}

pub fn add_or_update_jellyfin_server(server: JellyfinServerConfig) {
    let mut list = load_jellyfin_servers();
    list.retain(|s| s.id != server.id && s.server_url != server.server_url);
    list.push(server);
    save_jellyfin_servers(&list);
}

pub fn remove_jellyfin_server(id: &str) {
    let mut list = load_jellyfin_servers();
    list.retain(|s| s.id != id);
    save_jellyfin_servers(&list);
}

pub fn get_jellyfin_server(id: &str) -> Option<JellyfinServerConfig> {
    load_jellyfin_servers().into_iter().find(|s| s.id == id)
}

