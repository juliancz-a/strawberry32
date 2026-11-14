use crate::logger::{log_err, log_player, log_win32_err};
use crate::subtitles;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, FALSE, HWND};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW,
};

pub type HRESULT = i32;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GUID {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const GUID_NULL: GUID = GUID {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: [0; 8],
};

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(non_snake_case)]
pub struct PROPVARIANT {
    pub vt: u16,
    pub wReserved1: u16,
    pub wReserved2: u16,
    pub wReserved3: u16,
    pub hVal: i64,
    pub _padding: [u8; 8], // Full 24-byte layout for 32-bit Windows PROPVARIANT
}

impl Default for PROPVARIANT {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[link(name = "ole32")]
extern "system" {
    fn CoInitializeEx(pvReserved: *mut c_void, dwCoInit: u32) -> HRESULT;
}

#[link(name = "kernel32")]
extern "system" {
    fn SetThreadExecutionState(esFlags: u32) -> u32;
}

const ES_CONTINUOUS: u32 = 0x80000000;
const ES_DISPLAY_REQUIRED: u32 = 0x00000002;
const ES_SYSTEM_REQUIRED: u32 = 0x00000001;

/// Prevents the Surface RT screen from sleeping / blanking during playback
pub fn prevent_sleep(enable: bool) {
    unsafe {
        if enable {
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            log_player("System sleep prevention activated (ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED).");
        } else {
            SetThreadExecutionState(ES_CONTINUOUS);
            log_player("System sleep prevention cleared.");
        }
    }
}

#[allow(non_snake_case)]
type MFPCreateMediaPlayerFn = unsafe extern "system" fn(
    pwszURL: *const u16,
    fStartPlayback: i32,
    creationOptions: u32,
    pCallback: *mut c_void,
    hWnd: HWND,
    ppMediaPlayer: *mut *mut c_void,
) -> HRESULT;

type MFStartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;

// Generic COM vtable invocation helpers
unsafe fn com_call_0(this: *mut c_void, idx: usize) -> HRESULT {
    let vtable = *(this as *mut *const usize);
    let func: unsafe extern "system" fn(*mut c_void) -> HRESULT = std::mem::transmute(*vtable.add(idx));
    func(this)
}

unsafe fn com_call_1<A>(this: *mut c_void, idx: usize, a: A) -> HRESULT {
    let vtable = *(this as *mut *const usize);
    let func: unsafe extern "system" fn(*mut c_void, A) -> HRESULT = std::mem::transmute(*vtable.add(idx));
    func(this, a)
}

unsafe fn com_call_2<A, B>(this: *mut c_void, idx: usize, a: A, b: B) -> HRESULT {
    let vtable = *(this as *mut *const usize);
    let func: unsafe extern "system" fn(*mut c_void, A, B) -> HRESULT = std::mem::transmute(*vtable.add(idx));
    func(this, a, b)
}


#[allow(dead_code)]
struct EmbeddedPlayerState {
    p_player: *mut c_void,
    video_hwnd: HWND,
    is_paused: bool,
    is_fullscreen: bool,
}

unsafe impl Send for EmbeddedPlayerState {}
unsafe impl Sync for EmbeddedPlayerState {}

static EMBEDDED_PLAYER: Mutex<Option<EmbeddedPlayerState>> = Mutex::new(None);
static CURRENT_STREAM_URL: Mutex<String> = Mutex::new(String::new());
static IS_MKV_STREAM: AtomicBool = AtomicBool::new(false);
static KNOWN_DURATION_MS: AtomicU64 = AtomicU64::new(0);
static SEEK_BASE_OFFSET_MS: AtomicU64 = AtomicU64::new(0);
static MF_INITIALIZED: Mutex<bool> = Mutex::new(false);

pub fn refresh_video_window() {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.video_hwnd.is_null() {
                windows_sys::Win32::Graphics::Gdi::InvalidateRect(state.video_hwnd, std::ptr::null(), 0);
                windows_sys::Win32::Graphics::Gdi::UpdateWindow(state.video_hwnd);
            }
            if !state.p_player.is_null() {
                let _ = com_call_0(state.p_player, 32); // IMFPMediaPlayer::UpdateVideo
            }
        }
    }
}

pub fn set_stream_container_info(is_mkv: bool, duration_ms: u64) {
    IS_MKV_STREAM.store(is_mkv, Ordering::SeqCst);
    if duration_ms > 0 {
        KNOWN_DURATION_MS.store(duration_ms, Ordering::SeqCst);
    }
}

pub fn set_known_duration_ms(duration_ms: u64) {
    if duration_ms > 0 {
        KNOWN_DURATION_MS.store(duration_ms, Ordering::SeqCst);
    }
}

pub fn set_seek_base_offset_ms(offset_ms: u64) {
    SEEK_BASE_OFFSET_MS.store(offset_ms, Ordering::SeqCst);
}

fn is_jellyfin_transcode_url(url: &str) -> bool {
    url.contains("/Videos/") && url.contains("MediaSourceId=") && url.contains("VideoCodec=")
}

fn rebuild_jellyfin_seek_url(url: &str, start_ticks: u64) -> String {
    let mut base = url.to_string();
    if let Some(pos) = base.find("&StartTimeTicks=") {
        let rest = &base[pos + 1..];
        let end = rest.find('&').map(|i| pos + 1 + i).unwrap_or(base.len());
        base.replace_range(pos..end, "");
    }
    if start_ticks > 0 {
        if base.contains('?') {
            format!("{}&StartTimeTicks={}", base, start_ticks)
        } else {
            format!("{}?StartTimeTicks={}", base, start_ticks)
        }
    } else {
        base
    }
}

pub fn ensure_mf_started() {
    let mut guard = MF_INITIALIZED.lock().unwrap();
    if !*guard {
        unsafe {
            let lib = LoadLibraryW(to_wide("mfplat.dll").as_ptr());
            if !lib.is_null() {
                let proc = GetProcAddress(lib, b"MFStartup\0".as_ptr());
                if let Some(mf_startup) = proc {
                    let startup_fn: MFStartupFn = std::mem::transmute(mf_startup);
                    let hr = startup_fn(0x00020070, 0); // MF_VERSION = 0x00020070
                    log_player(&format!("MFStartup initialized: hr = 0x{:08X}", hr as u32));
                }
            }
        }
        *guard = true;
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Initializes Media Foundation player inside the provided video_hwnd
fn init_media_foundation_player(video_hwnd: HWND, stream_url: &str) -> Result<*mut c_void, String> {
    unsafe {
        let url_w = to_wide(stream_url);
        let mut p_player: *mut c_void = null_mut();

        let h_mfplay = LoadLibraryW(to_wide("mfplay.dll").as_ptr());
        if h_mfplay.is_null() {
            return Err("Could not load mfplay.dll from system".to_string());
        }

        let proc = match GetProcAddress(h_mfplay, b"MFPCreateMediaPlayer\0".as_ptr()) {
            Some(p) => p,
            None => return Err("MFPCreateMediaPlayer symbol not found in mfplay.dll".to_string()),
        };

        let mfp_create_media_player: MFPCreateMediaPlayerFn = std::mem::transmute(proc);
        log_player(&format!(
            "MFPCreateMediaPlayer: URL = {}, HWND = 0x{:X}",
            stream_url, video_hwnd as usize
        ));

        let hr = mfp_create_media_player(
            url_w.as_ptr(),
            1, // fStartPlayback: Autoplay
            0, // creationOptions: 0
            null_mut(),
            video_hwnd,
            &mut p_player,
        );

        if hr >= 0 && !p_player.is_null() {
            log_player(&format!(
                "Media Foundation player initialized successfully inside HWND 0x{:X}! (p_player = {:p})",
                video_hwnd as usize, p_player
            ));

            // Explicitly call Play (vtable index 3)
            let hr_play = com_call_0(p_player, 3);
            log_player(&format!("IMFPMediaPlayer::Play() result: 0x{:08X}", hr_play as u32));

            Ok(p_player)
        } else {
            Err(format!("MFPCreateMediaPlayer returned 0x{:08X}", hr as u32))
        }
    }
}

/// Initializes and starts the embedded native player inside video_hwnd via Media Foundation.
pub fn init_embedded_player(video_hwnd: HWND, stream_url: &str) -> Result<(), String> {
    stop_embedded_player();
    ensure_mf_started();

    *CURRENT_STREAM_URL.lock().unwrap() = stream_url.to_string();
    let lower_url = stream_url.to_lowercase();
    let is_mkv = lower_url.contains("mkv_stream.mp4")
        || lower_url.contains(".mkv")
        || lower_url.contains(".webm")
        || IS_MKV_STREAM.load(Ordering::SeqCst);
    IS_MKV_STREAM.store(is_mkv, Ordering::SeqCst);
    if !stream_url.contains("start=") && !stream_url.contains("StartTimeTicks=") {
        SEEK_BASE_OFFSET_MS.store(0, Ordering::SeqCst);
    }

    unsafe {
        const S_OK: HRESULT = 0;
        const S_FALSE: HRESULT = 1;
        const RPC_E_CHANGED_MODE: HRESULT = -2147417850; // 0x80010106

        let hr_init = CoInitializeEx(null_mut(), 0x2); // COINIT_APARTMENTTHREADED / MULTITHREADED
        if hr_init != S_OK && hr_init != S_FALSE && hr_init != RPC_E_CHANGED_MODE {
            log_player(&format!("CoInitializeEx notice: hr = 0x{:08X}", hr_init as u32));
        }
    }

    match init_media_foundation_player(video_hwnd, stream_url) {
        Ok(p_player) => {
            prevent_sleep(true);
            let mut guard = EMBEDDED_PLAYER.lock().unwrap();
            *guard = Some(EmbeddedPlayerState {
                p_player,
                video_hwnd,
                is_paused: false,
                is_fullscreen: false,
            });
            Ok(())
        }
        Err(mf_err) => {
            log_err(&format!("Media Foundation player initialization failed: {}", mf_err));
            Err(mf_err)
        }
    }
}

/// Stops the embedded player and cleanly releases all COM resources
pub fn stop_embedded_player() {
    prevent_sleep(false);
    let mut guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.take() {
        unsafe {
            if !state.p_player.is_null() {
                let _ = com_call_0(state.p_player, 5); // Stop
                let _ = com_call_0(state.p_player, 38); // Shutdown
                let _ = com_call_0(state.p_player, 2); // Release
            }
        }
        log_player("Embedded Media Foundation player stopped and resources released.");
    }
}

/// Returns the current player state (0 = Empty, 1 = Stopped, 2 = Playing, 3 = Paused, 4 = Shutdown)
pub fn get_player_state() -> u32 {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        if !state.p_player.is_null() {
            let mut st: u32 = 0;
            let hr = unsafe { com_call_1(state.p_player, 13, &mut st as *mut u32) };
            if hr >= 0 {
                return st;
            }
        }
    }
    0
}

/// Updates the video rectangle when the window resizes
pub fn update_video_position(_left: i32, _top: i32, _width: i32, _height: i32) {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let mut st: u32 = 0;
                let hr_state = com_call_1(state.p_player, 13, &mut st as *mut u32);
                // Only call UpdateVideo if player has resolved and is not Empty (0) or Shutdown (4)
                if hr_state >= 0 && st >= 1 && st <= 3 {
                    let _ = com_call_0(state.p_player, 32); // UpdateVideo
                    if !state.video_hwnd.is_null() {
                        windows_sys::Win32::Graphics::Gdi::InvalidateRect(state.video_hwnd, std::ptr::null(), 0);
                        windows_sys::Win32::Graphics::Gdi::UpdateWindow(state.video_hwnd);
                    }
                }
            }
        }
    }
}

/// Toggles Play / Pause on the active embedded video stream
pub fn toggle_pause() -> Result<bool, String> {
    let mut guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        unsafe {
            let new_paused = !state.is_paused;
            if !state.p_player.is_null() {
                if state.is_paused {
                    let _ = com_call_0(state.p_player, 3); // Play
                    prevent_sleep(true);
                } else {
                    let _ = com_call_0(state.p_player, 4); // Pause
                    prevent_sleep(false);
                }
            }
            state.is_paused = new_paused;
            log_player(if new_paused { "Playback paused." } else { "Playback resumed." });
            Ok(new_paused)
        }
    } else {
        Err("No active player".to_string())
    }
}

/// Sets playback volume (0.0 to 1.0) via IMFPMediaPlayer::SetVolume (vtable index 20)
pub fn set_volume(volume: f32) -> Result<(), String> {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let clamped = volume.clamp(0.0, 1.0);
                let hr = com_call_1(state.p_player, 20, clamped);
                if hr >= 0 {
                    log_player(&format!("Volume set to {:.0}%", clamped * 100.0));
                    return Ok(());
                } else {
                    return Err(format!("SetVolume returned 0x{:08X}", hr as u32));
                }
            }
        }
    }
    Err("No active player".to_string())
}

/// Gets current playback volume (0.0 to 1.0) via IMFPMediaPlayer::GetVolume (vtable index 19)
pub fn get_volume() -> f32 {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let mut vol: f32 = 1.0;
                let hr = com_call_1(state.p_player, 19, &mut vol as *mut f32);
                if hr >= 0 {
                    return vol.clamp(0.0, 1.0);
                }
            }
        }
    }
    1.0
}

/// Sets audio mute state via IMFPMediaPlayer::SetMute (vtable index 24)
pub fn set_muted(mute: bool) -> Result<(), String> {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let f_mute = if mute { 1i32 } else { 0i32 };
                let hr = com_call_1(state.p_player, 24, f_mute);
                if hr >= 0 {
                    log_player(if mute { "Audio muted." } else { "Audio unmuted." });
                    return Ok(());
                } else {
                    return Err(format!("SetMute returned 0x{:08X}", hr as u32));
                }
            }
        }
    }
    Err("No active player".to_string())
}

/// Gets audio mute state via IMFPMediaPlayer::GetMute (vtable index 23)
pub fn get_muted() -> bool {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let mut f_mute: i32 = 0;
                let hr = com_call_1(state.p_player, 23, &mut f_mute as *mut i32);
                if hr >= 0 {
                    return f_mute != 0;
                }
            }
        }
    }
    false
}

/// Sets playback speed/rate (0.5 to 2.0) via IMFPMediaPlayer::SetRate (vtable index 10)
pub fn set_playback_rate(rate: f32) -> Result<(), String> {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let hr = com_call_1(state.p_player, 10, rate);
                if hr >= 0 {
                    log_player(&format!("Playback rate set to {:.2}x", rate));
                    return Ok(());
                } else {
                    return Err(format!("SetRate returned 0x{:08X}", hr as u32));
                }
            }
        }
    }
    Err("No active player".to_string())
}

/// Seeks relative seconds (e.g. -15 or +30)
pub fn seek_relative(seconds: i32) -> Result<(), String> {
    let is_mkv = IS_MKV_STREAM.load(Ordering::SeqCst);
    if is_mkv {
        let (pos_ms, dur_ms, _) = get_playback_progress();
        let delta_ms = (seconds as i64) * 1000;
        let mut target_ms = (pos_ms as i64 + delta_ms).max(0) as u64;
        if dur_ms > 0 && target_ms > dur_ms {
            target_ms = dur_ms;
        }
        let pct = if dur_ms > 0 {
            (target_ms as f64 / dur_ms as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (video_hwnd, current_url) = {
            let guard = EMBEDDED_PLAYER.lock().unwrap();
            if let Some(state) = guard.as_ref() {
                let url = CURRENT_STREAM_URL.lock().unwrap().clone();
                (state.video_hwnd, url)
            } else {
                return Err("No active player".to_string());
            }
        };
        let clean_url = current_url.split('?').next().unwrap_or(&current_url);
        let new_url = format!("{}?start={}&pct={:.4}", clean_url, target_ms, pct);
        SEEK_BASE_OFFSET_MS.store(target_ms, Ordering::SeqCst);
        log_player(&format!("[MKV SEEK RELATIVE] Reconnecting player to {} (delta: {}s, target: {} ms)", new_url, seconds, target_ms));
        return init_embedded_player(video_hwnd, &new_url);
    }

    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if state.p_player.is_null() {
                return Err("MF player controller not available".to_string());
            }
            let mut pv_cur: PROPVARIANT = std::mem::zeroed();
            let mut pv_dur: PROPVARIANT = std::mem::zeroed();
            let _ = com_call_2(state.p_player, 8, &GUID_NULL as *const GUID as *const c_void, &mut pv_cur as *mut PROPVARIANT as *mut c_void);
            let _ = com_call_2(state.p_player, 9, &GUID_NULL as *const GUID as *const c_void, &mut pv_dur as *mut PROPVARIANT as *mut c_void);

            let cur_100ns = pv_cur.hVal + (SEEK_BASE_OFFSET_MS.load(Ordering::SeqCst) as i64) * 10_000;
            let mut dur_100ns = pv_dur.hVal;
            if dur_100ns <= 0 {
                let known_ms = KNOWN_DURATION_MS.load(Ordering::SeqCst);
                if known_ms > 0 {
                    dur_100ns = (known_ms as i64) * 10_000;
                }
            }
            let delta_100ns = (seconds as i64) * 10_000_000;
            let mut new_pos = cur_100ns + delta_100ns;
            if new_pos < 0 { new_pos = 0; }
            if dur_100ns > 0 && new_pos > dur_100ns { new_pos = dur_100ns; }

            let mut pv_new = PROPVARIANT::default();
            pv_new.vt = 20; // VT_I8
            pv_new.hVal = new_pos;
            let hr_seek = com_call_2(state.p_player, 7, &GUID_NULL as *const GUID as *const c_void, &pv_new as *const PROPVARIANT as *const c_void);
            if hr_seek >= 0 {
                log_player(&format!("Seeked relative: {}s", seconds));
                return Ok(());
            }
            let video_hwnd = state.video_hwnd;
            let current_url = CURRENT_STREAM_URL.lock().unwrap().clone();
            if is_jellyfin_transcode_url(&current_url) {
                let target_ms = (new_pos / 10_000).max(0) as u64;
                drop(guard);
                let new_url = rebuild_jellyfin_seek_url(&current_url, target_ms * 10_000);
                SEEK_BASE_OFFSET_MS.store(target_ms, Ordering::SeqCst);
                log_player(&format!("[JELLYFIN SEEK RELATIVE] Reconnecting at {} ms (hr=0x{:08X})", target_ms, hr_seek as u32));
                return init_embedded_player(video_hwnd, &new_url);
            }
            Err(format!("SetPosition failed: 0x{:08X}", hr_seek as u32))
        }
    } else {
        Err("No active player".to_string())
    }
}

/// Returns (pos_ms, dur_ms, progress_ratio)
pub fn get_playback_progress() -> (u64, u64, f32) {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let mut pv_cur: PROPVARIANT = std::mem::zeroed();
                let mut pv_dur: PROPVARIANT = std::mem::zeroed();
                let hr_cur = com_call_2(state.p_player, 8, &GUID_NULL as *const GUID as *const c_void, &mut pv_cur as *mut PROPVARIANT as *mut c_void);
                let hr_dur = com_call_2(state.p_player, 9, &GUID_NULL as *const GUID as *const c_void, &mut pv_dur as *mut PROPVARIANT as *mut c_void);
                if hr_cur >= 0 {
                    let base_offset = SEEK_BASE_OFFSET_MS.load(Ordering::SeqCst);
                    let raw_pos_ms = (pv_cur.hVal.max(0) / 10_000) as u64;
                    let raw_dur_ms = if hr_dur >= 0 && pv_dur.hVal > 0 {
                        (pv_dur.hVal / 10_000) as u64
                    } else {
                        0
                    };
                    let known_dur = KNOWN_DURATION_MS.load(Ordering::SeqCst);
                    let effective_dur = known_dur.max(raw_dur_ms);
                    if raw_dur_ms > 0 && known_dur < raw_dur_ms {
                        KNOWN_DURATION_MS.store(raw_dur_ms, Ordering::SeqCst);
                    }
                    let effective_pos = raw_pos_ms + base_offset;
                    let ratio = if effective_dur > 0 {
                        (effective_pos as f64 / effective_dur as f64).clamp(0.0, 1.0) as f32
                    } else {
                        0.0
                    };
                    return (effective_pos, effective_dur, ratio);
                }
            }
        }
    }
    (0, 0, 0.0)
}

/// Seeks to absolute percentage (0.0 to 1.0)
pub fn seek_percent(pct: f32) -> Result<(), String> {
    let is_mkv = IS_MKV_STREAM.load(Ordering::SeqCst);
    let pct_clamped = pct.clamp(0.0, 1.0);
    if is_mkv {
        let (video_hwnd, current_url) = {
            let guard = EMBEDDED_PLAYER.lock().unwrap();
            if let Some(state) = guard.as_ref() {
                let url = CURRENT_STREAM_URL.lock().unwrap().clone();
                (state.video_hwnd, url)
            } else {
                return Err("No active player".to_string());
            }
        };
        let dur_ms = KNOWN_DURATION_MS.load(Ordering::SeqCst);
        let target_ms = ((dur_ms as f64) * (pct_clamped as f64)) as u64;
        let clean_url = current_url.split('?').next().unwrap_or(&current_url);
        let new_url = format!("{}?start={}&pct={:.4}", clean_url, target_ms, pct_clamped);
        SEEK_BASE_OFFSET_MS.store(target_ms, Ordering::SeqCst);
        log_player(&format!("[MKV SEEK PERCENT] Reconnecting player to {} (pct: {:.1}%, target: {} ms)", new_url, pct_clamped * 100.0, target_ms));
        return init_embedded_player(video_hwnd, &new_url);
    }

    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if state.p_player.is_null() {
                return Err("MF player controller not available".to_string());
            }
            let mut pv_dur: PROPVARIANT = std::mem::zeroed();
            let _ = com_call_2(state.p_player, 9, &GUID_NULL as *const GUID as *const c_void, &mut pv_dur as *mut PROPVARIANT as *mut c_void);
            let mut dur_100ns = pv_dur.hVal;
            if dur_100ns <= 0 {
                let known_ms = KNOWN_DURATION_MS.load(Ordering::SeqCst);
                if known_ms > 0 {
                    dur_100ns = (known_ms as i64) * 10_000;
                }
            }
            if dur_100ns <= 0 {
                return Err("Unknown duration".to_string());
            }
            let target_100ns = ((dur_100ns as f64) * (pct_clamped as f64)) as i64;
            let mut pv_new = PROPVARIANT::default();
            pv_new.vt = 20; // VT_I8
            pv_new.hVal = target_100ns;
            let hr_seek = com_call_2(state.p_player, 7, &GUID_NULL as *const GUID as *const c_void, &pv_new as *const PROPVARIANT as *const c_void);
            if hr_seek >= 0 {
                log_player(&format!("Seeked to percent: {:.1}%", pct_clamped * 100.0));
                return Ok(());
            }
            let video_hwnd = state.video_hwnd;
            let current_url = CURRENT_STREAM_URL.lock().unwrap().clone();
            if is_jellyfin_transcode_url(&current_url) {
                let target_ms = (target_100ns / 10_000).max(0) as u64;
                drop(guard);
                let new_url = rebuild_jellyfin_seek_url(&current_url, target_ms * 10_000);
                SEEK_BASE_OFFSET_MS.store(target_ms, Ordering::SeqCst);
                log_player(&format!("[JELLYFIN SEEK PERCENT] Reconnecting at {} ms (pct: {:.1}%, hr=0x{:08X})", target_ms, pct_clamped * 100.0, hr_seek as u32));
                return init_embedded_player(video_hwnd, &new_url);
            }
            Err(format!("SetPosition failed: 0x{:08X}", hr_seek as u32))
        }
    } else {
        Err("No active player".to_string())
    }
}

/// Toggles full-screen mode flag
pub fn toggle_fullscreen() -> Result<bool, String> {
    let mut guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_mut() {
        state.is_fullscreen = !state.is_fullscreen;
        let is_fs = state.is_fullscreen;
        unsafe {
            if !state.p_player.is_null() {
                let _ = com_call_0(state.p_player, 32); // UpdateVideo
            }
        }
        Ok(is_fs)
    } else {
        Err("No active player".to_string())
    }
}

/// Returns current playback position in milliseconds
pub fn get_playback_pos_ms() -> u64 {
    let (pos, _, _) = get_playback_progress();
    pos
}

/// Returns active subtitle text for the current video position (if any)
pub fn get_current_subtitle() -> Option<String> {
    let pos_ms = get_playback_pos_ms();
    subtitles::get_current_subtitle_text(pos_ms)
}

/// Returns formatted playback position and duration: "MM:SS / MM:SS"
pub fn get_playback_time_str() -> String {
    let (cur_ms, dur_ms, _) = get_playback_progress();
    let cur_sec = cur_ms / 1000;
    if dur_ms > 0 {
        let dur_sec = dur_ms / 1000;
        format!("{:02}:{:02} / {:02}:{:02}", cur_sec / 60, cur_sec % 60, dur_sec / 60, dur_sec % 60)
    } else if cur_sec > 0 {
        format!("{:02}:{:02} / --:--", cur_sec / 60, cur_sec % 60)
    } else {
        let guard = EMBEDDED_PLAYER.lock().unwrap();
        if let Some(state) = guard.as_ref() {
            unsafe {
                if !state.p_player.is_null() {
                    let mut st: u32 = 0;
                    let _ = com_call_1(state.p_player, 13, &mut st as *mut u32);
                    match st {
                        0 => return "Connecting stream...".to_string(),
                        1 => return "Stopped".to_string(),
                        2 => return "Buffering / Playing...".to_string(),
                        _ => {}
                    }
                }
            }
        }
        "00:00 / --:--".to_string()
    }
}

// -------------------------------------------------------------------------------------------------
// EXTERNAL VLC PLAYER INTEGRATION
// -------------------------------------------------------------------------------------------------

#[repr(C)]
#[allow(non_snake_case)]
struct OPENFILENAMEW {
    lStructSize: u32,
    hwndOwner: HWND,
    hInstance: *mut core::ffi::c_void,
    lpstrFilter: *const u16,
    lpstrCustomFilter: *mut u16,
    nMaxCustFilter: u32,
    nFilterIndex: u32,
    lpstrFile: *mut u16,
    nMaxFile: u32,
    lpstrFileTitle: *mut u16,
    nMaxFileTitle: u32,
    lpstrInitialDir: *const u16,
    lpstrTitle: *const u16,
    flags: u32,
    nFileOffset: u16,
    nFileExtension: u16,
    lpstrDefExt: *const u16,
    lCustData: isize,
    lpfnHook: usize,
    lpTemplateName: *const u16,
    pvReserved: *mut core::ffi::c_void,
    dwReserved: u32,
    flagsEx: u32,
}

type GetOpenFileNameWFn = unsafe extern "system" fn(*mut OPENFILENAMEW) -> u32;

pub fn prompt_open_vlc_dialog(hwnd: HWND) -> Option<PathBuf> {
    unsafe {
        let lib_name = to_wide("comdlg32.dll");
        let h_lib = LoadLibraryW(lib_name.as_ptr());
        if h_lib.is_null() {
            log_err("Could not load comdlg32.dll");
            return None;
        }

        let fn_name = b"GetOpenFileNameW\0";
        let proc = GetProcAddress(h_lib, fn_name.as_ptr());
        if proc.is_none() {
            log_err("GetOpenFileNameW symbol not found in comdlg32.dll");
            return None;
        }

        let get_open_file_name: GetOpenFileNameWFn = std::mem::transmute(proc);

        let mut file_buf = vec![0u16; 1024];
        let filter = to_wide("VLC Media Player (vlc.exe)\0vlc.exe;*.exe\0All Files (*.*)\0*.*\0\0");
        let title = to_wide("Select VLC Executable (vlc.exe)");

        let mut ofn: OPENFILENAMEW = std::mem::zeroed();
        ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = hwnd;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        ofn.flags = 0x00001000 | 0x00000004 | 0x00080000; // OFN_FILEMUSTEXIST | OFN_HIDEREADONLY | OFN_EXPLORER

        if get_open_file_name(&mut ofn) != 0 {
            let len = file_buf.iter().position(|&c| c == 0).unwrap_or(file_buf.len());
            let path_str = String::from_utf16_lossy(&file_buf[..len]);
            let p = PathBuf::from(path_str);
            if p.is_file() {
                let _ = save_vlc_path(&p);
                return Some(p);
            }
        }
        None
    }
}

pub fn get_vlc_path() -> Option<PathBuf> {
    let conf_path = crate::state::get_app_storage_root().join("data").join("vlc_path.txt");
    if conf_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&conf_path) {
            let p = PathBuf::from(content.trim());
            if p.is_file() {
                return Some(p);
            }
        }
    }

    let default_paths = [
        r"C:\Program Files\VideoLAN\VLC\vlc.exe",
        r"C:\Program Files (x86)\VideoLAN\VLC\vlc.exe",
        r"C:\VLC\vlc.exe",
    ];

    for dp in &default_paths {
        let p = PathBuf::from(dp);
        if p.is_file() {
            let _ = save_vlc_path(&p);
            return Some(p);
        }
    }
    None
}

pub fn save_vlc_path(path: &Path) -> Result<(), String> {
    let dir = crate::state::get_app_storage_root().join("data");
    let _ = std::fs::create_dir_all(&dir);
    let conf_path = dir.join("vlc_path.txt");
    std::fs::write(&conf_path, path.to_string_lossy().as_bytes())
        .map_err(|e| format!("Failed to save VLC path: {}", e))
}

pub fn play_stream_in_vlc(stream_url: &str, parent_hwnd: HWND) -> Result<(), String> {
    let vlc_path = match get_vlc_path() {
        Some(p) => p,
        None => {
            log_player("VLC executable not found automatically. Prompting user to select...");
            match prompt_open_vlc_dialog(parent_hwnd) {
                Some(p) => p,
                None => return Err("VLC executable selection was canceled by user.".to_string()),
            }
        }
    };

    let cmd_line = format!(
        "\"{}\" \"{}\" --network-caching=3000 --file-caching=3000",
        vlc_path.to_string_lossy(),
        stream_url
    );

    log_player(&format!("Launching VLC: {}", cmd_line));
    launch_vlc_process(&cmd_line)
}

fn launch_vlc_process(cmd_line: &str) -> Result<(), String> {
    let mut cmd_wide = to_wide(cmd_line);

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;

    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let success = unsafe {
        CreateProcessW(
            null(),
            cmd_wide.as_mut_ptr(),
            null(),
            null(),
            FALSE,
            0,
            null(),
            null(),
            &mut si,
            &mut pi,
        )
    };

    if success == FALSE {
        let err = unsafe { GetLastError() };
        log_win32_err("CreateProcessW(vlc.exe)", err);
        return Err(format!("Failed to launch VLC process. Win32 error: {}", err));
    }

    unsafe {
        CloseHandle(pi.hProcess);
        CloseHandle(pi.hThread);
    }

    log_player("VLC launched successfully in external process.");
    Ok(())
}
