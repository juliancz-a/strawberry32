mod addons;
mod engine;
mod gui;
mod logger;
mod player;
mod state;
mod subtitles;
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

    // 3. Start background librqbit engine & HTTP range server on 127.0.0.1:3030
    log_info("Starting librqbit 8.0 streaming engine on background thread...");
    if let Err(e) = engine::start_engine() {
        log_err(&format!("Critical error starting librqbit engine: {}", e));
    }

    // 4. Launch Slint native user interface
    log_info("Launching native Slint user interface...");
    if let Err(e) = gui::run_app() {
        log_err(&format!("Error in Slint UI event loop: {}", e));
    }

    log_info("Closing Strawberry32...");
    // Terminate tokio background threads and all sockets cleanly
    std::process::exit(0);
}
