use crate::logger::{log_err, log_info};
use crate::state;
use image::imageops::FilterType;
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

const MAX_IMAGE_CACHE_BYTES: usize = 48 * 1024 * 1024; // 48 MB RAM budget per spec

/// Decoded image entry in RAM storing thread-safe SharedPixelBuffer
struct CachedImage {
    pixel_buffer: SharedPixelBuffer<Rgba8Pixel>,
    byte_size: usize,
    last_access: u64,
}

static RAM_CACHE: Mutex<Option<HashMap<String, CachedImage>>> = Mutex::new(None);
static RAM_CACHE_BYTES: Mutex<usize> = Mutex::new(0);
static ACCESS_COUNTER: Mutex<u64> = Mutex::new(0);

static DOWNLOAD_QUEUE: OnceLock<Sender<ImageRequest>> = OnceLock::new();
static GENERATION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct PerfStats {
    total_images: u64,
    total_dl_ms: u64,
    total_dec_ms: u64,
    total_res_ms: u64,
}
static PERF_STATS: Mutex<PerfStats> = Mutex::new(PerfStats {
    total_images: 0,
    total_dl_ms: 0,
    total_dec_ms: 0,
    total_res_ms: 0,
});

fn record_perf_stat(dl_ms: u64, dec_ms: u64, res_ms: u64) {
    let mut s = PERF_STATS.lock().unwrap();
    s.total_images += 1;
    s.total_dl_ms += dl_ms;
    s.total_dec_ms += dec_ms;
    s.total_res_ms += res_ms;
}

pub fn get_perf_summary() -> String {
    let s = PERF_STATS.lock().unwrap();
    if s.total_images == 0 {
        return "Images: 0".to_string();
    }
    format!(
        "N: {} | DL: {}ms | Dec: {}ms | Res: {}ms",
        s.total_images,
        s.total_dl_ms / s.total_images,
        s.total_dec_ms / s.total_images,
        s.total_res_ms / s.total_images,
    )
}

pub fn bump_generation() -> u64 {
    GENERATION_ID.fetch_add(1, Ordering::SeqCst) + 1
}

pub fn get_generation() -> u64 {
    GENERATION_ID.load(Ordering::SeqCst)
}

struct ImageRequest {
    url: String,
    target_width: u32,
    target_height: u32,
    gen_id: u64,
    callback: Box<dyn FnOnce(Option<SharedPixelBuffer<Rgba8Pixel>>) + Send + 'static>,
}

fn fnv1a_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Returns disk cache directory: <app_root>\cache\images\
pub fn get_image_cache_dir() -> PathBuf {
    let dir = state::get_app_storage_root().join("cache").join("images");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Returns the current RAM consumption of the decoded image cache in megabytes
pub fn get_ram_cache_usage_mb() -> f32 {
    let bytes = *RAM_CACHE_BYTES.lock().unwrap();
    bytes as f32 / (1024.0 * 1024.0)
}

/// Initializes the 2-worker image pipeline background threads
pub fn init() {
    if DOWNLOAD_QUEUE.get().is_some() {
        return;
    }

    let mut cache_guard = RAM_CACHE.lock().unwrap();
    if cache_guard.is_none() {
        *cache_guard = Some(HashMap::new());
    }
    drop(cache_guard);

    let (tx, rx) = channel::<ImageRequest>();
    let rx = Arc::new(Mutex::new(rx));

    // Spawn 2 background worker threads for non-blocking download & decoding
    for worker_id in 0..2 {
        let rx_clone = Arc::clone(&rx);
        thread::Builder::new()
            .name(format!("image-worker-{}", worker_id))
            .spawn(move || {
                log_info(&format!("Image worker {} started", worker_id));
                loop {
                    let req = {
                        let guard = rx_clone.lock().unwrap();
                        match guard.recv() {
                            Ok(r) => r,
                            Err(_) => break, // channel closed
                        }
                    };

                    // Skip cancelled/stale requests when page changed
                    if req.gen_id < get_generation() {
                        continue;
                    }

                    let opt_buf = process_image_request(&req.url, req.target_width, req.target_height);
                    (req.callback)(opt_buf);
                }
            })
            .expect("Failed to spawn image worker");
    }

    let _ = DOWNLOAD_QUEUE.set(tx);
    log_info("Image pipeline initialized with 2 worker threads");
}

fn process_image_request(url: &str, target_w: u32, target_h: u32) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    // 0. Server-side downscale hint: replace /poster/medium/ or /poster/large/ with /poster/small/
    let effective_url = if url.contains("images.metahub.space/poster/medium/") {
        url.replace("images.metahub.space/poster/medium/", "images.metahub.space/poster/small/")
    } else if url.contains("images.metahub.space/poster/large/") {
        url.replace("images.metahub.space/poster/large/", "images.metahub.space/poster/small/")
    } else {
        url.to_string()
    };

    // 1. Check RAM cache first
    let cache_key = format!("v2_{}_{}x{}", fnv1a_hash(&effective_url), target_w, target_h);
    {
        let mut guard = RAM_CACHE.lock().unwrap();
        if let Some(cache) = guard.as_mut() {
            if let Some(entry) = cache.get_mut(&cache_key) {
                let mut acc = ACCESS_COUNTER.lock().unwrap();
                *acc += 1;
                entry.last_access = *acc;
                return Some(entry.pixel_buffer.clone());
            }
        }
    }

    // 2. Check disk cache
    let disk_dir = get_image_cache_dir();
    let disk_path = disk_dir.join(format!("{}.raw", cache_key));

    let pixel_buffer = if disk_path.is_file() {
        if let Ok(bytes) = fs::read(&disk_path) {
            let expected_len = (target_w * target_h * 4) as usize;
            if bytes.len() == expected_len {
                let mut pb = SharedPixelBuffer::<Rgba8Pixel>::new(target_w, target_h);
                pb.make_mut_bytes().copy_from_slice(&bytes);
                Some(pb)
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let pixel_buffer = match pixel_buffer {
        Some(pb) => pb,
        None => {
            // 3. Download image via shared pooled ureq agent (reusing Schannel TLS sessions)
            let agent = crate::addons::get_shared_agent();

            let t_dl = std::time::Instant::now();
            let resp = match agent.get(&effective_url).call() {
                Ok(r) => r,
                Err(e) => {
                    log_err(&format!("Failed to fetch image '{}': {}", effective_url, e));
                    return None;
                }
            };

            let mut reader = resp.into_reader().take(5 * 1024 * 1024); // 5 MB max per poster
            let mut raw_encoded = Vec::new();
            if let Err(e) = std::io::copy(&mut reader, &mut raw_encoded) {
                log_err(&format!("Failed to read image bytes for '{}': {}", effective_url, e));
                return None;
            }
            let dl_ms = t_dl.elapsed().as_millis() as u64;

            // 4. Decode with pure-Rust image crate off the UI thread
            let t_dec = std::time::Instant::now();
            let dyn_img = match image::load_from_memory(&raw_encoded) {
                Ok(img) => img,
                Err(e) => {
                    log_err(&format!("Image decode error for '{}': {}", effective_url, e));
                    return None;
                }
            };
            let dec_ms = t_dec.elapsed().as_millis() as u64;

            // 5. Downsample using fast Triangle (bilinear) filter (fast on Cortex-A9 Tegra 3)
            let t_res = std::time::Instant::now();
            let resized = dyn_img.resize_exact(target_w, target_h, FilterType::Triangle);
            let mut rgba = resized.to_rgba8();

            // Pre-bake 6px rounded-corner alpha mask directly into raw RGBA bytes
            // Eliminates per-pixel software clipping in Slint software renderer
            let radius = 6.0f32;
            let (w_f, h_f) = (target_w as f32, target_h as f32);
            if target_w > 12 && target_h > 12 {
                for y in 0..target_h {
                    let y_f = y as f32 + 0.5;
                    for x in 0..target_w {
                        let x_f = x as f32 + 0.5;
                        let mut corner_dist = None;
                        if x_f < radius && y_f < radius {
                            // Top-left
                            let dx = radius - x_f;
                            let dy = radius - y_f;
                            corner_dist = Some((dx * dx + dy * dy).sqrt());
                        } else if x_f > w_f - radius && y_f < radius {
                            // Top-right
                            let dx = x_f - (w_f - radius);
                            let dy = radius - y_f;
                            corner_dist = Some((dx * dx + dy * dy).sqrt());
                        } else if x_f < radius && y_f > h_f - radius {
                            // Bottom-left
                            let dx = radius - x_f;
                            let dy = y_f - (h_f - radius);
                            corner_dist = Some((dx * dx + dy * dy).sqrt());
                        } else if x_f > w_f - radius && y_f > h_f - radius {
                            // Bottom-right
                            let dx = x_f - (w_f - radius);
                            let dy = y_f - (h_f - radius);
                            corner_dist = Some((dx * dx + dy * dy).sqrt());
                        }

                        if let Some(dist) = corner_dist {
                            if dist > radius {
                                let p = rgba.get_pixel_mut(x, y);
                                p[3] = 0;
                            } else if dist > radius - 1.0 {
                                let alpha_factor = (radius - dist).clamp(0.0, 1.0);
                                let p = rgba.get_pixel_mut(x, y);
                                p[3] = ((p[3] as f32) * alpha_factor) as u8;
                            }
                        }
                    }
                }
            }

            let raw_rgba_bytes = rgba.into_raw();
            let res_ms = t_res.elapsed().as_millis() as u64;

            record_perf_stat(dl_ms, dec_ms, res_ms);
            log_info(&format!(
                "[IMG PERF] dl: {}ms | dec: {}ms | res: {}ms | raw: {} KB",
                dl_ms, dec_ms, res_ms, raw_encoded.len() / 1024
            ));

            // Persist resized raw RGBA to disk
            let _ = fs::write(&disk_path, &raw_rgba_bytes);

            let mut pb = SharedPixelBuffer::<Rgba8Pixel>::new(target_w, target_h);
            pb.make_mut_bytes().copy_from_slice(&raw_rgba_bytes);
            pb
        }
    };

    let byte_size = (target_w * target_h * 4) as usize;

    // 6. Insert into RAM cache and evict if exceeding 48 MB limit
    {
        let mut guard = RAM_CACHE.lock().unwrap();
        let mut bytes_guard = RAM_CACHE_BYTES.lock().unwrap();
        let mut acc = ACCESS_COUNTER.lock().unwrap();
        *acc += 1;

        if let Some(cache) = guard.as_mut() {
            while *bytes_guard + byte_size > MAX_IMAGE_CACHE_BYTES && !cache.is_empty() {
                let oldest_key = cache
                    .iter()
                    .min_by_key(|(_, v)| v.last_access)
                    .map(|(k, _)| k.clone());

                if let Some(k) = oldest_key {
                    if let Some(removed) = cache.remove(&k) {
                        *bytes_guard = bytes_guard.saturating_sub(removed.byte_size);
                    }
                } else {
                    break;
                }
            }

            cache.insert(
                cache_key,
                CachedImage {
                    pixel_buffer: pixel_buffer.clone(),
                    byte_size,
                    last_access: *acc,
                },
            );
            *bytes_guard += byte_size;
        }
    }

    Some(pixel_buffer)
}

/// Request an image asynchronously; delivers slint::Image via invoke_from_event_loop
pub fn request_image<F>(url: String, target_width: u32, target_height: u32, callback: F)
where
    F: FnOnce(Option<Image>) + Send + 'static,
{
    init();

    // Check RAM cache synchronously first for zero latency
    let effective_url = if url.contains("images.metahub.space/poster/medium/") {
        url.replace("images.metahub.space/poster/medium/", "images.metahub.space/poster/small/")
    } else if url.contains("images.metahub.space/poster/large/") {
        url.replace("images.metahub.space/poster/large/", "images.metahub.space/poster/small/")
    } else {
        url
    };

    let cache_key = format!("v2_{}_{}x{}", fnv1a_hash(&effective_url), target_width, target_height);
    {
        let mut guard = RAM_CACHE.lock().unwrap();
        if let Some(cache) = guard.as_mut() {
            if let Some(entry) = cache.get_mut(&cache_key) {
                let mut acc = ACCESS_COUNTER.lock().unwrap();
                *acc += 1;
                entry.last_access = *acc;
                let pb = entry.pixel_buffer.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    callback(Some(Image::from_rgba8(pb)));
                });
                return;
            }
        }
    }

    // Otherwise dispatch to background workers
    if let Some(tx) = DOWNLOAD_QUEUE.get() {
        let gen_id = get_generation();
        let _ = tx.send(ImageRequest {
            url: effective_url,
            target_width,
            target_height,
            gen_id,
            callback: Box::new(move |opt_pb| {
                let _ = slint::invoke_from_event_loop(move || {
                    let opt_img = opt_pb.map(Image::from_rgba8);
                    callback(opt_img);
                });
            }),
        });
    }
}
