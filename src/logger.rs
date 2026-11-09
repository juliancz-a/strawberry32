use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Storage::FileSystem::GetTempPathW;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// Custom Win32 message posted to the GUI window when new log entries arrive.
pub const WM_APP_NEW_LOG: u32 = WM_APP + 101;

/// Thread-safe queue for log lines destined for the Win32 GUI
static LOG_QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Atomic flag preventing Win32 message queue saturation / re-entrancy storms
static LOG_NOTIFY_PENDING: AtomicBool = AtomicBool::new(false);

/// Win32 main window handle stored atomically
static GUI_HWND: AtomicIsize = AtomicIsize::new(0);

/// Cached path to %TEMP%\rt_stream_debug.log
static LOG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Returns the %TEMP% directory dynamically using Win32 GetTempPathW.
/// Avoids ERROR_ACCESS_DENIED (5) on Windows RT by not touching root or protected drives.
pub fn get_temp_dir() -> PathBuf {
    let mut buf = [0u16; 512];
    let len = unsafe { GetTempPathW(buf.len() as u32, buf.as_mut_ptr()) };
    if len > 0 {
        let path_str = String::from_utf16_lossy(&buf[..len as usize]);
        PathBuf::from(path_str.trim_end_matches('\\'))
    } else {
        std::env::temp_dir()
    }
}

/// Initializes the global logger and returns the active log file path.
pub fn init() -> PathBuf {
    let log_dir = if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local_appdata).join("Strawberry32").join("logs")
    } else {
        get_temp_dir().join("Strawberry32").join("logs")
    };
    let _ = std::fs::create_dir_all(&log_dir);
    let log_file = log_dir.join("strawberry32_debug.log");

    // Initialize or truncate log file on app startup
    let _ = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_file);

    let _ = LOG_FILE_PATH.set(log_file.clone());
    log_info(&format!("Logger initialized. File: {}", log_file.display()));
    log_file
}

/// Registers the Win32 main window HWND to receive log notifications.
pub fn set_gui_hwnd(hwnd: HWND) {
    GUI_HWND.store(hwnd as isize, Ordering::SeqCst);

    // If there were early logs buffered before GUI was ready, notify GUI now
    let has_pending = {
        LOG_QUEUE.lock().map(|q| !q.is_empty()).unwrap_or(false)
    };

    if has_pending && !LOG_NOTIFY_PENDING.swap(true, Ordering::SeqCst) {
        unsafe {
            PostMessageW(hwnd, WM_APP_NEW_LOG, 0, 0);
        }
    }
}

/// Drains all buffered log messages destined for the GUI listbox.
/// Resets the notification pending flag atomically.
pub fn drain_ui_logs() -> Vec<String> {
    // Reset notification flag FIRST so new incoming logs can trigger a future message
    LOG_NOTIFY_PENDING.store(false, Ordering::SeqCst);

    let mut result = Vec::new();
    if let Ok(mut q) = LOG_QUEUE.lock() {
        result.extend(q.drain(..));
    }
    result
}

/// Central log dispatcher:
/// 1. Prints to stdout
/// 2. Appends to %TEMP%\rt_stream_debug.log
/// 3. Queues for GUI in a minimal lock scope
/// 4. Drops lock FIRST, then posts WM_APP_NEW_LOG if no notification is pending
pub fn log_entry(tag: &str, message: &str) {
    let timestamp = format_local_now();
    let formatted = format!("{} [{}] {}", timestamp, tag, message);

    // 1. stdout
    println!("{}", formatted);

    // 2. Append to log file
    if let Some(path) = LOG_FILE_PATH.get() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{}", formatted);
        }
    }

    // 3. Queue for GUI inside a minimal scope block
    {
        if let Ok(mut q) = LOG_QUEUE.lock() {
            q.push_back(formatted);
            // Cap queue size at 500 lines to protect Surface RT RAM if GUI is delayed
            if q.len() > 500 {
                q.pop_front();
            }
        }
    } // Queue lock is dropped immediately here!

    // 4. Notify GUI without holding any locks, and only if a notification isn't already pending
    let hwnd = GUI_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        if !LOG_NOTIFY_PENDING.swap(true, Ordering::SeqCst) {
            unsafe {
                PostMessageW(hwnd as HWND, WM_APP_NEW_LOG, 0, 0);
            }
        }
    }
}

pub fn log_info(msg: &str) {
    log_entry("INFO", msg);
}

pub fn log_net(msg: &str) {
    log_entry("NET", msg);
}

pub fn log_torrent(msg: &str) {
    log_entry("TORRENT", msg);
}

pub fn log_player(msg: &str) {
    log_entry("PLAYER", msg);
}

pub fn log_err(msg: &str) {
    log_entry("ERR", msg);
}

/// Logs full ureq HTTP / TLS error chain (essential for diagnosing Windows RT Schannel handshakes)
pub fn log_ureq_err(context: &str, err: &ureq::Error) {
    match err {
        ureq::Error::Status(code, resp) => {
            let status_text = resp.status_text().to_string();
            log_err(&format!("{}: HTTP {} {}", context, code, status_text));
        }
        ureq::Error::Transport(transport_err) => {
            log_err(&format!("{}: Error de Transporte / TLS Schannel: {}", context, transport_err));
            let mut source = std::error::Error::source(transport_err);
            while let Some(s) = source {
                log_err(&format!("   -> Causa: {}", s));
                source = std::error::Error::source(s);
            }
        }
    }
}

/// Logs Win32 GetLastError with context
pub fn log_win32_err(context: &str, error_code: u32) {
    log_err(&format!("{}: Win32 Error Code {}", context, error_code));
}

/// Lightweight time helper for log timestamps
fn format_local_now() -> String {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}
