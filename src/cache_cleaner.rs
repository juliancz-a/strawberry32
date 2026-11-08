use crate::logger::{log_err, log_info};
use crate::state::{get_app_storage_root, load_settings};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

static ACTIVE_TORRENT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Marks the current active streaming torrent folder as protected from cache eviction.
pub fn set_active_torrent_dir(dir: Option<PathBuf>) {
    let mut guard = ACTIVE_TORRENT_DIR.lock().unwrap();
    *guard = dir;
}

/// Starts the periodic background cache cleaner task.
pub fn start_cache_cleaner() {
    std::thread::Builder::new()
        .name("cache-cleaner".to_string())
        .spawn(|| {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    log_err(&format!("Cache cleaner runtime failed: {}", e));
                    return;
                }
            };

            rt.block_on(async {
                log_info("Protected LRU cache cleaner service started.");
                // Initial cleanup run shortly after startup (30s delay)
                tokio::time::sleep(Duration::from_secs(30)).await;
                clean_disk_cache();

                // Periodic check every 30 minutes
                let mut interval = tokio::time::interval(Duration::from_secs(30 * 60));
                loop {
                    interval.tick().await;
                    clean_disk_cache();
                }
            });
        })
        .expect("Failed to spawn cache cleaner thread");
}

/// Runs a single pass of the cache cleaner:
/// 1. Protects the currently streaming torrent folder.
/// 2. Deletes cache files older than 30 days.
/// 3. Evicts oldest-modified files if total cache exceeds the configured max limit (default 4 GB).
pub fn clean_disk_cache() {
    let settings = load_settings();
    let max_cache_bytes = if settings.cache.max_bytes > 0 {
        settings.cache.max_bytes
    } else {
        4 * 1024 * 1024 * 1024
    };

    let torrent_cache_dir = crate::engine::get_cache_dir();
    let image_cache_dir = get_app_storage_root().join("image_cache");

    let active_dir = ACTIVE_TORRENT_DIR.lock().unwrap().clone();
    let mut protected_paths = HashSet::new();
    if let Some(dir) = active_dir {
        protected_paths.insert(dir);
    }

    let thirty_days = Duration::from_secs(30 * 24 * 60 * 60);
    let now = SystemTime::now();

    let mut candidate_files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total_cache_bytes = 0u64;

    for dir in &[&torrent_cache_dir, &image_cache_dir] {
        if !dir.is_dir() {
            continue;
        }

        collect_files_recursive(dir, &protected_paths, &mut candidate_files, &mut total_cache_bytes, now, thirty_days);
    }

    // Size-based eviction if cache exceeds configured budget
    if total_cache_bytes > max_cache_bytes {
        log_info(&format!(
            "Cache size ({:.1} MB) exceeds limit ({:.1} MB). Evicting oldest files...",
            total_cache_bytes as f64 / 1_048_576.0,
            max_cache_bytes as f64 / 1_048_576.0
        ));

        // Sort by last modified (oldest first)
        candidate_files.sort_by_key(|(_, _, modified)| *modified);

        let mut freed_bytes = 0u64;
        let mut deleted_count = 0usize;

        for (path, size, _) in candidate_files {
            if total_cache_bytes <= max_cache_bytes {
                break;
            }

            if let Ok(()) = fs::remove_file(&path) {
                total_cache_bytes = total_cache_bytes.saturating_sub(size);
                freed_bytes += size;
                deleted_count += 1;

                if let Some(parent) = path.parent() {
                    remove_empty_parents(parent, &torrent_cache_dir);
                }
            }
        }

        log_info(&format!(
            "Evicted {} files, freed {:.1} MB. New cache size: {:.1} MB",
            deleted_count,
            freed_bytes as f64 / 1_048_576.0,
            total_cache_bytes as f64 / 1_048_576.0
        ));
    }
}

fn collect_files_recursive(
    dir: &Path,
    protected_paths: &HashSet<PathBuf>,
    candidates: &mut Vec<(PathBuf, u64, SystemTime)>,
    total_size: &mut u64,
    now: SystemTime,
    max_age: Duration,
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Is folder protected?
            let is_protected = protected_paths.iter().any(|p| path.starts_with(p) || p.starts_with(&path));
            if !is_protected {
                collect_files_recursive(&path, protected_paths, candidates, total_size, now, max_age);
            }
        } else if path.is_file() {
            let is_protected = protected_paths.iter().any(|p| path.starts_with(p));
            if is_protected {
                if let Ok(meta) = entry.metadata() {
                    *total_size += meta.len();
                }
                continue;
            }

            if let Ok(meta) = entry.metadata() {
                let size = meta.len();
                let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);

                // Check 30-day age expiration
                let age = now.duration_since(modified).unwrap_or_default();
                if age > max_age {
                    let _ = fs::remove_file(&path);
                    continue;
                }

                *total_size += size;
                candidates.push((path, size, modified));
            }
        }
    }
}

fn remove_empty_parents(mut dir: &Path, root: &Path) {
    while dir != root && dir.starts_with(root) {
        if fs::remove_dir(dir).is_err() {
            break;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
}
