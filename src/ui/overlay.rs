use crate::logger::{log_err, log_info};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, InvalidateRect, SetWindowRgn, UpdateWindow, HGDIOBJ,
    RGN_OR,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetWindowLongW, SetParent, SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE,
    HWND_TOP, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNA, WS_CAPTION,
    WS_CHILD, WS_CLIPSIBLINGS, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
};

// Re-export generated component
slint::include_modules!();

static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static OVERLAY_WIDTH: AtomicI32 = AtomicI32::new(0);
static OVERLAY_HEIGHT: AtomicI32 = AtomicI32::new(0);
static CONTROLS_VISIBLE: AtomicBool = AtomicBool::new(true);
static HAS_SUBTITLE: AtomicBool = AtomicBool::new(false);
static ACTIVE_PANEL: AtomicI32 = AtomicI32::new(0);
static LAST_ACTIVITY: Mutex<Option<Instant>> = Mutex::new(None);
static CURRENT_SUB_TEXT: Mutex<String> = Mutex::new(String::new());
static SUB_SIZE_MODE: AtomicI32 = AtomicI32::new(1); // 0=Small, 1=Normal, 2=Large (scaled with video height)


pub fn set_overlay_sub_size(mode: i32) {
    SUB_SIZE_MODE.store(mode, Ordering::SeqCst);
    update_subtitle_window();
}

/// Updates the subtitle layer (per-pixel-alpha layered popup, see `subtitle_layer`).
/// Cheap when nothing changed, so it is safe to call from every UI tick.
pub fn update_subtitle_window() {
    let owner = MAIN_HWND.load(Ordering::SeqCst) as HWND;
    let text = CURRENT_SUB_TEXT.lock().unwrap().clone();
    crate::ui::subtitle_layer::tick(owner, &text, SUB_SIZE_MODE.load(Ordering::SeqCst));
}

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

/// Updates the visible window region for PlayerOverlay using SetWindowRgn.
/// When controls are visible, only the top bar, bottom bar, and any side panel exist as pixels.
/// When controls are hidden, an empty region is applied so the overlay takes 0 pixels.
/// Subtitles are rendered independently by subtitle_layer.
pub fn update_overlay_region(hwnd: HWND, width: i32, height: i32, controls_visible: bool, _has_subtitle: bool, active_panel: i32) {
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

            // Side panel rect if open (width - 340 .. width, 50 .. height - 74)
            if active_panel > 0 {
                let panel_w = 340.min(width);
                let panel_x = width - panel_w;
                let panel_y = 50;
                let panel_h = (height - 74 - 50).max(100);
                let panel_rgn = CreateRectRgn(panel_x, panel_y, width, panel_y + panel_h);
                CombineRgn(combined_rgn, combined_rgn, panel_rgn, RGN_OR);
                DeleteObject(panel_rgn as HGDIOBJ);
            }
        }

        // SetWindowRgn takes ownership of combined_rgn if successful
        let res = SetWindowRgn(hwnd, combined_rgn, 1);
        if res == 0 {
            DeleteObject(combined_rgn as HGDIOBJ);
            log_err("SetWindowRgn failed for PlayerOverlay");
        } else {
            InvalidateRect(hwnd, std::ptr::null(), 1);
            UpdateWindow(hwnd);
            let main_h = MAIN_HWND.load(Ordering::SeqCst) as HWND;
            if !main_h.is_null() {
                InvalidateRect(main_h, std::ptr::null(), 0);
            }
            crate::player::refresh_video_window();
        }
    }
}

/// Applies Win32 child window styling, reparents to MainWindow via SetParent, and sets the initial region
pub fn setup_overlay_window(hwnd: HWND, main_hwnd: HWND, width: i32, height: i32) {
    if hwnd.is_null() {
        return;
    }

    MAIN_HWND.store(main_hwnd as isize, Ordering::SeqCst);

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
        update_subtitle_window();
    }
}

/// Updates playback telemetry on the overlay
pub fn update_overlay_state(time_str: &str, progress: f32, is_paused: bool, sub_text: &str, is_fs: bool) {
    let has_sub = !sub_text.trim().is_empty();
    let prev_has_sub = HAS_SUBTITLE.swap(has_sub, Ordering::SeqCst);
    let text_changed = {
        let mut guard = CURRENT_SUB_TEXT.lock().unwrap();
        if *guard != sub_text {
            *guard = sub_text.to_string();
            true
        } else {
            false
        }
    };
    if (has_sub != prev_has_sub) || text_changed {
        update_subtitle_window();
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
        update_subtitle_window();
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
    crate::ui::subtitle_layer::hide();
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

thread_local! {
    static CACHED_SUBTITLE_ITEMS: RefCell<Vec<SubtitleItem>> = const { RefCell::new(Vec::new()) };
}

fn rebuild_subtitle_groups_on_overlay(ov: &PlayerOverlay, items: &[SubtitleItem]) {
    let cat_filter = ov.get_sub_category_filter().to_string();
    let cur_selected_lang = ov.get_selected_lang_group().to_string();
    let active_lang = ov.get_active_sub_lang().to_string();

    // 1. Collect language groups matching the current category filter ("all", "embedded", "addon")
    let mut lang_order: Vec<String> = Vec::new();
    let mut lang_counts: std::collections::HashMap<String, (i32, bool)> = std::collections::HashMap::new();

    for item in items {
        let src = item.source.as_str();
        if cat_filter != "all" && src != cat_filter {
            continue;
        }
        let lang = item.lang.to_string();
        let entry = lang_counts.entry(lang.clone()).or_insert_with(|| {
            lang_order.push(lang);
            (0, false)
        });
        entry.0 += 1;
        if src == "embedded" {
            entry.1 = true;
        }
    }

    // Sort languages: active language first, then languages with embedded tracks, then alphabetical
    lang_order.sort_by(|a, b| {
        let a_active = a == &active_lang;
        let b_active = b == &active_lang;
        if a_active != b_active {
            return b_active.cmp(&a_active);
        }
        let a_emb = lang_counts.get(a).map(|x| x.1).unwrap_or(false);
        let b_emb = lang_counts.get(b).map(|x| x.1).unwrap_or(false);
        if a_emb != b_emb {
            return b_emb.cmp(&a_emb);
        }
        a.cmp(b)
    });

    let groups: Vec<SubtitleLangGroup> = lang_order
        .iter()
        .filter_map(|l| {
            lang_counts.get(l).map(|(count, has_emb)| SubtitleLangGroup {
                lang: l.clone().into(),
                count: *count,
                has_embedded: *has_emb,
            })
        })
        .collect();

    // If current selected_lang_group no longer exists under this category filter, reset to ""
    let effective_lang = if !cur_selected_lang.is_empty() && lang_counts.contains_key(&cur_selected_lang) {
        cur_selected_lang
    } else if groups.len() == 1 {
        // Auto-expand if only 1 language group exists
        groups[0].lang.to_string()
    } else {
        String::new()
    };

    ov.set_selected_lang_group(effective_lang.clone().into());
    ov.set_language_groups(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(groups))));

    // 2. Populate visible_subtitles for the selected language group (with embedded tracks first)
    let mut visible: Vec<SubtitleItem> = items
        .iter()
        .filter(|item| {
            let src_ok = cat_filter == "all" || item.source.as_str() == cat_filter;
            let lang_ok = effective_lang.is_empty() || item.lang.as_str() == effective_lang;
            src_ok && lang_ok
        })
        .cloned()
        .collect();

    visible.sort_by(|a, b| {
        let a_emb = a.source.as_str() == "embedded";
        let b_emb = b.source.as_str() == "embedded";
        if a_emb != b_emb {
            b_emb.cmp(&a_emb)
        } else {
            a.idx.cmp(&b.idx)
        }
    });

    ov.set_visible_subtitles(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(visible))));
}

/// Updates rich subtitle items with category and vertical language grouping on the overlay
pub fn set_overlay_subtitle_items(items: Vec<SubtitleItem>, available_langs: Vec<String>, selected_idx: usize) {
    CACHED_SUBTITLE_ITEMS.with(|c| {
        *c.borrow_mut() = items.clone();
    });

    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(ov) = guard.as_ref() {
                let active_lang = items
                    .iter()
                    .find(|i| i.idx == selected_idx as i32)
                    .map(|i| i.lang.to_string())
                    .unwrap_or_default();

                ov.set_active_sub_lang(active_lang.into());
                ov.set_selected_lang_group("".into());
                ov.set_sub_category_filter("all".into());
                ov.set_all_subtitles(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(items.clone()))));
                let slint_langs: Vec<slint::SharedString> = available_langs.into_iter().map(|s| s.into()).collect();
                ov.set_available_languages(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(slint_langs))));
                ov.set_selected_subtitle_idx(selected_idx as i32);
                ov.set_is_subtitle_loading(false);
                rebuild_subtitle_groups_on_overlay(ov, &items);
            }
        }
    });
}

/// Selects a language group in the vertical subtitle list (or "" to return to All Languages)
pub fn select_overlay_language_group(lang: &str) {
    CACHED_SUBTITLE_ITEMS.with(|c| {
        let items = c.borrow();
        OVERLAY_INSTANCE.with(|cell| {
            if let Ok(guard) = cell.try_borrow() {
                if let Some(ov) = guard.as_ref() {
                    ov.set_selected_lang_group(lang.into());
                    if lang.is_empty() {
                        // Explicitly keep "" when returning to language groups list even if 1 group
                        let cat_filter = ov.get_sub_category_filter().to_string();
                        let active_lang = ov.get_active_sub_lang().to_string();
                        let mut lang_order: Vec<String> = Vec::new();
                        let mut lang_counts: std::collections::HashMap<String, (i32, bool)> = std::collections::HashMap::new();
                        for item in items.iter() {
                            let src = item.source.as_str();
                            if cat_filter != "all" && src != cat_filter {
                                continue;
                            }
                            let l = item.lang.to_string();
                            let entry = lang_counts.entry(l.clone()).or_insert_with(|| {
                                lang_order.push(l);
                                (0, false)
                            });
                            entry.0 += 1;
                            if src == "embedded" {
                                entry.1 = true;
                            }
                        }
                        lang_order.sort_by(|a, b| {
                            let a_active = a == &active_lang;
                            let b_active = b == &active_lang;
                            if a_active != b_active {
                                return b_active.cmp(&a_active);
                            }
                            let a_emb = lang_counts.get(a).map(|x| x.1).unwrap_or(false);
                            let b_emb = lang_counts.get(b).map(|x| x.1).unwrap_or(false);
                            if a_emb != b_emb {
                                return b_emb.cmp(&a_emb);
                            }
                            a.cmp(b)
                        });
                        let groups: Vec<SubtitleLangGroup> = lang_order
                            .iter()
                            .filter_map(|l| {
                                lang_counts.get(l).map(|(count, has_emb)| SubtitleLangGroup {
                                    lang: l.clone().into(),
                                    count: *count,
                                    has_embedded: *has_emb,
                                })
                            })
                            .collect();
                        ov.set_language_groups(slint::ModelRc::from(std::rc::Rc::new(slint::VecModel::from(groups))));
                    } else {
                        rebuild_subtitle_groups_on_overlay(ov, &items);
                    }
                }
            }
        });
    });
}

/// Selects the subtitle source category filter ("all", "embedded", "addon")
pub fn select_overlay_category_filter(category: &str) {
    CACHED_SUBTITLE_ITEMS.with(|c| {
        let items = c.borrow();
        OVERLAY_INSTANCE.with(|cell| {
            if let Ok(guard) = cell.try_borrow() {
                if let Some(ov) = guard.as_ref() {
                    ov.set_sub_category_filter(category.into());
                    rebuild_subtitle_groups_on_overlay(ov, &items);
                }
            }
        });
    });
}

/// Updates whether a subtitle is currently downloading
pub fn set_overlay_subtitle_loading(loading: bool) {
    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(ov) = guard.as_ref() {
                ov.set_is_subtitle_loading(loading);
            }
        }
    });
}

/// Updates the seek step interval in seconds on the overlay
pub fn set_overlay_seek_step(step: i32) {
    OVERLAY_INSTANCE.with(|cell| {
        if let Ok(guard) = cell.try_borrow() {
            if let Some(ov) = guard.as_ref() {
                ov.set_seek_step_s(step);
            }
        }
    });
}
