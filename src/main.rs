#![windows_subsystem = "windows"]

mod addons;
mod cache_cleaner;
mod container_meta;
mod engine;
mod gui;
mod logger;
mod net;
mod player;
mod redact;
mod sources;
mod state;
mod subtitles;
mod trackers;
mod ui;

#[cfg(test)]
mod tests;

use logger::{log_err, log_info};

#[repr(C)]
struct EXCEPTION_RECORD {
    exception_code: u32,
    exception_flags: u32,
    exception_record: *mut EXCEPTION_RECORD,
    exception_address: *mut std::ffi::c_void,
    number_parameters: u32,
    exception_information: [usize; 15],
}

#[repr(C)]
struct EXCEPTION_POINTERS {
    exception_record: *mut EXCEPTION_RECORD,
    context_record: *mut std::ffi::c_void,
}

const GENERIC_READ: u32 = 0x80000000;
const GENERIC_WRITE: u32 = 0x40000000;

#[allow(non_camel_case_types)]
type PVECTORED_EXCEPTION_HANDLER = Option<unsafe extern "system" fn(info: *mut EXCEPTION_POINTERS) -> i32>;

extern "system" {
    fn SetUnhandledExceptionFilter(top_level_filter: PVECTORED_EXCEPTION_HANDLER) -> PVECTORED_EXCEPTION_HANDLER;
}

unsafe extern "system" fn unhandled_native_exception_filter(info: *mut EXCEPTION_POINTERS) -> i32 {
    let (code, addr) = if !info.is_null() && !(*info).exception_record.is_null() {
        let rec = &*(*info).exception_record;
        (rec.exception_code, rec.exception_address as usize)
    } else {
        (0, 0)
    };

    let crash_file = state::get_logs_dir().join("crash.log");
    let diag = logger::get_system_diagnostic_summary();
    let recent = logger::get_recent_logs();

    let mut report = format!(
        "=== STRAWBERRY32 NATIVE EXCEPTION CRASH REPORT ===\nException Code: 0x{:08X}\nException Address: 0x{:08X}\n{}\nRecent Activity (last {} events):\n",
        code,
        addr,
        diag,
        recent.len()
    );
    for l in recent {
        report.push_str("  ");
        report.push_str(&l);
        report.push('\n');
    }
    report.push_str("\n=== END OF REPORT ===\n\n");

    let _ = std::fs::write(&crash_file, report);

    try_write_minidump(info);

    1 // EXCEPTION_EXECUTE_HANDLER
}

unsafe fn try_write_minidump(info: *mut EXCEPTION_POINTERS) {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
    let dbghelp = LoadLibraryA(b"dbghelp.dll\0".as_ptr());
    if !dbghelp.is_null() {
        let proc = GetProcAddress(dbghelp, b"MiniDumpWriteDump\0".as_ptr());
        if let Some(dump_fn) = proc {
            type MiniDumpWriteDumpFn = unsafe extern "system" fn(
                windows_sys::Win32::Foundation::HANDLE,
                u32,
                windows_sys::Win32::Foundation::HANDLE,
                u32,
                *const std::ffi::c_void,
                *const std::ffi::c_void,
                *const std::ffi::c_void,
            ) -> i32;

            let dump_fn: MiniDumpWriteDumpFn = std::mem::transmute(dump_fn);
            let dmp_path = state::get_logs_dir().join("crash.dmp");
            let dmp_wide: Vec<u16> = dmp_path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            use windows_sys::Win32::Storage::FileSystem::*;
            let file_handle = CreateFileW(
                dmp_wide.as_ptr(),
                GENERIC_WRITE,
                0,
                std::ptr::null(),
                CREATE_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            );
            if !file_handle.is_null() && file_handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                #[repr(C)]
                struct MINIDUMP_EXCEPTION_INFORMATION {
                    thread_id: u32,
                    exception_pointers: *mut EXCEPTION_POINTERS,
                    client_pointers: i32,
                }
                let exp_info = MINIDUMP_EXCEPTION_INFORMATION {
                    thread_id: windows_sys::Win32::System::Threading::GetCurrentThreadId(),
                    exception_pointers: info,
                    client_pointers: 0,
                };
                let current_proc = windows_sys::Win32::System::Threading::GetCurrentProcess();
                let current_pid = windows_sys::Win32::System::Threading::GetCurrentProcessId();
                let _ = dump_fn(
                    current_proc,
                    current_pid,
                    file_handle,
                    0, // MiniDumpNormal
                    &exp_info as *const _ as *const std::ffi::c_void,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                windows_sys::Win32::Foundation::CloseHandle(file_handle);
            }
        }
        windows_sys::Win32::Foundation::CloseHandle(dbghelp as windows_sys::Win32::Foundation::HANDLE);
    }
}

fn attach_console_if_requested() {
    let wants_console = std::env::args().any(|a| a == "--console");
    if wants_console {
        unsafe {
            if windows_sys::Win32::System::Console::AllocConsole() != 0 {
                use windows_sys::Win32::Storage::FileSystem::*;
                use windows_sys::Win32::System::Console::*;
                let conout_name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
                let conout = CreateFileW(
                    conout_name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if !conout.is_null() && conout != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
                    SetStdHandle(STD_OUTPUT_HANDLE, conout);
                    SetStdHandle(STD_ERROR_HANDLE, conout);
                }
            }
        }
    }
}

fn main() {
    // Check opt-in --console CLI flag
    attach_console_if_requested();

    // 0. Install native unhandled exception filter for Media Foundation / COM / DXVA native faults
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_native_exception_filter));
    }

    // 0b. Install Rust panic hook with hardware diagnostics & last 50 log lines
    std::panic::set_hook(Box::new(|info| {
        let crash_file = state::get_logs_dir().join("crash.log");
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            *s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "unknown panic payload"
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        let diag = logger::get_system_diagnostic_summary();
        let recent = logger::get_recent_logs();

        let mut report = format!(
            "=== STRAWBERRY32 RUST PANIC REPORT ===\nLocation: {}\nMessage: {}\n{}\nRecent Activity (last {} events):\n",
            location, payload, diag, recent.len()
        );
        for l in recent {
            report.push_str("  ");
            report.push_str(&l);
            report.push('\n');
        }
        report.push_str("\n=== END OF REPORT ===\n\n");

        let _ = std::fs::write(&crash_file, report);
        log_err(&format!("CRITICAL UNCAUGHT PANIC at {}: {}", location, payload));
    }));

    // 1. Initialize thread-safe telemetry and file logger
    let log_path = logger::init();
    log_info("=== STARTING STRAWBERRY32 APPLICATION (ARM32) ===");
    log_info(&format!("Active diagnostic log: {}", log_path.display()));

    // 2. Resolve storage root and canonical layout
    let storage_root = state::get_app_storage_root();
    log_info(&format!("Strawberry32 storage root: {}", storage_root.display()));
    migrate_legacy_data_if_needed(&storage_root);

    // 3. Start background librqbit engine & HTTP range server on 127.0.0.1:3030
    log_info("Starting librqbit 8.0 streaming engine on background thread...");
    if let Err(e) = engine::start_engine() {
        log_err(&format!("Critical error starting librqbit engine: {}", e));
    }

    // 4. Initialize live tracker manager (24h RTT probe & cache)
    trackers::init_tracker_manager();

    // 5. Start protected LRU disk cache cleaner
    cache_cleaner::start_cache_cleaner();

    // 6. Launch Slint native user interface
    log_info("Launching native Slint user interface...");
    if let Err(e) = gui::run_app() {
        log_err(&format!("Error in Slint UI event loop: {}", e));
    }

    log_info("Closing Strawberry32...");
    // Terminate tokio background threads and all sockets cleanly
    std::process::exit(0);
}

/// Automatically migrates settings and data from old folders ('streaming-app-arm32', 'rt_stream_data')
/// into '%LOCALAPPDATA%\Strawberry32' and removes old directories.
fn migrate_legacy_data_if_needed(target_root: &std::path::Path) {
    if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
        let legacy_dir = std::path::PathBuf::from(local_appdata).join("streaming-app-arm32");
        if legacy_dir.is_dir() {
            log_info(&format!("Migrating legacy data from {}...", legacy_dir.display()));
            let target_data = target_root.join("data");
            let _ = std::fs::create_dir_all(&target_data);
            for name in &["settings.json", "watch_history.json", "addons.json"] {
                let old_file = legacy_dir.join(name);
                let new_file = target_data.join(name);
                if old_file.is_file() && !new_file.exists() {
                    let _ = std::fs::copy(&old_file, &new_file);
                }
            }
            let _ = std::fs::remove_dir_all(&legacy_dir);
        }
    }

    let temp_data = logger::get_temp_dir().join("rt_stream_data");
    if temp_data.is_dir() {
        let target_data = target_root.join("data");
        for name in &["settings.json", "watch_history.json", "addons.json", "vlc_path.txt"] {
            let old_file = temp_data.join(name);
            let new_file = target_data.join(name);
            if old_file.is_file() && !new_file.exists() {
                let _ = std::fs::copy(&old_file, &new_file);
            }
        }
        let _ = std::fs::remove_dir_all(&temp_data);
    }

    let temp_cache = logger::get_temp_dir().join("rt_stream_cache");
    if temp_cache.is_dir() {
        let _ = std::fs::remove_dir_all(&temp_cache);
    }
}
