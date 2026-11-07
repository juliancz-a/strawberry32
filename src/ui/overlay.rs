use crate::logger::{log_err, log_info};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ, RGN_OR,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SetParent, SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
    HWND_TOP, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNA,
    WS_CAPTION, WS_CHILD, WS_CLIPSIBLINGS, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

// Re-export generated component
slint::include_modules!();

static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);
static OVERLAY_WIDTH: AtomicI32 = AtomicI32::new(0);
static OVERLAY_HEIGHT: AtomicI32 = AtomicI32::new(0);
static CONTROLS_VISIBLE: AtomicBool = AtomicBool::new(true);
static HAS_SUBTITLE: AtomicBool = AtomicBool::new(false);
static LAST_ACTIVITY: Mutex<Option<Instant>> = Mutex::new(None);

thread_local! {
    static OVERLAY_INSTANCE: RefCell<Option<PlayerOverlay>> = const { RefCell::new(None) };
}

fn get_component_hwnd(window: &slint::Window) -> HWND {
    if let Ok(wh) = window.window_handle().window_handle() {
        if let RawWindowHandle::Win32(h) = wh.as_raw() {
            return h.hwnd.get() as HWND;
        }
    }
    std::ptr::null_mut()
}

/// Updates the visible window region using SetWindowRgn instead of LWA_COLORKEY.
/// Only the top bar, bottom bar, and subtitle box exist as pixels;
/// the video area between them is completely excluded from the window, allowing
/// video to show through without color-keying or DWM transparency bugs.
pub fn update_overlay_region(hwnd: HWND, width: i32, height: i32, controls_visible: bool, has_subtitle: bool) {
    if hwnd.is_null() || width <= 0 || height <= 0 {
        return;
    }

    unsafe {
        let combined_rgn = CreateRectRgn(0, 0, 0, 0);

        if controls_visible {
            // Top bar rect: 0..width, 0..50
            let top_rgn = CreateRectRgn(0, 0, width, 50);
            CombineRgn(combined_rgn, combined_rgn, top_rgn, RGN_OR);
            DeleteObject(top_rgn as HGDIOBJ);

            // Bottom transport bar rect: 0..width, (height - 74)..height
            let bottom_y = (height - 74).max(0);
            let bottom_rgn = CreateRectRgn(0, bottom_y, width, height);
            CombineRgn(combined_rgn, combined_rgn, bottom_rgn, RGN_OR);
            DeleteObject(bottom_rgn as HGDIOBJ);
        }

        if has_subtitle {
            let sub_x1 = (width / 2 - 320).max(0);
            let sub_x2 = (width / 2 + 320).min(width);
            let (sub_y1, sub_y2) = if controls_visible {
                ((height - 136).max(0), (height - 80).max(0))
            } else {
                ((height - 68).max(0), (height - 12).max(0))
            };

            let sub_rgn = CreateRectRgn(sub_x1, sub_y1, sub_x2, sub_y2);
            CombineRgn(combined_rgn, combined_rgn, sub_rgn, RGN_OR);
            DeleteObject(sub_rgn as HGDIOBJ);
        }

        // SetWindowRgn takes ownership of combined_rgn if successful
        let res = SetWindowRgn(hwnd, combined_rgn, 1);
        if res == 0 {
            DeleteObject(combined_rgn as HGDIOBJ);
            log_err("SetWindowRgn failed for PlayerOverlay");
        }
    }
}

/// Applies Win32 child window styling, reparents to MainWindow via SetParent, and sets the initial region
pub fn setup_overlay_window(hwnd: HWND, main_hwnd: HWND, width: i32, height: i32) {
    if hwnd.is_null() {
        return;
    }

    unsafe {
        // 1. Immediately hide so Windows 8.1/RT unregisters the window from the Taskbar & App Switcher
        ShowWindow(hwnd, SW_HIDE);

        // 2. Strip WS_EX_APPWINDOW and WS_EX_LAYERED; set WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
        let mut ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        ex_style &= !(WS_EX_APPWINDOW | WS_EX_LAYERED);
        ex_style |= WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        SetWindowLongW(hwnd, GWL_EXSTYLE, ex_style as i32);

        // 3. Strip WS_POPUP, captions, and frames; set WS_CHILD | WS_CLIPSIBLINGS
        let mut style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        style &= !(WS_POPUP | WS_CAPTION | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_SYSMENU);
        style |= WS_CHILD | WS_CLIPSIBLINGS;
        SetWindowLongW(hwnd, GWL_STYLE, style as i32);

        // 4. Reparent to MainWindow as a true Win32 child
        if !main_hwnd.is_null() {
            SetParent(hwnd, main_hwnd);
            log_info(&format!(
                "PlayerOverlay HWND 0x{:X} converted to WS_CHILD and reparented to MainWindow 0x{:X}",
                hwnd as usize, main_hwnd as usize
            ));
        }

        // 5. Apply initial window region and position
        update_overlay_region(hwnd, width, height, true, false);
        SetWindowPos(
            hwnd,
            HWND_TOP,
            0,
            0,
            width,
            height,
            SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// Creates PlayerOverlay on first call, reparents it, styles it, and tracks activity
pub fn ensure_overlay(
    main_hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<PlayerOverlay, String> {
    OVERLAY_INSTANCE.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(existing) = guard.as_ref() {
            position_overlay(main_hwnd, x, y, width, height);
            show_overlay();
            notify_user_activity();
            return Ok(existing.clone_strong());
        }

        let overlay = PlayerOverlay::new()
            .map_err(|e| format!("Failed to create PlayerOverlay: {}", e))?;

        // 1. Show FIRST so winit creates and maps the native HWND
        overlay.show().map_err(|e| format!("Failed to show PlayerOverlay: {}", e))?;

        // 2. Resolve HWND immediately
        let hwnd = get_component_hwnd(overlay.window());
        if !hwnd.is_null() {
            setup_overlay_window(hwnd, main_hwnd, width, height);
            OVERLAY_HWND.store(hwnd as isize, Ordering::SeqCst);
            position_overlay(main_hwnd, x, y, width, height);
            notify_user_activity();
            log_info(&format!(
                "PlayerOverlay HWND resolved immediately (0x{:X}) and configured as WS_CHILD",
                hwnd as usize
            ));
        } else {
            // Retry on the next tick if winit hasn't attached the HWND synchronously
            log_info("PlayerOverlay HWND was null after show(); scheduling tick retry...");
            let overlay_weak = overlay.as_weak();
            slint::Timer::single_shot(Duration::from_millis(20), move || {
                if let Some(ov) = overlay_weak.upgrade() {
                    let h = get_component_hwnd(ov.window());
                    if !h.is_null() {
                        setup_overlay_window(h, main_hwnd, width, height);
                        OVERLAY_HWND.store(h as isize, Ordering::SeqCst);
                        position_overlay(main_hwnd, x, y, width, height);
                        notify_user_activity();
                        log_info(&format!("PlayerOverlay HWND resolved on retry tick: 0x{:X}", h as usize));
                    } else {
                        log_err("PlayerOverlay HWND still null on retry tick!");
                    }
                }
            });
        }

        *guard = Some(overlay.clone_strong());
        Ok(overlay)
    })
}

/// Repositions the overlay window via Win32 SetWindowPos using client coordinates
pub fn position_overlay(_main_hwnd: HWND, x_client: i32, y_client: i32, width: i32, height: i32) {
    let hwnd = OVERLAY_HWND.load(Ordering::SeqCst) as HWND;
    if hwnd.is_null() || width <= 0 || height <= 0 {
        return;
    }

    OVERLAY_WIDTH.store(width, Ordering::SeqCst);
    OVERLAY_HEIGHT.store(height, Ordering::SeqCst);

    unsafe {
        // Because overlay is a WS_CHILD of main_hwnd, client coords are direct!
        SetWindowPos(
            hwnd,
            HWND_TOP,
            x_client,
            y_client,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );

        let vis = CONTROLS_VISIBLE.load(Ordering::SeqCst);
        let sub = HAS_SUBTITLE.load(Ordering::SeqCst);
        update_overlay_region(hwnd, width, height, vis, sub);
    }
}

/// Updates playback telemetry on the overlay
pub fn update_overlay_state(time_str: &str, progress: f32, is_paused: bool, sub_text: &str, is_fs: bool) {
    let has_sub = !sub_text.trim().is_empty();
    let prev_has_sub = HAS_SUBTITLE.swap(has_sub, Ordering::SeqCst);
    if has_sub != prev_has_sub {
        let hwnd = OVERLAY_HWND.load(Ordering::SeqCst) as HWND;
        let w = OVERLAY_WIDTH.load(Ordering::SeqCst);
        let h = OVERLAY_HEIGHT.load(Ordering::SeqCst);
        let vis = CONTROLS_VISIBLE.load(Ordering::SeqCst);
        if !hwnd.is_null() && w > 0 && h > 0 {
            update_overlay_region(hwnd, w, h, vis, has_sub);
        }
    }

    OVERLAY_INSTANCE.with(|cell| {
        if let Some(overlay) = cell.borrow().as_ref() {
            overlay.set_time_str(time_str.into());
            overlay.set_progress(progress);
            overlay.set_is_paused(is_paused);
            overlay.set_subtitle_text(sub_text.into());
            overlay.set_is_fullscreen(is_fs);
        }
    });
}

/// Toggles controls visibility on the overlay
pub fn toggle_controls_visible() {
    let cur = CONTROLS_VISIBLE.load(Ordering::SeqCst);
    set_controls_visible(!cur);
    if !cur {
        notify_user_activity();
    }
}

/// Sets controls visibility on the overlay and updates the window region
pub fn set_controls_visible(visible: bool) {
    CONTROLS_VISIBLE.store(visible, Ordering::SeqCst);
    let hwnd = OVERLAY_HWND.load(Ordering::SeqCst) as HWND;
    let w = OVERLAY_WIDTH.load(Ordering::SeqCst);
    let h = OVERLAY_HEIGHT.load(Ordering::SeqCst);
    let sub = HAS_SUBTITLE.load(Ordering::SeqCst);

    if !hwnd.is_null() && w > 0 && h > 0 {
        update_overlay_region(hwnd, w, h, visible, sub);
    }

    OVERLAY_INSTANCE.with(|cell| {
        if let Some(overlay) = cell.borrow().as_ref() {
            overlay.set_controls_visible(visible);
        }
    });
}

/// Returns current controls visibility
pub fn get_controls_visible() -> bool {
    CONTROLS_VISIBLE.load(Ordering::SeqCst)
}

/// Notifies user activity: resets inactivity timer, wakes controls if hidden
pub fn notify_user_activity() {
    let mut guard = LAST_ACTIVITY.lock().unwrap();
    *guard = Some(Instant::now());
    drop(guard);

    if !CONTROLS_VISIBLE.load(Ordering::SeqCst) {
        set_controls_visible(true);
    }
}

/// Checks inactivity timer: auto-hides controls if not paused and > 3500ms since last activity
pub fn check_auto_hide(is_paused: bool) {
    if is_paused {
        return; // Always keep controls visible when paused
    }
    if !CONTROLS_VISIBLE.load(Ordering::SeqCst) {
        return; // Already hidden
    }

    let guard = LAST_ACTIVITY.lock().unwrap();
    if let Some(t) = *guard {
        if t.elapsed() > Duration::from_millis(3500) {
            drop(guard);
            set_controls_visible(false);
        }
    }
}

/// Shows the overlay window via Win32 SW_SHOWNA (never call overlay.show() which resets winit styles)
pub fn show_overlay() {
    let hwnd = OVERLAY_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe {
            ShowWindow(hwnd, SW_SHOWNA);
        }
    }
}

/// Hides the overlay window when leaving the player or minimizing (via Win32 SW_HIDE)
pub fn hide_overlay() {
    let hwnd = OVERLAY_HWND.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Returns the overlay HWND
pub fn get_overlay_hwnd() -> HWND {
    OVERLAY_HWND.load(Ordering::SeqCst) as HWND
}
