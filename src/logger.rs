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

/// Ring buffer preserving the last 50 log lines for crash dumps and diagnostics
static RECENT_LOGS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Atomic flag preventing Win32 message queue saturation / re-entrancy storms
static LOG_NOTIFY_PENDING: AtomicBool = AtomicBool::new(false);

/// Win32 main window handle stored atomically
static GUI_HWND: AtomicIsize = AtomicIsize::new(0);

/// Cached path to active log file
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

/// Rotates log files in `logs_dir` keeping at most 5 files.
fn rotate_logs_if_needed(logs_dir: &std::path::Path) {
    let active_log = logs_dir.join("strawberry32_debug.log");
    if !active_log.is_file() {
        return;
    }

    // Only rotate if file has existing content
    if let Ok(meta) = active_log.metadata() {
        if meta.len() == 0 {
            return;
        }
    }

    // Shift 3 -> 4, 2 -> 3, 1 -> 2
    let _ = std::fs::remove_file(logs_dir.join("strawberry32_debug.4.log"));
    for i in (1..=3).rev() {
        let src = logs_dir.join(format!("strawberry32_debug.{}.log", i));
        let dst = logs_dir.join(format!("strawberry32_debug.{}.log", i + 1));
        if src.is_file() {
            let _ = std::fs::rename(src, dst);
        }
    }
    let _ = std::fs::rename(&active_log, logs_dir.join("strawberry32_debug.1.log"));
}

/// Sanitizes log messages to ensure authentication tokens, API keys and credentials are never logged.
pub fn sanitize_log_message(msg: &str) -> String {
    let lower = msg.to_ascii_lowercase();
    if !lower.contains("token=") && !lower.contains("api_key=") && !lower.contains("key=") && !lower.contains("secret=") {
        return msg.to_string();
    }

    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(idx) = rest.find(|c| c == '?' || c == '&') {
        out.push_str(&rest[..idx + 1]);
        rest = &rest[idx + 1..];
        for prefix in &["token=", "api_key=", "key=", "secret="] {
            if rest.to_ascii_lowercase().starts_with(prefix) {
                out.push_str(prefix);
                out.push_str("[REDACTED]");
                if let Some(next_param) = rest.find('&') {
                    rest = &rest[next_param..];
                } else if let Some(space) = rest.find(' ') {
                    rest = &rest[space..];
                } else {
                    rest = "";
                }
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Returns the last ~50 log lines recorded in memory.
pub fn get_recent_logs() -> Vec<String> {
    if let Ok(guard) = RECENT_LOGS.lock() {
        guard.iter().cloned().collect()
    } else {
        Vec::new()
    }
}

/// Gathers hardware and OS diagnostic information for crash dumps and diagnostics UI.
pub fn get_system_diagnostic_summary() -> String {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut mem: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let (total_ram_mb, free_ram_mb, mem_load) = unsafe {
        if GlobalMemoryStatusEx(&mut mem) != 0 {
            (mem.ullTotalPhys / (1024 * 1024), mem.ullAvailPhys / (1024 * 1024), mem.dwMemoryLoad)
        } else {
            (0, 0, 0)
        }
    };

    let base_module = unsafe {
        windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null()) as usize
    };

    format!(
        "Application: Strawberry32 v{}\nTarget: ARM32 thumbv7a-pc-windows-msvc (Windows RT 8.1 / NVIDIA Tegra 3)\nExecutable Base: 0x{:08X}\nRAM: {} MB Free / {} MB Total (Load: {}%)\n",
        env!("CARGO_PKG_VERSION"),
        base_module,
        free_ram_mb,
        total_ram_mb,
        mem_load
    )
}

/// Initializes the global logger, performs log rotation, and returns the active log file path.
pub fn init() -> PathBuf {
    let log_dir = if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        PathBuf::from(local_appdata).join("Strawberry32").join("logs")
    } else {
        get_temp_dir().join("Strawberry32").join("logs")
    };
    let _ = std::fs::create_dir_all(&log_dir);

    rotate_logs_if_needed(&log_dir);

    let log_file = log_dir.join("strawberry32_debug.log");

    // Initialize or truncate active log file on app startup
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
/// 1. Sanitizes credentials & tokens
/// 2. Prints to stdout (if console allocated)
/// 3. Appends to active log file (flushing immediately on errors)
/// 4. Stores in 50-line memory ring buffer
/// 5. Queues for GUI listbox
pub fn log_entry(tag: &str, message: &str) {
    let sanitized = sanitize_log_message(message);
    let timestamp = format_local_now();
    let formatted = format!("{} [{}] {}", timestamp, tag, sanitized);

    // 1. stdout
    println!("{}", formatted);

    // 2. Append to log file
    if let Some(path) = LOG_FILE_PATH.get() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{}", formatted);
            if tag == "ERR" {
                let _ = file.flush();
            }
        }
    }

    // 3. Ring buffer for crash logs & diagnostics
    if let Ok(mut rb) = RECENT_LOGS.lock() {
        rb.push_back(formatted.clone());
        if rb.len() > 50 {
            rb.pop_front();
        }
    }

    // 4. Queue for GUI inside a minimal scope block
    {
        if let Ok(mut q) = LOG_QUEUE.lock() {
            q.push_back(formatted);
            // Cap queue size at 500 lines to protect Surface RT RAM if GUI is delayed
            if q.len() > 500 {
                q.pop_front();
            }
        }
    } // Queue lock is dropped immediately here!

    // 5. Notify GUI without holding any locks, and only if a notification isn't already pending
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
