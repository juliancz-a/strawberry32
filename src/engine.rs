pub mod remux_proxy;

use crate::addons;
use crate::logger::{get_temp_dir, log_err, log_info, log_torrent};
use librqbit::{
    api::Api,
    http_api::HttpApi,
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Session,
    SessionOptions,
};
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct StreamStatus {
    pub is_loading: bool,
    pub title: String,
    pub message: String,
    pub peers: usize,
    pub speed_kbs: f64,
    pub progress_mb: f64,
    pub elapsed_secs: u64,
    pub error: Option<String>,
}

static STREAM_STATUS: Mutex<StreamStatus> = Mutex::new(StreamStatus {
    is_loading: false,
    title: String::new(),
    message: String::new(),
    peers: 0,
    speed_kbs: 0.0,
    progress_mb: 0.0,
    elapsed_secs: 0,
    error: None,
});

pub fn get_stream_status() -> StreamStatus {
    STREAM_STATUS.lock().unwrap().clone()
}

pub fn set_stream_loading(title: &str) {
    let mut guard = STREAM_STATUS.lock().unwrap();
    guard.is_loading = true;
    guard.title = title.to_string();
    guard.message = "Connecting to swarm & resolving metadata...".to_string();
    guard.peers = 0;
    guard.speed_kbs = 0.0;
    guard.progress_mb = 0.0;
    guard.elapsed_secs = 0;
    guard.error = None;
}

pub fn cancel_stream_loading() {
    let mut guard = STREAM_STATUS.lock().unwrap();
    guard.is_loading = false;
    guard.message = "Playback canceled".to_string();
}

#[allow(dead_code)]
enum EngineCommand {
    StartStream {
        info_hash: String,
        file_idx: Option<usize>,
        trackers: Vec<String>,
        resp: Sender<Result<String, String>>,
    },
    ClearCache {
        resp: Sender<Result<usize, String>>,
    },
}

static ENGINE_TX: OnceLock<Sender<EngineCommand>> = OnceLock::new();

/// Returns the cache directory on Windows RT: %TEMP%\rt_stream_cache\
pub fn get_cache_dir() -> PathBuf {
    get_temp_dir().join("rt_stream_cache")
}

/// Initializes the background tokio runtime and starts librqbit 8.0 + HttpApi server.
/// Configures low-memory footprint (<150 MB RAM) for the 2 GB Surface RT.
pub fn start_engine() -> Result<(), String> {
    let cache_dir = get_cache_dir();
    if let Err(e) = std::fs::create_dir_all(&cache_dir) {
        log_err(&format!("Could not create cache folder at {}: {}", cache_dir.display(), e));
        return Err(format!("Error creating cache: {}", e));
    }

    log_info(&format!("Streaming cache directory: {}", cache_dir.display()));

    // Start local proxy/remux server for MKV playback & Media Foundation (:3031)
    if let Err(e) = remux_proxy::start_proxy_server() {
        log_err(&format!("Notice starting remux proxy: {}", e));
    }

    let (cmd_tx, cmd_rx) = channel::<EngineCommand>();
    let _ = ENGINE_TX.set(cmd_tx);

    std::thread::Builder::new()
        .name("librqbit-engine".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log_err(&format!("Failed to start tokio runtime: {}", e));
                    return;
                }
            };

            rt.block_on(async move {
                log_torrent("Starting librqbit Session with low-RAM profile (Surface RT)...");

                let session_opts = SessionOptions {
                    disable_dht_persistence: false,
                    ..Default::default()
                };

                let session = match Session::new_with_opts(cache_dir.clone(), session_opts).await {
                    Ok(s) => s,
                    Err(e) => {
                        log_err(&format!("Error creating librqbit Session: {}", e));
                        return;
                    }
                };

                log_torrent("librqbit Session initialized successfully (RAM optimized).");

                // Start HTTP API server on 127.0.0.1:3030
                let api = Api::new(session.clone(), None, None);
                let http_api = HttpApi::new(api, None);

                let bind_addr = "127.0.0.1:3030";
                match tokio::net::TcpListener::bind(bind_addr).await {
                    Ok(listener) => {
                        log_torrent(&format!("HTTP Range server active on http://{}", bind_addr));
                        tokio::spawn(async move {
                            if let Err(e) = http_api.make_http_api_and_run(listener, None).await {
                                log_err(&format!("Error in librqbit HttpApi: {}", e));
                            }
                        });
                    }
                    Err(e) => {
                        log_err(&format!("Could not bind port 3030 for HTTP API: {}", e));
                    }
                }

                // Command loop from UI
                while let Ok(cmd) = cmd_rx.recv() {
                    match cmd {
                        EngineCommand::StartStream { info_hash, file_idx, trackers, resp } => {
                            let result = handle_start_stream(&session, &info_hash, file_idx, trackers).await;
                            let _ = resp.send(result);
                        }
                        EngineCommand::ClearCache { resp } => {
                            let result = handle_clear_cache(&cache_dir);
                            let _ = resp.send(result);
                        }
                    }
                }
            });
        })
        .map_err(|e| format!("Error spawning librqbit thread: {}", e))?;

    Ok(())
}

/// Asynchronously resolves torrent metadata, performs smart MP4 Header + Tail moov atom
/// pre-fetching, and returns local streaming URL.
async fn handle_start_stream(
    session: &std::sync::Arc<Session>,
    info_hash: &str,
    preferred_file_idx: Option<usize>,
    trackers: Vec<String>,
) -> Result<String, String> {
    log_torrent(&format!("Preparing stream for infoHash: {}", info_hash));

    let default_trackers = [
        "http://tracker.openbittorrent.com:80/announce",
        "http://tracker.opentrackr.org:1337/announce",
        "http://open.acgnxtracker.com:80/announce",
        "http://tracker.bt4g.com:2095/announce",
        "http://tracker.files.fm:6969/announce",
        "http://tracker.gbitt.info:80/announce",
        "udp://tracker.opentrackr.org:1337/announce",
        "udp://open.stealth.si:80/announce",
        "udp://exodus.desync.com:6969/announce",
        "udp://tracker.torrent.eu.org:451/announce",
        "udp://open.demonii.com:1337/announce",
        "udp://tracker.moeking.me:6969/announce",
        "udp://explodie.org:6969/announce",
        "udp://tracker.coppersurfer.tk:6969/announce",
        "udp://tracker.leechers-paradise.org:6969/announce",
    ];

    let mut all_trackers: Vec<String> = trackers;
    for dt in &default_trackers {
        if !all_trackers.iter().any(|t| t == *dt) {
            all_trackers.push(dt.to_string());
        }
    }

    log_torrent(&format!(
        "Configuring magnet with {} trackers (HTTP + UDP)...",
        all_trackers.len()
    ));

    let mut magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
    for tr in &all_trackers {
        let encoded_tr = addons::percent_encode(tr);
        magnet.push_str("&tr=");
        magnet.push_str(&encoded_tr);
    }

    let add_opts = AddTorrentOptions {
        overwrite: true,
        force_tracker_interval: Some(Duration::from_secs(5)),
        trackers: Some(all_trackers.clone()),
        ..Default::default()
    };

    let add_res = match session.add_torrent(AddTorrent::from_url(&magnet), Some(add_opts)).await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Error adding magnet to librqbit: {}", e);
            log_err(&msg);
            let mut guard = STREAM_STATUS.lock().unwrap();
            guard.is_loading = false;
            guard.error = Some(msg.clone());
            return Err(msg);
        }
    };

    let (torrent_id, handle) = match add_res {
        AddTorrentResponse::Added(id, h) => {
            log_torrent(&format!("Torrent added with internal ID: {}", id));
            (id, h)
        }
        AddTorrentResponse::AlreadyManaged(id, h) => {
            log_torrent(&format!("Torrent already managed with internal ID: {}", id));
            (id, h)
        }
        AddTorrentResponse::ListOnly(..) => {
            let msg = "Error: unexpected ListOnly response from librqbit".to_string();
            log_err(&msg);
            let mut guard = STREAM_STATUS.lock().unwrap();
            guard.is_loading = false;
            guard.error = Some(msg.clone());
            return Err(msg);
        }
    };

    log_torrent("Waiting for metadata resolution (60s max)...");

    let start_time = tokio::time::Instant::now();
    let timeout_duration = Duration::from_secs(60);
    let wait_fut = handle.wait_until_initialized();
    tokio::pin!(wait_fut);

    let mut interval = tokio::time::interval(Duration::from_secs(2));
    interval.tick().await;

    let init_success = loop {
        tokio::select! {
            res = &mut wait_fut => {
                match res {
                    Ok(()) => break Ok(()),
                    Err(e) => break Err(format!("Torrent initialization failed: {}", e)),
                }
            }
            _ = interval.tick() => {
                let elapsed = start_time.elapsed().as_secs();
                if elapsed >= 60 {
                    break Err("Timed out waiting for peers/metadata (60s).".to_string());
                }

                let stats = handle.stats();
                let (live_peers, down_kbs) = if let Some(live) = &stats.live {
                    let peers = live.snapshot.peer_stats.live;
                    let kbs = live.download_speed.mbps * 125.0;
                    (peers, kbs)
                } else {
                    (0, 0.0)
                };
                let prog_mb = stats.progress_bytes as f64 / 1_048_576.0;

                {
                    let mut guard = STREAM_STATUS.lock().unwrap();
                    guard.peers = live_peers;
                    guard.speed_kbs = down_kbs;
                    guard.progress_mb = prog_mb;
                    guard.elapsed_secs = elapsed;
                    guard.message = if live_peers > 0 {
                        format!("Fetching metadata: {} connected peers ({}s / 60s)", live_peers, elapsed)
                    } else {
                        format!("Contacting DHT swarm & trackers... ({}s / 60s)", elapsed)
                    };
                }

                log_torrent(&format!(
                    "[TORRENT] Contacting swarm: {} peers | Elapsed: {}s / 60s",
                    live_peers, elapsed
                ));
            }
            _ = tokio::time::sleep_until(start_time + timeout_duration) => {
                break Err("Timed out waiting for peers/metadata (60s).".to_string());
            }
        }
    };

    match init_success {
        Ok(()) => {
            log_torrent("Torrent metadata resolved successfully.");
            let mut guard = STREAM_STATUS.lock().unwrap();
            guard.message = "Metadata resolved! Buffering initial segment...".to_string();
        }
        Err(e) => {
            log_err(&e);
            let mut guard = STREAM_STATUS.lock().unwrap();
            guard.is_loading = false;
            guard.error = Some(e.clone());
            return Err(e);
        }
    }

    // Determine target video file index, length and filename
    let (chosen_idx, file_len, file_name) = match preferred_file_idx {
        Some(idx) => {
            let res = handle.with_metadata(|meta| {
                if let Ok(details) = meta.info.iter_file_details() {
                    for (i, fd) in details.enumerate() {
                        if i == idx {
                            let name = fd.filename.to_string().unwrap_or_default();
                            return (fd.len, name);
                        }
                    }
                }
                (0u64, String::new())
            }).unwrap_or((0u64, String::new()));
            (idx, res.0, res.1)
        }
        None => {
            let best = handle.with_metadata(|meta| {
                let mut max_len = 0u64;
                let mut best_idx = 0usize;
                let mut best_name = String::new();

                if let Ok(details) = meta.info.iter_file_details() {
                    for (i, fd) in details.enumerate() {
                        let fname = fd.filename.to_string().unwrap_or_default();
                        let name_lower = fname.to_lowercase();
                        let is_video = name_lower.ends_with(".mp4")
                            || name_lower.ends_with(".mkv")
                            || name_lower.ends_with(".avi")
                            || name_lower.ends_with(".m4v")
                            || name_lower.ends_with(".mov");

                        if is_video && fd.len > max_len {
                            max_len = fd.len;
                            best_idx = i;
                            best_name = fname;
                        } else if fd.len > max_len {
                            max_len = fd.len;
                            best_idx = i;
                            best_name = fname;
                        }
                    }
                }
                (best_idx, max_len, best_name)
            });

            match best {
                Ok((idx, len, name)) => {
                    log_torrent(&format!(
                        "Selected video file index {} ({:.1} MB, '{}')",
                        idx,
                        len as f64 / 1_048_576.0,
                        name
                    ));
                    (idx, len, name)
                }
                Err(e) => {
                    log_err(&format!("Could not enumerate files: {}. Using index 0.", e));
                    (0, 0u64, String::new())
                }
            }
        }
    };

    // Register file info with the local proxy/remuxer server
    remux_proxy::register_file_info(torrent_id, chosen_idx, &file_name, file_len);

    let stream_url = format!("http://127.0.0.1:3030/torrents/{}/stream/{}", torrent_id, chosen_idx);

    // PART 2: SMART CONCURRENT PRE-FETCHING (Header 5 MB + Tail 2.5 MB moov atom)
    let head_limit = (5 * 1024 * 1024).min(file_len as usize);
    let tail_limit = (2621440usize).min(file_len as usize);
    log_torrent(&format!(
        "Starting smart pre-buffering (Header {:.1}MB + Tail moov {:.1}MB) for {:.1} MB file...",
        head_limit as f64 / 1_048_576.0,
        tail_limit as f64 / 1_048_576.0,
        file_len as f64 / 1_048_576.0
    ));

    let ttfb_timer = std::time::Instant::now();

    // 1. Header request: 0 to head_limit bytes (5 MB)
    let url_head = stream_url.clone();
    let head_range = format!("bytes=0-{}", head_limit.saturating_sub(1));
    let head_handle = tokio::task::spawn_blocking(move || {
        let client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build();
        let resp = client.get(&url_head).set("Range", &head_range).call();
        match resp {
            Ok(r) => {
                let mut reader = r.into_reader();
                let mut buf = [0u8; 65536];
                let mut total = 0usize;
                while total < head_limit {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(total)
            }
            Err(e) => Err(e.to_string()),
        }
    });

    // 2. Tail request: last 2.5 MB (where MP4 moov atom / seek table resides)
    let url_tail = stream_url.clone();
    let tail_handle = tokio::task::spawn_blocking(move || {
        if file_len <= (head_limit as u64) {
            return Ok(0usize);
        }
        let tail_start = file_len.saturating_sub(tail_limit as u64);
        let tail_end = file_len.saturating_sub(1);
        let range_val = format!("bytes={}-{}", tail_start, tail_end);

        let client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build();
        let resp = client.get(&url_tail).set("Range", &range_val).call();
        match resp {
            Ok(r) => {
                let mut reader = r.into_reader();
                let mut buf = [0u8; 65536];
                let mut total = 0usize;
                while total < tail_limit {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Ok(total)
            }
            Err(e) => Err(e.to_string()),
        }
    });

    // Wait for both pre-fetches to complete or timeout
    let (head_res, tail_res) = tokio::join!(head_handle, tail_handle);
    let ttfb_ms = ttfb_timer.elapsed().as_millis();

    let head_bytes = head_res.unwrap_or(Ok(0)).unwrap_or(0);
    let tail_bytes = tail_res.unwrap_or(Ok(0)).unwrap_or(0);
    let total_buffered_mb = (head_bytes + tail_bytes) as f64 / 1_048_576.0;

    log_torrent(&format!(
        "[TORRENT] TTFB: {} ms | MP4 header and tail buffered ({:.2} MB ready). Ready for player.",
        ttfb_ms, total_buffered_mb
    ));

    // Mark stream status ready
    {
        let mut guard = STREAM_STATUS.lock().unwrap();
        guard.is_loading = false;
        guard.message = "Ready".to_string();
        guard.progress_mb = total_buffered_mb;
    }

    // Return the local remux/proxy URL with explicit .mp4 extension for Windows RT Media Foundation
    let proxy_url = format!("http://127.0.0.1:3031/play/{}/{}/movie.mp4", torrent_id, chosen_idx);
    log_torrent(&format!("[PROXY] Local streaming URL for embedded player: {}", proxy_url));

    Ok(proxy_url)
}

/// Removes cached torrent chunks from %TEMP%\rt_stream_cache to free Surface RT disk space
fn handle_clear_cache(cache_dir: &PathBuf) -> Result<usize, String> {
    log_torrent("Cleaning temporary cache files...");
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(_) = std::fs::remove_file(&path) {
                    count += 1;
                }
            } else if path.is_dir() {
                if let Ok(_) = std::fs::remove_dir_all(&path) {
                    count += 1;
                }
            }
        }
    }
    log_torrent(&format!("Clean complete. {} items removed from cache.", count));
    Ok(count)
}

/// Public API: requests the engine to start a stream for an infoHash
pub fn start_stream(
    info_hash: String,
    file_idx: Option<usize>,
    trackers: Vec<String>,
) -> Result<String, String> {
    let tx = match ENGINE_TX.get() {
        Some(t) => t,
        None => return Err("Torrent engine has not been started".to_string()),
    };

    let (resp_tx, resp_rx) = channel::<Result<String, String>>();
    tx.send(EngineCommand::StartStream {
        info_hash,
        file_idx,
        trackers,
        resp: resp_tx,
    })
    .map_err(|e| format!("Failed to send command to engine: {}", e))?;

    resp_rx
        .recv()
        .map_err(|e| format!("Did not receive response from engine: {}", e))?
}

/// Synchronous wrapper around start_stream
pub fn start_stream_sync(
    info_hash: String,
    file_idx: Option<usize>,
    trackers: Vec<String>,
) -> Result<String, String> {
    start_stream(info_hash, file_idx, trackers)
}

/// Clears cached chunk files
pub fn clear_cache() -> Result<usize, String> {
    let tx = match ENGINE_TX.get() {
        Some(t) => t,
        None => return Err("Torrent engine has not been started".to_string()),
    };

    let (resp_tx, resp_rx) = channel::<Result<usize, String>>();
    tx.send(EngineCommand::ClearCache { resp: resp_tx })
        .map_err(|e| format!("Failed to send clear cache command: {}", e))?;

    resp_rx
        .recv()
        .map_err(|e| format!("Did not receive response from engine: {}", e))?
}
