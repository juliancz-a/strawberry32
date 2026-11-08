use crate::logger::{log_err, log_info};
use crate::state::get_app_storage_root;
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;

const TRACKERS_BEST_URL: &str =
    "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt";
const CACHE_VALIDITY_SECS: u64 = 24 * 60 * 60; // 24 hours
const UDP_PROBE_TIMEOUT: Duration = Duration::from_millis(1000); // 1s
const HTTP_PROBE_TIMEOUT: Duration = Duration::from_millis(2000); // 2s

/// Curated baseline of high-uptime live trackers (used when offline or before initial fetch)
const CURATED_BASELINE_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://explodie.org:6969/announce",
    "udp://tracker.filemail.com:6969/announce",
    "udp://p4p.arenabg.com:1337/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "http://tracker.opentrackr.org:1337/announce",
];

#[derive(Serialize, Deserialize)]
struct TrackerCache {
    timestamp: u64,
    trackers: Vec<String>,
}

static LIVE_TRACKERS: OnceLock<Arc<RwLock<Vec<String>>>> = OnceLock::new();

fn get_trackers_rw() -> &'static Arc<RwLock<Vec<String>>> {
    LIVE_TRACKERS.get_or_init(|| {
        let initial: Vec<String> = CURATED_BASELINE_TRACKERS.iter().map(|s| s.to_string()).collect();
        Arc::new(RwLock::new(initial))
    })
}

fn get_cache_path() -> PathBuf {
    get_app_storage_root().join("tracker_cache.json")
}

/// Returns the currently active, verified fast trackers.
pub fn get_verified_trackers() -> Vec<String> {
    let rw = get_trackers_rw();
    rw.read().unwrap().clone()
}

/// Initializes tracker manager: loads cache if <24h old, or spawns background prober to refresh.
pub fn init_tracker_manager() {
    let cache_path = get_cache_path();
    let mut needs_refresh = true;

    if cache_path.is_file() {
        if let Ok(data) = fs::read_to_string(&cache_path) {
            if let Ok(cache) = serde_json::from_str::<TrackerCache>(&data) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                if now.saturating_sub(cache.timestamp) < CACHE_VALIDITY_SECS && !cache.trackers.is_empty() {
                    log_info(&format!(
                        "Loaded {} live trackers from cache (age: {}m)",
                        cache.trackers.len(),
                        (now - cache.timestamp) / 60
                    ));
                    *get_trackers_rw().write().unwrap() = cache.trackers;
                    needs_refresh = false;
                }
            }
        }
    }

    if needs_refresh {
        log_info("Tracker cache expired or missing. Spawning background tracker prober...");
        std::thread::Builder::new()
            .name("tracker-prober".to_string())
            .spawn(|| {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log_err(&format!("Tracker prober runtime failed: {}", e));
                        return;
                    }
                };

                rt.block_on(async {
                    refresh_and_probe_trackers().await;
                });
            })
            .expect("Failed to spawn tracker-prober thread");
    }
}

/// Fetches tracker list, probes each UDP/HTTP endpoint, sorts by lowest RTT, and caches top 20.
pub async fn refresh_and_probe_trackers() {
    let raw_list = match fetch_trackers_list().await {
        Ok(list) if !list.is_empty() => list,
        Err(e) => {
            log_err(&format!("Failed to fetch live tracker list: {}. Using curated baseline.", e));
            CURATED_BASELINE_TRACKERS.iter().map(|s| s.to_string()).collect()
        }
        _ => CURATED_BASELINE_TRACKERS.iter().map(|s| s.to_string()).collect(),
    };

    let ranked = rank_trackers(raw_list).await;
    if !ranked.is_empty() {
        let top_20: Vec<String> = ranked.into_iter().take(20).collect();
        log_info(&format!("Probe complete. Top {} fast trackers selected.", top_20.len()));

        // Save to cache
        let cache = TrackerCache {
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            trackers: top_20.clone(),
        };

        if let Ok(json) = serde_json::to_string_pretty(&cache) {
            let _ = fs::write(get_cache_path(), json);
        }

        // Update in-memory state
        *get_trackers_rw().write().unwrap() = top_20;
    }
}

async fn fetch_trackers_list() -> Result<Vec<String>, String> {
    tokio::task::spawn_blocking(|| {
        let agent = crate::addons::get_shared_agent();
        let resp = agent.get(TRACKERS_BEST_URL)
            .timeout(Duration::from_secs(5))
            .call()
            .map_err(|e| e.to_string())?;

        let body = resp.into_string().map_err(|e| e.to_string())?;
        let trackers: Vec<String> = body
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && (l.starts_with("udp://") || l.starts_with("http://") || l.starts_with("https://")))
            .collect();
        Ok(trackers)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Probes all trackers concurrently, measuring round-trip time (RTT).
async fn rank_trackers(trackers: Vec<String>) -> Vec<String> {
    let mut handles = Vec::new();

    for tracker in trackers {
        handles.push(tokio::spawn(async move {
            let rtt = probe_tracker(&tracker).await;
            (tracker, rtt)
        }));
    }

    let mut responsive = Vec::new();
    for h in handles {
        if let Ok((tracker, Some(rtt))) = h.await {
            responsive.push((tracker, rtt));
        }
    }

    responsive.sort_by_key(|(_, rtt)| *rtt);
    responsive.into_iter().map(|(url, _)| url).collect()
}

async fn probe_tracker(url_str: &str) -> Option<Duration> {
    if let Some(stripped) = url_str.strip_prefix("udp://") {
        probe_udp(stripped).await
    } else if url_str.starts_with("http://") || url_str.starts_with("https://") {
        probe_http(url_str).await
    } else {
        None
    }
}

/// BitTorrent UDP Tracker Connect Handshake (BEP 15)
/// Magic Constant: 0x41727101980 (64-bit int)
/// Action: 0 (connect, 32-bit int)
/// Transaction ID: 12345 (32-bit int)
async fn probe_udp(host_and_path: &str) -> Option<Duration> {
    let host_port = host_and_path.split('/').next()?;
    
    // Resolve DNS in background blocking pool to prevent blocking tokio executor
    let host_port_string = host_port.to_string();
    let addr = tokio::task::spawn_blocking(move || {
        host_port_string.to_socket_addrs().ok().and_then(|mut iter| iter.next())
    })
    .await
    .ok()??;

    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    socket.connect(addr).await.ok()?;

    let protocol_id: u64 = 0x41727101980;
    let action: u32 = 0;
    let transaction_id: u32 = 12345;

    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&protocol_id.to_be_bytes());
    buf[8..12].copy_from_slice(&action.to_be_bytes());
    buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());

    let start = Instant::now();
    socket.send(&buf).await.ok()?;

    let mut recv_buf = [0u8; 16];
    let timeout_res = tokio::time::timeout(UDP_PROBE_TIMEOUT, socket.recv(&mut recv_buf)).await;

    match timeout_res {
        Ok(Ok(n)) if n >= 8 => {
            let recv_action = u32::from_be_bytes(recv_buf[0..4].try_into().unwrap());
            let recv_trans = u32::from_be_bytes(recv_buf[4..8].try_into().unwrap());
            if recv_action == 0 && recv_trans == transaction_id {
                Some(start.elapsed())
            } else {
                None
            }
        }
        _ => None,
    }
}

async fn probe_http(url: &str) -> Option<Duration> {
    let url_owned = url.to_string();
    tokio::task::spawn_blocking(move || {
        let agent = crate::addons::get_shared_agent();
        let start = Instant::now();
        // Send a quick HEAD or GET request with 2s timeout
        let req = agent.get(&url_owned).timeout(HTTP_PROBE_TIMEOUT);
        match req.call() {
            Ok(_) => Some(start.elapsed()),
            // Many trackers return 400 or 404 for plain HTTP without info_hash, which still confirms it's alive!
            Err(ureq::Error::Status(code, _)) if code < 500 => Some(start.elapsed()),
            Err(_) => None,
        }
    })
    .await
    .ok()?
}

