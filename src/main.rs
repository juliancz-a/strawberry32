mod addons;
mod cache_cleaner;
mod container_meta;
mod engine;
mod gui;
mod logger;
mod player;
mod state;
mod subtitles;
mod trackers;
mod ui;

use logger::{log_err, log_info};
use std::fs::OpenOptions;
use std::io::Write;

fn main() {
    // 0. Install panic hook to persist fatal panics to logs/crash.log
    std::panic::set_hook(Box::new(|info| {
        let crash_file = state::get_logs_dir().join("crash.log");
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown panic payload"
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let report = format!(
            "=== STRAWBERRY32 CRASH REPORT ===\nLocation: {}\nMessage: {}\n\n",
            location, payload
        );

        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&crash_file) {
            let _ = f.write_all(report.as_bytes());
        }
        log_err(&format!("CRITICAL UNCAUGHT PANIC at {}: {}", location, payload));
    }));

    // 1. Initialize thread-safe telemetry and file logger
    let log_path = logger::init();
    log_info("=== STARTING STRAWBERRY32 APPLICATION (ARM32) ===");
    log_info(&format!("Active diagnostic log: {}", log_path.display()));

    // 2. Resolve storage root and canonical layout
    let storage_root = state::get_app_storage_root();
    log_info(&format!("Strawberry32 storage root: {}", storage_root.display()));
    migrate_legacy_data_if_needed(&storage_root);

    // 3. Start background librqbit engine & HTTP range server on 127.0.0.1:3030
    log_info("Starting librqbit 8.0 streaming engine on background thread...");
    if let Err(e) = engine::start_engine() {
        log_err(&format!("Critical error starting librqbit engine: {}", e));
    }

    // 4. Initialize live tracker manager (24h RTT probe & cache)
    trackers::init_tracker_manager();

    // 5. Start protected LRU disk cache cleaner
    cache_cleaner::start_cache_cleaner();

    // 6. Launch Slint native user interface
    log_info("Launching native Slint user interface...");
    if let Err(e) = gui::run_app() {
        log_err(&format!("Error in Slint UI event loop: {}", e));
    }

    log_info("Closing Strawberry32...");
    // Terminate tokio background threads and all sockets cleanly
    std::process::exit(0);
}

/// Automatically migrates settings and data from old folders ('streaming-app-arm32', 'rt_stream_data')
/// into '%LOCALAPPDATA%\Strawberry32' and removes old directories.
fn migrate_legacy_data_if_needed(target_root: &std::path::Path) {
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let legacy_dir = std::path::PathBuf::from(local_appdata).join("streaming-app-arm32");
        if legacy_dir.is_dir() {
            log_info(&format!("Migrating legacy data from {}...", legacy_dir.display()));
            let target_data = target_root.join("data");
            let _ = std::fs::create_dir_all(&target_data);
            for name in &["settings.json", "watch_history.json", "addons.json"] {
                let old_file = legacy_dir.join(name);
                let new_file = target_data.join(name);
                if old_file.is_file() && !new_file.exists() {
                    let _ = std::fs::copy(&old_file, &new_file);
                }
            }
            let _ = std::fs::remove_dir_all(&legacy_dir);
        }
    }

    let temp_data = logger::get_temp_dir().join("rt_stream_data");
    if temp_data.is_dir() {
        let target_data = target_root.join("data");
        for name in &["settings.json", "watch_history.json", "addons.json", "vlc_path.txt"] {
            let old_file = temp_data.join(name);
            let new_file = target_data.join(name);
            if old_file.is_file() && !new_file.exists() {
                let _ = std::fs::copy(&old_file, &new_file);
            }
        }
        let _ = std::fs::remove_dir_all(&temp_data);
    }

    let temp_cache = logger::get_temp_dir().join("rt_stream_cache");
    if temp_cache.is_dir() {
        let _ = std::fs::remove_dir_all(&temp_cache);
    }
}
