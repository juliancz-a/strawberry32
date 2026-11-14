pub mod remux_proxy;

use crate::addons;
use crate::logger::{log_err, log_info, log_torrent};
use librqbit::{
    api::Api,
    http_api::HttpApi,
    limits::LimitsConfig,
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
    pub stage: u8, // 1=Peers (15%), 2=Meta (15%), 3=Container (10%), 4=Index (10%), 5=Buffering (40%), 6=Player (10%), 7=Ready
    pub title: String,
    pub message: String,
    pub peers: usize,
    pub speed_kbs: f64,
    pub progress_mb: f64,
    pub target_mb: f64,
    pub elapsed_secs: u64,
    pub eta_secs: u64,
    pub error: Option<String>,
    pub progress_ratio: f32,
    pub has_hint: bool,
    pub hint_message: String,
    pub events: Vec<String>,
}

static STREAM_STATUS: Mutex<StreamStatus> = Mutex::new(StreamStatus {
    is_loading: false,
    stage: 0,
    title: String::new(),
    message: String::new(),
    peers: 0,
    speed_kbs: 0.0,
    progress_mb: 0.0,
    target_mb: 0.0,
    elapsed_secs: 0,
    eta_secs: 0,
    error: None,
    progress_ratio: 0.0,
    has_hint: false,
    hint_message: String::new(),
    events: Vec::new(),
});

pub fn record_stream_event(msg: &str) {
    let mut guard = STREAM_STATUS.lock().unwrap();
    guard.events.push(msg.to_string());
    if guard.events.len() > 8 {
        guard.events.remove(0);
    }
}

static CURRENT_VIDEO_HASH: Mutex<Option<(String, u64)>> = Mutex::new(None);
static BUNDLED_SUBTITLES: Mutex<Vec<crate::subtitles::BundledSubtitle>> = Mutex::new(Vec::new());

#[link(name = "kernel32")]
extern "system" {
    fn GetDiskFreeSpaceExW(
        lpDirectoryName: *const u16,
        lpFreeBytesAvailableToCaller: *mut u64,
        lpTotalNumberOfBytes: *mut u64,
        lpTotalNumberOfFreeBytes: *mut u64,
    ) -> i32;
}

/// Queries free disk space in megabytes for the drive holding the specified path
pub fn get_disk_free_mb(path: &std::path::Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let mut free_avail: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut total_free: u64 = 0;
    let ret = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_avail,
            &mut total_bytes,
            &mut total_free,
        )
    };
    if ret != 0 {
        free_avail / (1024 * 1024)
    } else {
        u64::MAX
    }
}

/// Deletes a specific torrent cache folder or files after playback stops
pub fn delete_torrent_cache(info_hash: &str) {
    let cache_dir = get_cache_dir();
    let lower_hash = info_hash.to_lowercase();
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_lowercase();
            if name.contains(&lower_hash) {
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
                log_torrent(&format!("[CACHE] Deleted torrent payload for {} from disk", info_hash));
            }
        }
    }
}

pub fn get_stream_status() -> StreamStatus {
    STREAM_STATUS.lock().unwrap().clone()
}

pub fn get_current_video_hash() -> Option<(String, u64)> {
    CURRENT_VIDEO_HASH.lock().unwrap().clone()
}

pub fn get_bundled_subtitles() -> Vec<crate::subtitles::BundledSubtitle> {
    BUNDLED_SUBTITLES.lock().unwrap().clone()
}

pub fn set_stream_loading(title: &str) {
    let mut guard = STREAM_STATUS.lock().unwrap();
    guard.is_loading = true;
    guard.stage = 1;
    guard.title = title.to_string();
    guard.message = "Contacting DHT swarm & trackers...".to_string();
    guard.peers = 0;
    guard.speed_kbs = 0.0;
    guard.progress_mb = 0.0;
    guard.target_mb = 0.0;
    guard.elapsed_secs = 0;
    guard.eta_secs = 0;
    guard.error = None;
    guard.progress_ratio = 0.05;
    guard.has_hint = false;
    guard.hint_message.clear();
    guard.events.clear();
    guard.events.push("Initializing streaming session...".to_string());
}

pub fn cancel_stream_loading() {
    let mut guard = STREAM_STATUS.lock().unwrap();
    guard.is_loading = false;
    guard.stage = 0;
    guard.message = "Playback canceled".to_string();
}

pub fn finish_stream_loading() {
    let mut guard = STREAM_STATUS.lock().unwrap();
    guard.is_loading = false;
    guard.stage = 7;
    guard.progress_ratio = 1.0;
    guard.message = "Playing".to_string();
}

#[allow(dead_code)]
enum EngineCommand {
    StartStream {
        info_hash: String,
        file_idx: Option<usize>,
        season: Option<u32>,
        episode: Option<u32>,
        trackers: Vec<String>,
        resp: Sender<Result<String, String>>,
    },
    StopActiveStream,
    ClearCache {
        resp: Sender<Result<usize, String>>,
    },
}

static ENGINE_TX: OnceLock<Sender<EngineCommand>> = OnceLock::new();
static ACTIVE_TORRENT_ID: Mutex<Option<usize>> = Mutex::new(None);

pub fn stop_active_stream() {
    BUNDLED_SUBTITLES.lock().unwrap().clear();
    if let Some(tx) = ENGINE_TX.get() {
        let _ = tx.send(EngineCommand::StopActiveStream);
    }
}

/// Returns the cache directory on Windows RT: %LOCALAPPDATA%\Strawberry32\cache\torrents\
pub fn get_cache_dir() -> PathBuf {
    crate::state::get_app_storage_root().join("cache").join("torrents")
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

                let cache_cfg = crate::state::load_settings().cache;
                let max_conn = if cache_cfg.max_connections > 0 { cache_cfg.max_connections } else { cache_cfg.max_peers };
                let ratelimits = LimitsConfig {
                    download_bps: if cache_cfg.download_limit_kbps > 0 {
                        std::num::NonZeroU32::new(cache_cfg.download_limit_kbps.saturating_mul(1024))
                    } else {
                        None
                    },
                    upload_bps: None,
                };
                log_torrent(&format!("[CACHE_CONFIG] Applied torrent limits: max_connections={}, download_limit_kbps={}", max_conn, cache_cfg.download_limit_kbps));

                let session_opts = SessionOptions {
                    listen_port_range: Some(42000..42010),
                    enable_upnp_port_forwarding: true,
                    peer_opts: Some(librqbit::PeerConnectionOptions {
                        connect_timeout: Some(Duration::from_secs(10)),
                        read_write_timeout: Some(Duration::from_secs(30)),
                        ..Default::default()
                    }),
                    disable_dht_persistence: false,
                    ratelimits: ratelimits.clone(),
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
                        EngineCommand::StartStream { info_hash, file_idx, season, episode, trackers, resp } => {
                            let result = handle_start_stream(&session, &info_hash, file_idx, season, episode, trackers).await;
                            let _ = resp.send(result);
                        }
                        EngineCommand::StopActiveStream => {
                            stop_previous_torrent(&session).await;
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

async fn stop_previous_torrent(session: &std::sync::Arc<Session>) {
    BUNDLED_SUBTITLES.lock().unwrap().clear();
    crate::engine::remux_proxy::bump_stream_generation();
    let old_id = {
        let mut guard = ACTIVE_TORRENT_ID.lock().unwrap();
        guard.take()
    };
    if let Some(id) = old_id {
        let delete_files = !crate::state::load_settings().cache.keep_after_playback;
        log_torrent(&format!("[ENGINE] Parando y liberando torrent activo id={} de la RAM (delete_files: {})", id, delete_files));
        let _ = session.delete(librqbit::api::TorrentIdOrHash::Id(id), delete_files).await;
    }
    crate::engine::remux_proxy::clear_mkv_cache();
}

/// Asynchronously resolves torrent metadata, performs smart MP4 Header + Tail moov atom
/// pre-fetching, and returns local streaming URL.
async fn handle_start_stream(
    session: &std::sync::Arc<Session>,
    info_hash: &str,
    preferred_file_idx: Option<usize>,
    season: Option<u32>,
    episode: Option<u32>,
    trackers: Vec<String>,
) -> Result<String, String> {
    stop_previous_torrent(session).await;
    let t_fast_start = std::time::Instant::now();
    log_torrent(&format!("Preparing stream for infoHash: {}", info_hash));

    // Phase 5: Disk Guard - Check free disk space before buffering
    let cache_dir = get_cache_dir();
    let free_mb = get_disk_free_mb(&cache_dir);
    log_torrent(&format!("[DISK_GUARD] Free disk space on cache drive: {} MB", free_mb));
    if free_mb < 500 {
        log_err(&format!("[DISK_GUARD] Low disk space ({} MB < 500 MB). Triggering cache clean pass...", free_mb));
        let _ = handle_clear_cache(&cache_dir);
    }

    let verified_trackers = crate::trackers::get_verified_trackers();
    let mut all_trackers: Vec<String> = trackers;
    for vt in &verified_trackers {
        if !all_trackers.iter().any(|t| t == vt) {
            all_trackers.push(vt.clone());
        }
    }

    log_torrent(&format!(
        "Configuring magnet with {} verified live trackers...",
        all_trackers.len()
    ));

    let mut magnet = format!("magnet:?xt=urn:btih:{}", info_hash);
    for tr in &all_trackers {
        let encoded_tr = addons::percent_encode(tr);
        magnet.push_str("&tr=");
        magnet.push_str(&encoded_tr);
    }

    let cache_cfg = crate::state::load_settings().cache;
    let max_conn = if cache_cfg.max_connections > 0 { cache_cfg.max_connections } else { cache_cfg.max_peers };
    let ratelimits = LimitsConfig {
        download_bps: if cache_cfg.download_limit_kbps > 0 {
            std::num::NonZeroU32::new(cache_cfg.download_limit_kbps.saturating_mul(1024))
        } else {
            None
        },
        upload_bps: None,
    };
    log_torrent(&format!("[CACHE_CONFIG] Applied torrent limits: max_connections={}, download_limit_kbps={}", max_conn, cache_cfg.download_limit_kbps));

    let mut add_opts = AddTorrentOptions {
        overwrite: true,
        force_tracker_interval: Some(Duration::from_secs(5)),
        trackers: Some(all_trackers.clone()),
        ratelimits,
        ..Default::default()
    };
    if let Some(idx) = preferred_file_idx {
        add_opts.only_files = Some(vec![idx]);
    }

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

    *ACTIVE_TORRENT_ID.lock().unwrap() = Some(torrent_id);

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

                    if live_peers == 0 {
                        guard.stage = 1;
                        guard.progress_ratio = ((elapsed as f32 / 15.0).min(1.0) * 0.15).max(0.02);
                        guard.message = format!("Contacting swarm & trackers... ({}s / 60s)", elapsed);
                        if elapsed >= 15 {
                            guard.has_hint = true;
                            guard.hint_message = "Few peers detected. Try another source if buffering stalls.".to_string();
                        }
                    } else {
                        guard.stage = 2;
                        guard.progress_ratio = 0.15 + ((elapsed as f32 / 20.0).min(1.0) * 0.15);
                        guard.message = format!("Fetching metadata: {} connected peers ({}s / 60s)", live_peers, elapsed);
                    }
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
            log_torrent(&format!("[FAST_START] Torrent metadata resolved in {} ms", t_fast_start.elapsed().as_millis()));
            record_stream_event("Torrent metadata resolved successfully.");
            let mut guard = STREAM_STATUS.lock().unwrap();
            guard.stage = 3;
            guard.progress_ratio = 0.30;
            guard.message = "Metadata resolved! Reading container headers...".to_string();
        }
        Err(e) => {
            log_err(&e);
            let mut guard = STREAM_STATUS.lock().unwrap();
            guard.is_loading = false;
            guard.error = Some(e.clone());
            return Err(e);
        }
    }

    // Enumerate all files from torrent metadata
    let all_files: Vec<(usize, u64, String)> = handle.with_metadata(|meta| {
        let mut list = Vec::new();
        if let Ok(details) = meta.info.iter_file_details() {
            for (i, fd) in details.enumerate() {
                list.push((i, fd.len, fd.filename.to_string().unwrap_or_default()));
            }
        }
        list
    }).unwrap_or_default();

    // Scan for bundled subtitle files (.srt, .vtt, .ass, .ssa)
    let bundled_subs_info: Vec<(usize, String, u64)> = all_files.iter().map(|(i, len, name)| (*i, name.clone(), *len)).collect();
    let found_bundled = crate::subtitles::find_bundled_subtitles(&bundled_subs_info);
    *BUNDLED_SUBTITLES.lock().unwrap() = found_bundled.clone();

    // Determine target video file index, length and filename (with series season/episode matching)
    let (chosen_idx, file_len, file_name) = match preferred_file_idx {
        Some(idx) if idx < all_files.len() => {
            let (i, len, name) = &all_files[idx];
            (*i, *len, name.clone())
        }
        _ => {
            let s_info = season.and_then(|s| episode.map(|e| (s, e)));
            let idx = guess_file_index(&all_files, s_info);
            if idx < all_files.len() {
                let (i, len, name) = &all_files[idx];
                (*i, *len, name.clone())
            } else {
                (0, 0u64, String::new())
            }
        }
    };

    log_torrent(&format!(
        "Selected video file index {} ({:.1} MB, '{}')",
        chosen_idx,
        file_len as f64 / 1_048_576.0,
        file_name
    ));

    // Restrict librqbit download exclusively to the target file and any bundled subtitles
    let mut only_files_set = std::collections::HashSet::new();
    only_files_set.insert(chosen_idx);
    for sub in &found_bundled {
        only_files_set.insert(sub.file_idx);
    }
    if let Err(e) = session.update_only_files(&handle, &only_files_set).await {
        log_err(&format!("Notice: update_only_files failed: {}", e));
    } else {
        log_torrent(&format!("Torrent pack isolation active: downloading exclusively file index {} (plus {} bundled subtitles).", chosen_idx, found_bundled.len()));
    }

    // Register file info with the local proxy/remuxer server
    remux_proxy::register_file_info(torrent_id, chosen_idx, &file_name, file_len);

    let is_mkv = file_name.to_lowercase().ends_with(".mkv")
        || file_name.to_lowercase().ends_with(".webm");
    crate::player::set_stream_container_info(is_mkv, 0);

    let stream_url = format!("http://127.0.0.1:3030/torrents/{}/stream/{}", torrent_id, chosen_idx);

    // Track active folder for protected cache cleaning
    let active_torrent_folder = get_cache_dir().join(&file_name);
    crate::cache_cleaner::set_active_torrent_dir(Some(active_torrent_folder));

    // PART 2: SMART CONTAINER METADATA INSPECTION & TARGETED PRE-BUFFERING
    {
        record_stream_event(&format!("Reading container headers: {}", file_name));
        let mut guard = STREAM_STATUS.lock().unwrap();
        guard.stage = 3;
        guard.progress_ratio = 0.35;
        guard.message = format!("Reading container: {} ({:.1} MB)", file_name, file_len as f64 / 1_048_576.0);
    }

    let head_chunk_size = 262_144usize.min(file_len as usize); // 256 KB
    let url_head = stream_url.clone();
    let head_range_hdr = "bytes=0-".to_string();
    let head_handle = tokio::task::spawn_blocking(move || -> Vec<u8> {
        let client = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build();
        if let Ok(resp) = client.get(&url_head).set("Range", &head_range_hdr).call() {
            let mut reader = Read::take(resp.into_reader(), head_chunk_size as u64);
            let mut buf = Vec::with_capacity(head_chunk_size);
            let _ = std::io::copy(&mut reader, &mut buf);
            buf
        } else {
            Vec::new()
        }
    });

    let ttfb_timer = std::time::Instant::now();
    let head_bytes = head_handle.await.unwrap_or_default();

    // Compute OpenSubtitles hash asynchronously in background - NEVER block playback start!
    let tail_chunk_size = 65536u64.min(file_len);
    let tail_start = file_len.saturating_sub(tail_chunk_size);
    let url_tail = stream_url.clone();
    let tail_range_hdr = format!("bytes={}-", tail_start);
    let head_bytes_for_hash = head_bytes.clone();
    tokio::spawn(async move {
        let tail_bytes = tokio::task::spawn_blocking(move || -> Vec<u8> {
            let client = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(60))
                .build();
            if let Ok(resp) = client.get(&url_tail).set("Range", &tail_range_hdr).call() {
                let mut reader = Read::take(resp.into_reader(), tail_chunk_size);
                let mut buf = Vec::with_capacity(tail_chunk_size as usize);
                let _ = std::io::copy(&mut reader, &mut buf);
                buf
            } else {
                Vec::new()
            }
        }).await.unwrap_or_default();

        if !head_bytes_for_hash.is_empty() && !tail_bytes.is_empty() {
            let opensub_hash = crate::subtitles::calculate_opensub_hash(file_len, &head_bytes_for_hash, &tail_bytes);
            log_info(&format!("Computed OpenSubtitles videoHash in background: {} for size: {}", opensub_hash, file_len));
            *CURRENT_VIDEO_HASH.lock().unwrap() = Some((opensub_hash, file_len));
        }
    });

    // Inspect container metadata
    let mut cursor = std::io::Cursor::new(head_bytes.clone());
    let meta = crate::container_meta::MetadataInspector::get_or_inspect(
        info_hash,
        chosen_idx,
        &file_name,
        file_len,
        &mut cursor,
    ).await;

    // Targeted atom pre-fetching:
    {
        let mut guard = STREAM_STATUS.lock().unwrap();
        guard.stage = 4;
        guard.progress_ratio = 0.45;
        guard.message = "Fetching container index (moov/Cues)...".to_string();
    }

    if let Some((moov_offset, moov_size)) = meta.moov_range {
        if moov_offset > (head_chunk_size as u64) {
            record_stream_event(&format!("Pre-fetching tail MP4 moov atom ({} KB)", moov_size / 1024));
            log_torrent(&format!(
                "Pre-fetching exact tail 'moov' atom at {} ({} KB)...",
                moov_offset, moov_size / 1024
            ));
            let url_moov = stream_url.clone();
            let moov_range_hdr = format!("bytes={}-", moov_offset);
            let _ = tokio::task::spawn_blocking(move || {
                let client = ureq::AgentBuilder::new()
                    .timeout_connect(Duration::from_secs(10))
                    .timeout_read(Duration::from_secs(30))
                    .build();
                if let Ok(resp) = client.get(&url_moov).set("Range", &moov_range_hdr).call() {
                    let mut reader = Read::take(resp.into_reader(), moov_size);
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                }
            }).await;
        }
    } else if let Some(cues_offset) = meta.cues_offset {
        record_stream_event("Pre-fetching MKV Cues index...");
        log_torrent(&format!("Pre-fetching MKV Cues index at {} in background...", cues_offset));
        let url_cues = stream_url.clone();
        let cues_range_hdr = format!("bytes={}-", cues_offset);
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                let client = ureq::AgentBuilder::new()
                    .timeout_connect(Duration::from_secs(10))
                    .timeout_read(Duration::from_secs(30))
                    .build();
                if let Ok(resp) = client.get(&url_cues).set("Range", &cues_range_hdr).call() {
                    let mut reader = Read::take(resp.into_reader(), 500_000);
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                }
            }).await;
        });
    } else {
        // Fallback for MP4 files where moov was not in the first 256KB: prefetch last 3 MB
        let is_mp4 = file_name.to_lowercase().ends_with(".mp4") || file_name.to_lowercase().ends_with(".m4v");
        if is_mp4 && file_len > 3_000_000 {
            let tail_moov_start = file_len - 3_000_000;
            let url_tail_moov = stream_url.clone();
            let tail_moov_hdr = format!("bytes={}-", tail_moov_start);
            record_stream_event("Pre-fetching MP4 tail index (3 MB)...");
            log_torrent(&format!("Pre-fetching MP4 tail index at byte {} (3 MB) for Media Foundation...", tail_moov_start));
            let _ = tokio::task::spawn_blocking(move || {
                let client = ureq::AgentBuilder::new()
                    .timeout_connect(Duration::from_secs(10))
                    .timeout_read(Duration::from_secs(30))
                    .build();
                if let Ok(resp) = client.get(&url_tail_moov).set("Range", &tail_moov_hdr).call() {
                    let mut reader = Read::take(resp.into_reader(), 3_000_000);
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                }
            }).await;
        }
    }

    // Buffer initial playback segment (ensuring full moov atom and initial frames are cached)
    let is_1080p = file_name.to_lowercase().contains("1080p") || file_name.to_lowercase().contains("fhd");
    let is_mp4 = file_name.to_lowercase().ends_with(".mp4") || file_name.to_lowercase().ends_with(".m4v");

    let moov_head_needed = if is_mp4 {
        if let Some((offset, size)) = meta.moov_range {
            if offset < 20_000_000 {
                // moov is in the first 20 MB (fast-start MP4) -> ensure moov + 1MB of video payload is downloaded
                offset + size + 1_048_576
            } else {
                0
            }
        } else {
            0
        }
    } else {
        0
    };

    let base_min = if is_1080p { 2_621_440u64 } else { 1_572_864u64 };
    let init_play_size = (moov_head_needed.max(base_min) as usize).min(file_len as usize);
    let target_mb = init_play_size as f64 / 1_048_576.0;

    {
        record_stream_event(&format!("Buffering initial segment ({:.1} MB)...", target_mb));
        let mut guard = STREAM_STATUS.lock().unwrap();
        guard.stage = 5;
        guard.progress_ratio = 0.50;
        guard.target_mb = target_mb;
        guard.message = format!("Buffering initial segment (0.0/{:.1} MB)...", target_mb);
    }

    let url_play = stream_url.clone();
    let play_range_hdr = "bytes=0-".to_string();
    let t_buf_start = std::time::Instant::now();
    let _ = tokio::task::spawn_blocking(move || {
        let client = ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(10)).build();
        if let Ok(resp) = client.get(&url_play).set("Range", &play_range_hdr).call() {
            let mut reader = Read::take(resp.into_reader(), init_play_size as u64);
            let mut chunk = [0u8; 65536];
            let mut total_read = 0usize;
            while total_read < init_play_size {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        total_read += n;
                        let frac = (total_read as f32 / init_play_size as f32).clamp(0.0, 1.0);
                        let weighted_ratio = 0.50 + frac * 0.40;
                        let elapsed = t_buf_start.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.05 {
                            (total_read as f64 / 1024.0) / elapsed
                        } else {
                            0.0
                        };
                        let remaining_bytes = init_play_size.saturating_sub(total_read);
                        let eta_secs = if speed > 10.0 {
                            (remaining_bytes as f64 / (speed * 1024.0)).ceil() as u64
                        } else {
                            0
                        };

                        let mut status = STREAM_STATUS.lock().unwrap();
                        status.progress_mb = total_read as f64 / 1_048_576.0;
                        status.target_mb = target_mb;
                        status.speed_kbs = speed;
                        status.eta_secs = eta_secs;
                        status.progress_ratio = weighted_ratio;
                        status.message = if eta_secs > 0 {
                            format!(
                                "Buffering segment: {:.1}/{:.1} MB (ETA ~{}s @ {:.0} KB/s)",
                                status.progress_mb, target_mb, eta_secs, speed
                            )
                        } else {
                            format!(
                                "Buffering segment: {:.1}/{:.1} MB (@ {:.0} KB/s)",
                                status.progress_mb, target_mb, speed
                            )
                        };
                    }
                    Err(_) => break,
                }
            }
        }
    }).await;

    let ttfb_ms = ttfb_timer.elapsed().as_millis();
    log_torrent(&format!(
        "[FAST_START] TTFB: {} ms | Stream ready for Media Foundation in {} ms total",
        ttfb_ms,
        t_fast_start.elapsed().as_millis()
    ));

    // Mark stage 6: Starting player (covers silent decoder startup gap until first frame)
    {
        record_stream_event("Pre-buffering complete. Opening Media Foundation decoder...");
        let mut guard = STREAM_STATUS.lock().unwrap();
        guard.stage = 6;
        guard.progress_ratio = 0.95;
        guard.message = "Opening Media Foundation decoder & DXVA2 hardware surface...".to_string();
    }

    // Return the local remux/proxy URL always ending in .mp4 for Windows RT Media Foundation MPEG4ByteStreamHandler
    let stream_name = if is_mkv { "mkv_stream.mp4" } else { "mp4_stream.mp4" };
    let proxy_url = format!("http://127.0.0.1:3031/play/{}/{}/{}", torrent_id, chosen_idx, stream_name);
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
    season: Option<u32>,
    episode: Option<u32>,
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
        season,
        episode,
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
    season: Option<u32>,
    episode: Option<u32>,
    trackers: Vec<String>,
) -> Result<String, String> {
    start_stream(info_hash, file_idx, season, episode, trackers)
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

/// Matches series episodes using SxxExx and NxN patterns against video files,
/// falling back to the largest video file.
pub fn guess_file_index(
    files: &[(usize, u64, String)],
    series_info: Option<(u32, u32)>,
) -> usize {
    let video_extensions = [
        ".mp4", ".mkv", ".avi", ".m4v", ".mov", ".ts", ".webm", ".wmv",
    ];

    let mut media_files: Vec<(usize, u64, String)> = Vec::new();
    for (idx, length, filename) in files {
        let lower = filename.to_lowercase();
        if video_extensions.iter().any(|ext| lower.ends_with(ext)) {
            media_files.push((*idx, *length, lower));
        }
    }

    if media_files.is_empty() {
        return files.iter().max_by_key(|(_, len, _)| *len).map(|(idx, _, _)| *idx).unwrap_or(0);
    }

    if let Some((season, episode)) = series_info {
        let re_sxe = regex::Regex::new(r"[sS](\d+)[eE](\d+)").unwrap();
        let re_xx = regex::Regex::new(r"(\d+)x(\d+)").unwrap();

        let mut candidates = Vec::new();
        for (idx, length, filename) in &media_files {
            let mut found_s = None;
            let mut found_e = None;

            if let Some(caps) = re_sxe.captures(filename) {
                found_s = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
                found_e = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
            } else if let Some(caps) = re_xx.captures(filename) {
                found_s = caps.get(1).and_then(|m| m.as_str().parse::<u32>().ok());
                found_e = caps.get(2).and_then(|m| m.as_str().parse::<u32>().ok());
            }

            if found_s == Some(season) && found_e == Some(episode) {
                candidates.push((*idx, *length));
            }
        }

        if let Some((best_idx, _)) = candidates.into_iter().max_by_key(|&(_, len)| len) {
            return best_idx;
        }
    }

    media_files
        .into_iter()
        .max_by_key(|&(_, len, _)| len)
        .map(|(idx, _, _)| idx)
        .unwrap_or(0)
}
