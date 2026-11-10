//! Per-pixel-alpha subtitle layer.
//!
//! Subtitles are rendered into a 32-bit premultiplied BGRA DIB (anti-aliased yellow fill with a
//! smooth dark outline) and presented through `UpdateLayeredWindow` on a dedicated layered,
//! click-through, non-activating popup window *owned* by the main window.
//!
//! Why a popup instead of a child window / `SetWindowRgn`:
//! * A region is a 1-bit mask (no alpha), so outlines look like a jagged box.
//! * Child windows over the EVR/DXVA video HWND are not reliably composited, and any unpainted
//!   pixel under a `WS_CLIPCHILDREN` parent shows the desktop (fullscreen bleed).
//! * Layered top-level windows are blended by DWM over whatever is below (video, UI, desktop)
//!   and work on Windows 7/8/8.1 RT without a manifest.
//!
//! Taskbar / Alt-Tab: the popup is owned (no taskbar button), `WS_EX_TOOLWINDOW` and
//! `WS_EX_NOACTIVATE`, and is never shown before those styles are applied.
//!
//! All functions must be called from the UI thread.

use crate::logger::{log_err, log_info};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GdiFlush, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
    ANTIALIASED_QUALITY, BITMAPINFO, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_TOP, DT_WORDBREAK,
    FF_SWISS, FW_BOLD, HGDIOBJ, OUT_TT_PRECIS, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetAncestor, GetClientRect, GetForegroundWindow, IsIconic,
    IsWindowVisible, RegisterClassExW, ShowWindow, UpdateLayeredWindow, GA_ROOT, HTTRANSPARENT,
    SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

const AC_SRC_OVER: u8 = 0;
const AC_SRC_ALPHA: u8 = 1;

/// Space (unscaled px) kept free at the bottom for the transport bar (74px + 12px margin).
const BOTTOM_RESERVED: i32 = 86;

static LAYER_HWND: AtomicIsize = AtomicIsize::new(0);

#[derive(PartialEq, Clone, Default)]
struct RenderKey {
    text: String,
    mode: i32,
    client_w: i32,
    client_h: i32,
    screen_x: i32,
    screen_y: i32,
}

struct LayerState {
    key: RenderKey,
    shown: bool,
}

static STATE: Mutex<LayerState> = Mutex::new(LayerState {
    key: RenderKey {
        text: String::new(),
        mode: 0,
        client_w: 0,
        client_h: 0,
        screen_x: 0,
        screen_y: 0,
    },
    shown: false,
});

unsafe extern "system" fn layer_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_MOUSEACTIVATE: u32 = 0x0021;
    const WM_NCHITTEST: u32 = 0x0084;
    const MA_NOACTIVATE: isize = 3;

    match msg {
        WM_NCHITTEST => HTTRANSPARENT as isize,
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn ensure_window(owner: HWND) -> HWND {
    let existing = LAYER_HWND.load(Ordering::SeqCst) as HWND;
    if !existing.is_null() {
        return existing;
    }
    if owner.is_null() {
        return std::ptr::null_mut();
    }

    unsafe {
        let class_name: Vec<u16> = "StrawberrySubtitleLayer\0".encode_utf16().collect();
        let hinst = GetModuleHandleW(std::ptr::null());

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(layer_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);

        // Created hidden (no WS_VISIBLE) with tool-window + no-activate styles from the start so
        // Windows never registers a taskbar button / Alt-Tab entry for it.
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            owner,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null_mut(),
        );

        if hwnd.is_null() {
            let err = windows_sys::Win32::Foundation::GetLastError();
            log_err(&format!("SubtitleLayer: CreateWindowExW failed, error {}", err));
            return std::ptr::null_mut();
        }
        LAYER_HWND.store(hwnd as isize, Ordering::SeqCst);
        log_info(&format!(
            "SubtitleLayer: layered popup created (0x{:X}), owner 0x{:X}",
            hwnd as usize, owner as usize
        ));
        hwnd
    }
}

/// Hides the subtitle layer (leaving the player, minimising, losing focus, no active cue).
pub fn hide() {
    let hwnd = LAYER_HWND.load(Ordering::SeqCst) as HWND;
    let mut st = STATE.lock().unwrap();
    if !hwnd.is_null() && st.shown {
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
    st.shown = false;
    // Force a re-render next time it becomes visible.
    st.key = RenderKey::default();
}

/// Synchronises the layer with the current cue text, size mode and the owner's client rect.
/// Cheap when nothing changed (only visibility checks), so it is safe to call every UI tick.
pub fn tick(owner: HWND, text: &str, mode: i32) {
    if owner.is_null() {
        return;
    }

    let text = text.trim();
    let (foreground_ok, rc, origin) = unsafe {
        let iconic = IsIconic(owner) != 0;
        let visible = IsWindowVisible(owner) != 0;
        let fg = GetForegroundWindow();
        let fg_ok = !fg.is_null() && GetAncestor(fg, GA_ROOT) == owner;

        let mut rc: RECT = std::mem::zeroed();
        GetClientRect(owner, &mut rc);
        let mut pt = POINT { x: 0, y: 0 };
        ClientToScreen(owner, &mut pt);
        (!iconic && visible && fg_ok, rc, pt)
    };

    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if text.is_empty() || cw <= 0 || ch <= 0 || !foreground_ok {
        hide();
        return;
    }

    let hwnd = ensure_window(owner);
    if hwnd.is_null() {
        return;
    }

    let key = RenderKey {
        text: text.to_string(),
        mode,
        client_w: cw,
        client_h: ch,
        screen_x: origin.x,
        screen_y: origin.y,
    };

    let mut st = STATE.lock().unwrap();
    if st.shown && st.key == key {
        return;
    }

    if present(hwnd, &key) {
        st.key = key;
        if !st.shown {
            unsafe {
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            st.shown = true;
        }
    } else {
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
        st.shown = false;
    }
}

fn font_px_for(client_h: i32, mode: i32) -> i32 {
    let scale = match mode {
        0 => 0.036,
        2 => 0.056,
        _ => 0.045,
    };
    ((client_h as f32 * scale) as i32).clamp(16, 72)
}

/// Pixel offsets (dx, dy, weight 0..=255) of a soft-edged disc used to dilate the glyph coverage
/// into an anti-aliased outline.
fn disc_kernel(radius: i32) -> Vec<(i32, i32, u32)> {
    let mut k = Vec::new();
    let r = radius as f32;
    for dy in -radius - 1..=radius + 1 {
        for dx in -radius - 1..=radius + 1 {
            let dist = ((dx * dx + dy * dy) as f32).sqrt();
            let w = (r + 0.5 - dist).clamp(0.0, 1.0);
            if w > 0.0 {
                k.push((dx, dy, (w * 255.0) as u32));
            }
        }
    }
    k
}

/// Renders the subtitle bitmap and pushes it to the layered window. Returns false on failure.
fn present(hwnd: HWND, key: &RenderKey) -> bool {
    let font_px = font_px_for(key.client_h, key.mode);
    let outline_r = (font_px / 9).max(2);
    let margin = outline_r + 2;

    let wide: Vec<u16> = key.text.encode_utf16().collect();
    if wide.is_empty() {
        return false;
    }
    let font_name: Vec<u16> = "Segoe UI\0".encode_utf16().collect();

    unsafe {
        let screen_dc = GetDC(std::ptr::null_mut());
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_null() {
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            log_err("SubtitleLayer: CreateCompatibleDC failed");
            return false;
        }

        let hfont = CreateFontW(
            -font_px,
            0,
            0,
            0,
            FW_BOLD as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_TT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            // Greyscale AA: ClearType is invalid when the result is used as alpha.
            ANTIALIASED_QUALITY as u32,
            (DEFAULT_PITCH | FF_SWISS) as u32,
            font_name.as_ptr(),
        );
        let old_font = SelectObject(mem_dc, hfont as HGDIOBJ);

        // 1. Measure
        let max_text_w = ((key.client_w as f32 * 0.8) as i32).clamp(240, 1100).min(key.client_w - 2 * margin).max(120);
        let mut calc = RECT { left: 0, top: 0, right: max_text_w, bottom: 0 };
        DrawTextW(
            mem_dc,
            wide.as_ptr(),
            wide.len() as i32,
            &mut calc,
            DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
        );
        let text_w = (calc.right - calc.left).max(1);
        let text_h = (calc.bottom - calc.top).max(1);
        let w = text_w + 2 * margin;
        let h = text_h + 2 * margin;

        // 2. 32-bit top-down DIB
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of_val(&bmi.bmiHeader) as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if dib.is_null() || bits.is_null() {
            log_err("SubtitleLayer: CreateDIBSection failed");
            SelectObject(mem_dc, old_font);
            DeleteObject(hfont as HGDIOBJ);
            DeleteDC(mem_dc);
            ReleaseDC(std::ptr::null_mut(), screen_dc);
            return false;
        }
        let old_bmp = SelectObject(mem_dc, dib as HGDIOBJ);

        let npx = (w * h) as usize;
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, npx);
        px.fill(0);

        // 3. Draw white text on black -> coverage in the R channel
        SetBkMode(mem_dc, TRANSPARENT as i32);
        SetTextColor(mem_dc, 0x00FFFFFF);
        let mut rc = RECT { left: margin, top: margin, right: w - margin, bottom: h - margin };
        DrawTextW(
            mem_dc,
            wide.as_ptr(),
            wide.len() as i32,
            &mut rc,
            DT_CENTER | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
        );
        GdiFlush();

        let cover: Vec<u8> = px.iter().map(|p| ((p >> 16) & 0xFF) as u8).collect();

        // 4. Dilate coverage -> smooth outline
        let kernel = disc_kernel(outline_r);
        let mut outline = vec![0u8; npx];
        for y in 0..h {
            let row = (y * w) as usize;
            for x in 0..w {
                let f = cover[row + x as usize] as u32;
                if f == 0 {
                    continue;
                }
                for &(dx, dy, wt) in &kernel {
                    let nx = x + dx;
                    let ny = y + dy;
                    if nx < 0 || ny < 0 || nx >= w || ny >= h {
                        continue;
                    }
                    let v = (f * wt / 255) as u8;
                    let idx = (ny * w + nx) as usize;
                    if outline[idx] < v {
                        outline[idx] = v;
                    }
                }
            }
        }

        // 5. Compose premultiplied BGRA: yellow #FFFF55 fill over black outline
        for i in 0..npx {
            let f = cover[i] as u32;
            let o = outline[i] as u32;
            let a = f + o * (255 - f) / 255;
            let r = 0xFF * f / 255;
            let g = 0xFF * f / 255;
            let b = 0x55 * f / 255;
            px[i] = (a << 24) | (r << 16) | (g << 8) | b;
        }

        // 6. Position: centred, anchored above the transport bar (screen coordinates)
        let x = key.screen_x + (key.client_w - w) / 2;
        let y = key.screen_y + (key.client_h - BOTTOM_RESERVED - h).max(10);

        let pt_dst = POINT { x, y };
        let size = SIZE { cx: w, cy: h };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA,
        };
        let ok = UpdateLayeredWindow(
            hwnd,
            screen_dc,
            &pt_dst,
            &size,
            mem_dc,
            &pt_src,
            0,
            &blend,
            ULW_ALPHA,
        ) != 0;
        if !ok {
            let err = windows_sys::Win32::Foundation::GetLastError();
            log_err(&format!("SubtitleLayer: UpdateLayeredWindow failed, error {}", err));
        }

        #[cfg(debug_assertions)]
        dump_bmp(px, w, h);

        SelectObject(mem_dc, old_bmp);
        DeleteObject(dib as HGDIOBJ);
        SelectObject(mem_dc, old_font);
        DeleteObject(hfont as HGDIOBJ);
        DeleteDC(mem_dc);
        ReleaseDC(std::ptr::null_mut(), screen_dc);
        ok
    }
}

/// Debug builds: dumps the last composed bitmap (BGRA, 32-bit) next to the exe's log folder so the
/// pixels can be inspected without the device compositor.
#[cfg(debug_assertions)]
fn dump_bmp(px: &[u32], w: i32, h: i32) {
    let dir = crate::state::get_app_storage_root().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let data_len = (w * h * 4) as u32;
    let mut out = Vec::with_capacity(54 + data_len as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(54 + data_len).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&(-h).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);
    for p in px {
        out.extend_from_slice(&p.to_le_bytes());
    }
    let _ = std::fs::write(dir.join("sub_debug.bmp"), out);
}

