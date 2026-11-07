use crate::logger::{get_temp_dir, log_err, log_player, log_win32_err};
use crate::subtitles;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
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
static MF_INITIALIZED: Mutex<bool> = Mutex::new(false);

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
                } else {
                    let _ = com_call_0(state.p_player, 4); // Pause
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

/// Seeks relative seconds (e.g. -15 or +30)
pub fn seek_relative(seconds: i32) -> Result<(), String> {
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

            let cur_100ns = pv_cur.hVal;
            let dur_100ns = pv_dur.hVal;
            let delta_100ns = (seconds as i64) * 10_000_000;
            let mut new_pos = cur_100ns + delta_100ns;
            if new_pos < 0 { new_pos = 0; }
            if dur_100ns > 0 && new_pos > dur_100ns { new_pos = dur_100ns; }

            let mut pv_new = PROPVARIANT::default();
            pv_new.vt = 20; // VT_I8
            pv_new.hVal = new_pos;
            let _ = com_call_2(state.p_player, 7, &GUID_NULL as *const GUID as *const c_void, &pv_new as *const PROPVARIANT as *const c_void);
            log_player(&format!("Seeked relative: {}s", seconds));
            Ok(())
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
                if hr_cur >= 0 && hr_dur >= 0 && pv_dur.hVal > 0 {
                    let pos_ms = (pv_cur.hVal.max(0) / 10_000) as u64;
                    let dur_ms = (pv_dur.hVal.max(0) / 10_000) as u64;
                    let ratio = (pv_cur.hVal as f64 / pv_dur.hVal as f64).clamp(0.0, 1.0) as f32;
                    return (pos_ms, dur_ms, ratio);
                }
            }
        }
    }
    (0, 0, 0.0)
}

/// Seeks to absolute percentage (0.0 to 1.0)
pub fn seek_percent(pct: f32) -> Result<(), String> {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if state.p_player.is_null() {
                return Err("MF player controller not available".to_string());
            }
            let mut pv_dur: PROPVARIANT = std::mem::zeroed();
            let _ = com_call_2(state.p_player, 9, &GUID_NULL as *const GUID as *const c_void, &mut pv_dur as *mut PROPVARIANT as *mut c_void);
            let dur_100ns = pv_dur.hVal;
            if dur_100ns <= 0 {
                return Err("Unknown duration".to_string());
            }
            let target_100ns = ((dur_100ns as f64) * (pct.clamp(0.0, 1.0) as f64)) as i64;
            let mut pv_new = PROPVARIANT::default();
            pv_new.vt = 20; // VT_I8
            pv_new.hVal = target_100ns;
            let _ = com_call_2(state.p_player, 7, &GUID_NULL as *const GUID as *const c_void, &pv_new as *const PROPVARIANT as *const c_void);
            log_player(&format!("Seeked to percent: {:.1}%", pct * 100.0));
            Ok(())
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
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let mut pv_cur: PROPVARIANT = std::mem::zeroed();
                let hr = com_call_2(state.p_player, 8, &GUID_NULL as *const GUID as *const c_void, &mut pv_cur as *mut PROPVARIANT as *mut c_void);
                if hr >= 0 && pv_cur.hVal > 0 {
                    return (pv_cur.hVal / 10_000) as u64; // 100ns to ms
                }
            }
        }
    }
    0
}

/// Returns active subtitle text for the current video position (if any)
pub fn get_current_subtitle() -> Option<String> {
    let pos_ms = get_playback_pos_ms();
    subtitles::get_current_subtitle_text(pos_ms)
}

/// Returns formatted playback position and duration: "MM:SS / MM:SS"
pub fn get_playback_time_str() -> String {
    let guard = EMBEDDED_PLAYER.lock().unwrap();
    if let Some(state) = guard.as_ref() {
        unsafe {
            if !state.p_player.is_null() {
                let mut pv_cur: PROPVARIANT = std::mem::zeroed();
                let mut pv_dur: PROPVARIANT = std::mem::zeroed();
                let hr_pos = com_call_2(state.p_player, 8, &GUID_NULL as *const GUID as *const c_void, &mut pv_cur as *mut PROPVARIANT as *mut c_void);
                let hr_dur = com_call_2(state.p_player, 9, &GUID_NULL as *const GUID as *const c_void, &mut pv_dur as *mut PROPVARIANT as *mut c_void);

                if hr_pos >= 0 && hr_dur >= 0 && pv_dur.hVal > 0 {
                    let cur_sec = (pv_cur.hVal / 10_000_000).max(0);
                    let dur_sec = (pv_dur.hVal / 10_000_000).max(0);
                    return format!("{:02}:{:02} / {:02}:{:02}", cur_sec / 60, cur_sec % 60, dur_sec / 60, dur_sec % 60);
                } else if hr_pos >= 0 && pv_cur.hVal > 0 {
                    let cur_sec = (pv_cur.hVal / 10_000_000).max(0);
                    return format!("{:02}:{:02} / --:--", cur_sec / 60, cur_sec % 60);
                } else {
                    let mut st: u32 = 0;
                    let _ = com_call_1(state.p_player, 13, &mut st as *mut u32);
                    match st {
                        0 => return "Connecting stream...".to_string(),
                        1 => return "Stopped".to_string(),
                        2 => return "Buffering / Playing...".to_string(),
                        3 => return "Paused".to_string(),
                        _ => {}
                    }
                }
            }
        }
    }
    "00:00 / 00:00".to_string()
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
    let conf_path = get_temp_dir().join("rt_stream_data").join("vlc_path.txt");
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
    let dir = get_temp_dir().join("rt_stream_data");
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
