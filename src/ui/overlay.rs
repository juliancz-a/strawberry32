use crate::logger::{log_err, log_info};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, InvalidateRect, SetWindowRgn, UpdateWindow, HGDIOBJ, RGN_OR,
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
static ACTIVE_PANEL: AtomicI32 = AtomicI32::new(0);
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
/// When controls are visible, only the top bar and bottom transport bar exist as pixels.
/// When controls are hidden and a subtitle is present, only a full-width bottom subtitle bar exists.
/// The video area between them is completely excluded from the window, allowing
/// video to show through without color-keying, transparency bugs, or dirty framebuffer artifacts.
pub fn update_overlay_region(hwnd: HWND, width: i32, height: i32, controls_visible: bool, has_subtitle: bool, active_panel: i32) {
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

            // Bottom transport bar rect: 0..width, (height - bot_h)..height
            // If subtitles are active, bottom bar expands to 106px to cleanly house the subtitle
            let bot_h = if has_subtitle { 106 } else { 74 };
            let bottom_y = (height - bot_h).max(0);
            let bottom_rgn = CreateRectRgn(0, bottom_y, width, height);
            CombineRgn(combined_rgn, combined_rgn, bottom_rgn, RGN_OR);
            DeleteObject(bottom_rgn as HGDIOBJ);

            // Side panel rect if open (width - 340 .. width, 50 .. height - bot_h)
            if active_panel > 0 {
                let panel_w = 340.min(width);
                let panel_x = width - panel_w;
                let panel_y = 50;
                let panel_h = (height - bot_h - 50).max(100);
                let panel_rgn = CreateRectRgn(panel_x, panel_y, width, panel_y + panel_h);
                CombineRgn(combined_rgn, combined_rgn, panel_rgn, RGN_OR);
                DeleteObject(panel_rgn as HGDIOBJ);
            }
        } else if has_subtitle {
            // Full-width subtitle banner at bottom: 0..width, (height - 52)..height
            let sub_y = (height - 52).max(0);
            let sub_rgn = CreateRectRgn(0, sub_y, width, height);
            CombineRgn(combined_rgn, combined_rgn, sub_rgn, RGN_OR);
            DeleteObject(sub_rgn as HGDIOBJ);
        }

        // SetWindowRgn takes ownership of combined_rgn if successful
        let res = SetWindowRgn(hwnd, combined_rgn, 1);
        if res == 0 {
            DeleteObject(combined_rgn as HGDIOBJ);
            log_err("SetWindowRgn failed for PlayerOverlay");
        } else {
            InvalidateRect(hwnd, std::ptr::null(), 1);
            UpdateWindow(hwnd);
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
        update_overlay_region(hwnd, width, height, true, false, 0);
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
    let existing = OVERLAY_INSTANCE.with(|cell| {
        cell.try_borrow().ok().and_then(|g| g.as_ref().map(|o| o.clone_strong()))
    });

    if let Some(existing) = existing {
        position_overlay(main_hwnd, x, y, width, height);
        show_overlay();
        notify_user_activity();
        return Ok(existing);
    }

    let overlay = PlayerOverlay::new()
        .map_err(|e| format!("Failed to create PlayerOverlay: {}", e))?;

    // 1. Show FIRST so winit creates and maps the native HWND
    overlay.show().map_err(|e| format!("Failed to show PlayerOverlay: {}", e))?;

    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(mut g) = cell.try_borrow_mut() {
            *g = Some(overlay.clone_strong());
        }
    });

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

    Ok(overlay)
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
        let panel = ACTIVE_PANEL.load(Ordering::SeqCst);
        update_overlay_region(hwnd, width, height, vis, sub, panel);
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
        let panel = ACTIVE_PANEL.load(Ordering::SeqCst);
        if !hwnd.is_null() && w > 0 && h > 0 {
            update_overlay_region(hwnd, w, h, vis, has_sub, panel);
        }
    }

    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(overlay) = guard.as_ref() {
                overlay.set_time_str(time_str.into());
                overlay.set_progress(progress);
                overlay.set_is_paused(is_paused);
                overlay.set_subtitle_text(sub_text.into());
                overlay.set_is_fullscreen(is_fs);
            }
        }
    });
}

/// Sets active panel on the overlay (0=None, 1=Subtitles, 2=Info, 3=Volume)
pub fn set_active_panel(panel: i32) {
    ACTIVE_PANEL.store(panel, Ordering::SeqCst);
    let hwnd = OVERLAY_HWND.load(Ordering::SeqCst) as HWND;
    let w = OVERLAY_WIDTH.load(Ordering::SeqCst);
    let h = OVERLAY_HEIGHT.load(Ordering::SeqCst);
    let vis = CONTROLS_VISIBLE.load(Ordering::SeqCst);
    let sub = HAS_SUBTITLE.load(Ordering::SeqCst);

    if !hwnd.is_null() && w > 0 && h > 0 {
        update_overlay_region(hwnd, w, h, vis, sub, panel);
    }

    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(overlay) = guard.as_ref() {
                overlay.set_active_panel(panel);
            }
        }
    });
}

/// Returns current active panel (0=None, 1=Subtitles, 2=Info, 3=Volume)
pub fn get_active_panel() -> i32 {
    ACTIVE_PANEL.load(Ordering::SeqCst)
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
    let panel = ACTIVE_PANEL.load(Ordering::SeqCst);

    if !hwnd.is_null() && w > 0 && h > 0 {
        update_overlay_region(hwnd, w, h, visible, sub, panel);
    }

    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(overlay) = guard.as_ref() {
                overlay.set_controls_visible(visible);
            }
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

/// Updates the available subtitle tracks and active selection on the overlay
pub fn set_overlay_subtitle_tracks(track_names: Vec<String>, selected_idx: usize) {
    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(ov) = guard.as_ref() {
                let slint_names: Vec<slint::SharedString> = track_names.into_iter().map(|s| s.into()).collect();
                ov.set_subtitle_track_names(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(slint_names))));
                ov.set_selected_subtitle_idx(selected_idx as i32);
            }
        }
    });
}
