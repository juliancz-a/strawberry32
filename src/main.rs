mod addons;
mod engine;
mod gui;
mod logger;
mod player;
mod state;
mod subtitles;

use logger::{log_err, log_info};

fn main() {
    // 1. Initialize thread-safe telemetry and file logger
    let log_path = logger::init();
    log_info("=== STARTING NATIVE RT-STREAM APPLICATION (ARM32) ===");
    log_info(&format!("Active diagnostic log: {}", log_path.display()));

    // 2. Ensure cache directory in %TEMP% exists
    let cache_dir = engine::get_cache_dir();
    log_info(&format!("Torrent storage directory: {}", cache_dir.display()));

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

    log_info("Closing RT-Stream...");
    // Terminate tokio background threads and all sockets cleanly
    std::process::exit(0);
}
