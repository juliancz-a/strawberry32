use crate::addons::{self, CompatibleStream};
use crate::engine;
use crate::logger::{self, log_err, log_info, log_player};
use crate::player;
use crate::state;
use crate::subtitles;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, FindWindowW, GetForegroundWindow, MoveWindow, RegisterClassExW,
    ShowWindow, CS_HREDRAW, CS_VREDRAW, SW_HIDE, SW_MAXIMIZE, SW_RESTORE, SW_SHOW, WNDCLASSEXW,
    WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};

#[link(name = "imm32")]
extern "system" {}

slint::slint! {
    import { Button, LineEdit, ScrollView } from "std-widgets.slint";

    export struct CatalogItem {
        id: string,
        title: string,
        year: string,
        item_type: string,
    }

    export struct StreamItem {
        idx: int,
        display_title: string,
        badge: string,
        badge_color: color,
        quality: string,
        size: string,
        seeders: int,
        is_mp4: bool,
    }

    export component MainWindow inherits Window {
        preferred-width: 960px;
        preferred-height: 640px;
        min-width: 640px;
        min-height: 480px;
        title: "RT-Stream - Surface RT (ARM32)";
        background: #101216;

        // View state: 0 = Catalog, 1 = Detail, 2 = Player
        in-out property <int> current_view: 0;

        // Top Bar properties
        in-out property <string> search_text: "";
        in-out property <string> active_filter: "all";
        in-out property <string> status_text: "Ready. Click Quick Setup or Search.";

        // Catalog Model
        in-out property <[CatalogItem]> catalog_items: [];
        in-out property <bool> is_catalog_loading: false;

        // Detail Model
        in-out property <string> detail_id: "";
        in-out property <string> detail_title: "";
        in-out property <string> detail_type: "";
        in-out property <string> detail_year: "";
        in-out property <string> detail_desc: "";
        in-out property <bool> detail_is_series: false;
        in-out property <string> series_season: "1";
        in-out property <string> series_episode: "1";
        in-out property <[StreamItem]> stream_items: [];
        in-out property <bool> is_streams_loading: false;

        // Player View Model
        in-out property <bool> is_player_loading: false;
        in-out property <string> player_title: "";
        in-out property <string> player_status_msg: "";
        in-out property <string> player_peers_str: "";
        in-out property <string> player_speed_str: "";
        in-out property <string> player_progress_str: "";
        in-out property <string> time_str: "--:-- / --:--";
        in-out property <string> subtitle_text: "";
        in-out property <bool> is_paused: false;
        in-out property <bool> is_fullscreen: false;

        // Diagnostic Logs Drawer
        in-out property <bool> logs_visible: false;
        in-out property <[string]> log_lines: [];

        // Callbacks to Rust
        callback search_submitted(string);
        callback filter_clicked(string);
        callback quick_dev_setup();
        callback toggle_logs();
        callback clear_logs();

        callback card_selected(string);
        callback back_to_catalog();
        callback fetch_episode_streams(string, string, string);

        callback play_stream_clicked(int);
        callback play_vlc_clicked(int);

        callback player_back();
        callback player_play_pause();
        callback player_seek_backward();
        callback player_seek_forward();
        callback player_cycle_subtitles();
        callback player_toggle_fullscreen();
        callback player_open_vlc();

        VerticalLayout {
            padding: 0px;
            spacing: 0px;

            // TOP NAVIGATION BAR (Visible in Catalog & Detail views)
            if root.current_view != 2: Rectangle {
                height: 56px;
                background: #161922;
                border-width: 1px;
                border-color: #242938;

                HorizontalLayout {
                    padding-left: 14px;
                    padding-right: 14px;
                    padding-top: 8px;
                    padding-bottom: 8px;
                    spacing: 10px;

                    // Brand title
                    Text {
                        text: "⚡ RT-STREAM";
                        font-size: 16px;
                        font-weight: 800;
                        color: #A855F7;
                        vertical-alignment: center;
                    }

                    // Search input
                    search_edit := LineEdit {
                        width: 200px;
                        placeholder-text: "Search movies or series...";
                        text <=> root.search_text;
                        accepted => { root.search_submitted(self.text); }
                    }

                    Button {
                        text: "🔍 Search";
                        clicked => { root.search_submitted(root.search_text); }
                    }

                    // Filter chips
                    Button {
                        text: root.active_filter == "all" ? "• All •" : "All";
                        clicked => { root.filter_clicked("all"); }
                    }

                    Button {
                        text: root.active_filter == "movie" ? "• Movies •" : "Movies";
                        clicked => { root.filter_clicked("movie"); }
                    }

                    Button {
                        text: root.active_filter == "series" ? "• Series •" : "Series";
                        clicked => { root.filter_clicked("series"); }
                    }

                    // Quick load dev button
                    Button {
                        text: "⚡ Quick Setup";
                        clicked => { root.quick_dev_setup(); }
                    }

                    // Logs toggle button
                    Button {
                        text: root.logs_visible ? "⚙ Hide Logs" : "⚙ Logs";
                        clicked => { root.toggle_logs(); }
                    }
                }
            }

            // STATUS SUB-BAR (Catalog & Detail)
            if root.current_view != 2: Rectangle {
                height: 24px;
                background: #111319;
                HorizontalLayout {
                    padding-left: 14px;
                    padding-right: 14px;
                    Text {
                        text: root.status_text;
                        font-size: 11px;
                        color: #94A3B8;
                        vertical-alignment: center;
                    }
                }
            }

            // VIEW 0: CATALOG VIEW
            if root.current_view == 0: Rectangle {
                background: #101216;

                if root.is_catalog_loading: Text {
                    text: "Loading catalog from Cinemeta...";
                    font-size: 16px;
                    color: #94A3B8;
                    horizontal-alignment: center;
                    vertical-alignment: center;
                }

                if !root.is_catalog_loading && root.catalog_items.length == 0: VerticalLayout {
                    alignment: center;
                    spacing: 12px;
                    Text {
                        text: "No titles loaded yet.";
                        font-size: 18px;
                        font-weight: 700;
                        color: #E2E8F0;
                        horizontal-alignment: center;
                    }
                    Text {
                        text: "Click [⚡ Quick Setup] to load Cinemeta + Torrentio, or enter a search query.";
                        font-size: 13px;
                        color: #94A3B8;
                        horizontal-alignment: center;
                    }
                }

                if !root.is_catalog_loading && root.catalog_items.length > 0: ScrollView {
                    VerticalLayout {
                        padding: 12px;
                        spacing: 8px;

                        for item[i] in root.catalog_items: Rectangle {
                            height: 74px;
                            background: item_touch.has-hover ? #1F2432 : #151820;
                            border-radius: 8px;
                            border-width: 1px;
                            border-color: #272D3D;

                            item_touch := TouchArea {
                                clicked => { root.card_selected(item.id); }
                            }

                            HorizontalLayout {
                                padding-left: 12px;
                                padding-right: 14px;
                                padding-top: 8px;
                                padding-bottom: 8px;
                                spacing: 14px;

                                // Media icon placeholder
                                Rectangle {
                                    width: 44px;
                                    height: 56px;
                                    background: #232838;
                                    border-radius: 6px;
                                    Text {
                                        text: item.item_type == "movie" ? "🎬" : "📺";
                                        font-size: 22px;
                                        horizontal-alignment: center;
                                        vertical-alignment: center;
                                    }
                                }

                                VerticalLayout {
                                    alignment: center;
                                    spacing: 4px;

                                    HorizontalLayout {
                                        spacing: 10px;

                                        Text {
                                            text: item.title;
                                            font-size: 15px;
                                            font-weight: 700;
                                            color: #FFFFFF;
                                        }

                                        Rectangle {
                                            width: 54px;
                                            height: 18px;
                                            background: item.item_type == "movie" ? rgb(124, 58, 237) : rgb(37, 99, 235);
                                            border-radius: 4px;
                                            Text {
                                                text: item.item_type == "movie" ? "MOVIE" : "SERIES";
                                                font-size: 10px;
                                                font-weight: 700;
                                                color: #FFFFFF;
                                                horizontal-alignment: center;
                                                vertical-alignment: center;
                                            }
                                        }
                                    }

                                    Text {
                                        text: item.year != "" ? item.year : "Popular";
                                        font-size: 12px;
                                        color: #94A3B8;
                                    }
                                }

                                Text {
                                    text: "Streams ▶";
                                    font-size: 13px;
                                    font-weight: 600;
                                    color: #A855F7;
                                    horizontal-alignment: right;
                                    vertical-alignment: center;
                                }
                            }
                        }
                    }
                }
            }

            // VIEW 1: TITLE DETAIL & STREAMS VIEW
            if root.current_view == 1: VerticalLayout {
                padding: 12px;
                spacing: 10px;

                // Header with back button
                HorizontalLayout {
                    spacing: 12px;
                    Button {
                        text: "← Back to Catalog";
                        clicked => { root.back_to_catalog(); }
                    }

                    Text {
                        text: root.detail_title;
                        font-size: 18px;
                        font-weight: 800;
                        color: #FFFFFF;
                        vertical-alignment: center;
                    }

                    Rectangle {
                        width: 54px;
                        height: 20px;
                        background: root.detail_type == "movie" ? rgb(124, 58, 237) : rgb(37, 99, 235);
                        border-radius: 4px;
                        Text {
                            text: root.detail_type == "movie" ? "MOVIE" : "SERIES";
                            font-size: 10px;
                            font-weight: 700;
                            color: #FFFFFF;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                    }

                    Text {
                        text: root.detail_year;
                        font-size: 13px;
                        color: #94A3B8;
                        vertical-alignment: center;
                    }
                }

                // Synopsis
                if root.detail_desc != "": Rectangle {
                    background: #161922;
                    border-radius: 6px;
                    HorizontalLayout {
                        padding: 8px;
                        Text {
                            text: root.detail_desc;
                            font-size: 12px;
                            color: #CBD5E1;
                            wrap: word-wrap;
                        }
                    }
                }

                // Series Season & Episode selectors
                if root.detail_is_series: HorizontalLayout {
                    spacing: 10px;
                    Text {
                        text: "Season:";
                        color: #FFFFFF;
                        vertical-alignment: center;
                    }
                    season_input := LineEdit {
                        width: 50px;
                        text <=> root.series_season;
                    }
                    Text {
                        text: "Episode:";
                        color: #FFFFFF;
                        vertical-alignment: center;
                    }
                    episode_input := LineEdit {
                        width: 50px;
                        text <=> root.series_episode;
                    }
                    Button {
                        text: "Fetch Streams";
                        clicked => {
                            root.fetch_episode_streams(root.detail_id, root.series_season, root.series_episode);
                        }
                    }
                }

                // Streams list header
                Text {
                    text: "Available Streams (Sorted by Surface RT Tegra 3 Compatibility):";
                    font-size: 13px;
                    font-weight: 700;
                    color: #A855F7;
                }

                if root.is_streams_loading: Text {
                    text: "Querying Torrentio for streams...";
                    font-size: 14px;
                    color: #94A3B8;
                }

                if !root.is_streams_loading && root.stream_items.length == 0: Text {
                    text: "No compatible H.264 streams found for this title.";
                    font-size: 13px;
                    color: #EF4444;
                }

                if !root.is_streams_loading && root.stream_items.length > 0: ScrollView {
                    VerticalLayout {
                        spacing: 8px;

                        for stream[i] in root.stream_items: Rectangle {
                            height: 66px;
                            background: #151820;
                            border-radius: 6px;
                            border-width: 1px;
                            border-color: #272D3D;

                            HorizontalLayout {
                                padding-left: 10px;
                                padding-right: 10px;
                                padding-top: 6px;
                                padding-bottom: 6px;
                                spacing: 10px;

                                // Badge
                                Rectangle {
                                    width: 120px;
                                    height: 24px;
                                    background: stream.badge_color;
                                    border-radius: 4px;
                                    Text {
                                        text: stream.badge;
                                        font-size: 10px;
                                        font-weight: 700;
                                        color: #FFFFFF;
                                        horizontal-alignment: center;
                                        vertical-alignment: center;
                                    }
                                }

                                VerticalLayout {
                                    alignment: center;
                                    spacing: 2px;
                                    Text {
                                        text: stream.display_title;
                                        font-size: 12px;
                                        font-weight: 600;
                                        color: #FFFFFF;
                                        overflow: elide;
                                    }
                                    Text {
                                        text: "Resolution: " + stream.quality + " | Size: " + stream.size + " | Seeders: " + stream.seeders;
                                        font-size: 11px;
                                        color: #94A3B8;
                                    }
                                }

                                HorizontalLayout {
                                    spacing: 6px;
                                    alignment: center;

                                    Button {
                                        text: "▶ Play";
                                        clicked => { root.play_stream_clicked(stream.idx); }
                                    }

                                    Button {
                                        text: "🚀 VLC";
                                        clicked => { root.play_vlc_clicked(stream.idx); }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // VIEW 2: EMBEDDED PLAYER VIEW
            if root.current_view == 2: VerticalLayout {
                spacing: 0px;

                // IN-PLAYER LOADING OVERLAY (Stremio / Nuvio Style)
                if root.is_player_loading: Rectangle {
                    background: #0B0D11;

                    VerticalLayout {
                        padding: 20px;
                        spacing: 16px;

                        HorizontalLayout {
                            Button {
                                text: "← Cancel & Return to Catalog";
                                clicked => { root.player_back(); }
                            }
                            Rectangle {}
                        }

                        VerticalLayout {
                            alignment: center;
                            spacing: 14px;

                            Text {
                                text: root.player_title;
                                font-size: 22px;
                                font-weight: 800;
                                color: #FFFFFF;
                                horizontal-alignment: center;
                            }

                            Text {
                                text: root.player_status_msg;
                                font-size: 14px;
                                font-weight: 600;
                                color: #38BDF8;
                                horizontal-alignment: center;
                            }

                            Rectangle {
                                width: 400px;
                                height: 110px;
                                background: #161922;
                                border-radius: 8px;
                                border-width: 1px;
                                border-color: #272D3D;

                                VerticalLayout {
                                    padding: 12px;
                                    spacing: 6px;
                                    alignment: center;

                                    Text {
                                        text: "Swarm Peers: " + root.player_peers_str;
                                        font-size: 13px;
                                        color: #E2E8F0;
                                        horizontal-alignment: center;
                                    }
                                    Text {
                                        text: "Download Speed: " + root.player_speed_str;
                                        font-size: 13px;
                                        font-weight: 700;
                                        color: #10B981;
                                        horizontal-alignment: center;
                                    }
                                    Text {
                                        text: "Initial Buffer Progress: " + root.player_progress_str;
                                        font-size: 12px;
                                        color: #94A3B8;
                                        horizontal-alignment: center;
                                    }
                                }
                            }

                            Text {
                                text: "⚡ Tegra 3 Hardware H.264 Acceleration Active";
                                font-size: 12px;
                                color: #A855F7;
                                horizontal-alignment: center;
                            }
                        }
                    }
                }

                // IN-PLAYER ACTIVE VIDEO VIEW (When stream is ready)
                if !root.is_player_loading: VerticalLayout {
                    spacing: 0px;

                    // Player Top Bar
                    Rectangle {
                        height: 44px;
                        background: #161922;
                        HorizontalLayout {
                            padding-left: 12px;
                            padding-right: 12px;
                            spacing: 12px;

                            Button {
                                text: "← Back";
                                clicked => { root.player_back(); }
                            }

                            Text {
                                text: root.player_title;
                                font-size: 14px;
                                font-weight: 700;
                                color: #FFFFFF;
                                vertical-alignment: center;
                            }
                        }
                    }

                    // Video Viewport Area (Native Win32 child HWND sits over this)
                    Rectangle {
                        background: #000000;

                        // Subtitle overlay
                        if root.subtitle_text != "": VerticalLayout {
                            alignment: end;
                            padding-bottom: 20px;
                            HorizontalLayout {
                                alignment: center;
                                Rectangle {
                                    background: #000000CC;
                                    border-radius: 6px;
                                    HorizontalLayout {
                                        padding-left: 14px;
                                        padding-right: 14px;
                                        padding-top: 6px;
                                        padding-bottom: 6px;
                                        Text {
                                            text: root.subtitle_text;
                                            font-size: 16px;
                                            font-weight: 700;
                                            color: #FFFF55;
                                            horizontal-alignment: center;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Player Bottom Transport Bar
                    Rectangle {
                        height: 60px;
                        background: #161922;
                        border-width: 1px;
                        border-color: #242938;

                        HorizontalLayout {
                            padding-left: 14px;
                            padding-right: 14px;
                            spacing: 10px;
                            alignment: center;

                            Button {
                                text: "⏮ -15s";
                                clicked => { root.player_seek_backward(); }
                            }

                            Button {
                                text: root.is_paused ? "▶ Play" : "⏸ Pause";
                                clicked => { root.player_play_pause(); }
                            }

                            Button {
                                text: "⏭ +30s";
                                clicked => { root.player_seek_forward(); }
                            }

                            Text {
                                text: root.time_str;
                                font-size: 13px;
                                font-weight: 600;
                                color: #FFFFFF;
                                vertical-alignment: center;
                            }

                            Button {
                                text: "💬 Subs";
                                clicked => { root.player_cycle_subtitles(); }
                            }

                            Button {
                                text: root.is_fullscreen ? "🗗 Restore" : "⛶ Fullscreen";
                                clicked => { root.player_toggle_fullscreen(); }
                            }

                            Button {
                                text: "🚀 Ext. VLC";
                                clicked => { root.player_open_vlc(); }
                            }
                        }
                    }
                }
            }

            // DIAGNOSTIC LOGS DRAWER (Bottom collapsible)
            if root.logs_visible: Rectangle {
                height: 140px;
                background: #0B0D11;
                border-width: 1px;
                border-color: #282E3E;

                VerticalLayout {
                    padding: 8px;
                    spacing: 4px;

                    HorizontalLayout {
                        spacing: 10px;
                        Text {
                            text: "Diagnostic Logs (Live Engine Telemetry):";
                            font-size: 12px;
                            font-weight: 700;
                            color: #A855F7;
                            vertical-alignment: center;
                        }

                        Button {
                            text: "Clear Logs";
                            clicked => { root.clear_logs(); }
                        }

                        Button {
                            text: "✖ Close";
                            clicked => { root.toggle_logs(); }
                        }
                    }

                    ScrollView {
                        VerticalLayout {
                            spacing: 2px;
                            for line[i] in root.log_lines: Text {
                                text: line;
                                font-size: 10px;
                                color: #94A3B8;
                            }
                        }
                    }
                }
            }
        }
    }
}

static CURRENT_STREAMS: Mutex<Vec<CompatibleStream>> = Mutex::new(Vec::new());
static ACTIVE_STREAM_URL: Mutex<String> = Mutex::new(String::new());
static SUBTITLE_TRACKS: Mutex<Vec<state::SubtitleTrack>> = Mutex::new(Vec::new());
static CURRENT_SUB_INDEX: Mutex<usize> = Mutex::new(0);

// Global child video HWND stored as isize for thread safety
static VIDEO_HWND: Mutex<isize> = Mutex::new(0);
static LAST_RECT: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, 0, 0));
static LAST_PLAYER_STATE: Mutex<u32> = Mutex::new(999);

unsafe extern "system" fn video_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_ERASEBKGND: u32 = 0x0014;

    if msg == WM_ERASEBKGND {
        return 1; // Evita que GDI borre el fotograma de hardware DXVA2
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn get_slint_hwnd(window: &slint::Window) -> HWND {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    if let Ok(wh) = window.window_handle().window_handle() {
        if let RawWindowHandle::Win32(h) = wh.as_raw() {
            return h.hwnd.get() as HWND;
        }
    }
    std::ptr::null_mut() // la ventana aún no existe: reintentar más tarde
}

pub fn ensure_video_hwnd(window: &slint::Window) -> HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetParent, IsWindow};

    let slint_hwnd = get_slint_hwnd(window);
    if slint_hwnd.is_null() {
        return std::ptr::null_mut(); // sin log aquí: se llama muy seguido
    }

    let mut guard = VIDEO_HWND.lock().unwrap();
    if *guard != 0 {
        let h = *guard as HWND;
        unsafe {
            if IsWindow(h) != 0 && GetParent(h) == slint_hwnd {
                return h;
            }
            if IsWindow(h) != 0 {
                DestroyWindow(h); // padre equivocado: recrear
            }
        }
        *guard = 0;
    }

    let slint_hwnd = get_slint_hwnd(window);
    if slint_hwnd.is_null() {
        log_err("ensure_video_hwnd: Unable to locate Slint parent HWND!");
        return std::ptr::null_mut();
    }

    unsafe {
        // CRÍTICO: Activar WS_CLIPCHILDREN en el HWND padre de Slint para que el
        // renderer por software de Slint NO pinte su fondo negro encima de nuestro video_hwnd!
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongW, SetWindowLongW, GWL_STYLE};
        let parent_style = GetWindowLongW(slint_hwnd, GWL_STYLE) as u32;
        if (parent_style & WS_CLIPCHILDREN) == 0 {
            SetWindowLongW(slint_hwnd, GWL_STYLE, (parent_style | WS_CLIPCHILDREN | WS_CLIPSIBLINGS) as i32);
            log_info("Applied WS_CLIPCHILDREN to Slint parent HWND so video is never overwritten.");
        }

        let class_name = to_wide("RTStreamVideoViewport");
        let hinst = GetModuleHandleW(std::ptr::null());
        let brush = CreateSolidBrush(0x00000000);

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(video_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: brush,
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        RegisterClassExW(&wc);

        // Creamos el HWND inicialmente oculto (sin WS_VISIBLE) para no tapar el catálogo al inicio
        let v_hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            to_wide("Video Viewport").as_ptr(),
            WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
            0,
            44,
            800,
            450,
            slint_hwnd,
            std::ptr::null_mut(),
            hinst,
            std::ptr::null_mut(),
        );

        if !v_hwnd.is_null() {
            log_info(&format!(
                "Child Video HWND created successfully: 0x{:X} (parent: 0x{:X})",
                v_hwnd as usize, slint_hwnd as usize
            ));
            *guard = v_hwnd as isize;
            v_hwnd
        } else {
            let err = windows_sys::Win32::Foundation::GetLastError();
            log_err(&format!("CreateWindowExW failed for Video Viewport: error {}", err));
            std::ptr::null_mut()
        }
    }
}

pub fn run_app() -> Result<(), String> {
    log_info("Starting Slint GUI window...");
    let app = MainWindow::new().map_err(|e| format!("Failed to create Slint MainWindow: {}", e))?;

    // 1. Search callback
    let app_weak = app.as_weak();
    app.on_search_submitted(move |query| {
        let query_str = query.to_string();
        if let Some(ui) = app_weak.upgrade() {
            ui.set_is_catalog_loading(true);
            ui.set_status_text(format!("Searching for '{}'...", query_str).into());
        }
        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            let results = addons::search_cinemeta(&query_str, None).unwrap_or_default();
            let count = results.len();
            let items: Vec<CatalogItem> = results
                .into_iter()
                .map(|m| CatalogItem {
                    id: m.id.into(),
                    title: m.name.into(),
                    year: m.year.unwrap_or_default().into(),
                    item_type: m.item_type.unwrap_or_else(|| "movie".to_string()).into(),
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_catalog_loading(false);
                    let model = Rc::new(VecModel::from(items));
                    ui.set_catalog_items(ModelRc::from(model));
                    ui.set_status_text(format!("Found {} titles for '{}'", count, query_str).into());
                }
            });
        });
    });

    // 2. Filter clicked callback
    let app_weak = app.as_weak();
    app.on_filter_clicked(move |filter| {
        let f_str = filter.to_string();
        if let Some(ui) = app_weak.upgrade() {
            ui.set_active_filter(f_str.clone().into());
            ui.set_is_catalog_loading(true);
            ui.set_status_text(format!("Loading {} catalog...", f_str).into());
        }
        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            let filter_opt = if f_str == "all" { None } else { Some(f_str.as_str()) };
            let results = addons::load_popular_catalog(filter_opt).unwrap_or_default();
            let count = results.len();
            let items: Vec<CatalogItem> = results
                .into_iter()
                .map(|m| CatalogItem {
                    id: m.id.into(),
                    title: m.name.into(),
                    year: m.year.unwrap_or_default().into(),
                    item_type: m.item_type.unwrap_or_else(|| "movie".to_string()).into(),
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_catalog_loading(false);
                    let model = Rc::new(VecModel::from(items));
                    ui.set_catalog_items(ModelRc::from(model));
                    ui.set_status_text(format!("Loaded {} popular titles", count).into());
                }
            });
        });
    });

    // 3. Quick Dev Setup: registers Cinemeta + Torrentio and fetches Popular Movies
    let app_weak = app.as_weak();
    app.on_quick_dev_setup(move || {
        if let Some(ui) = app_weak.upgrade() {
            ui.set_is_catalog_loading(true);
            ui.set_status_text("Setting up Cinemeta & Torrentio...".into());
        }
        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            let _ = state::setup_dev_default_addons();
            let results = addons::load_popular_catalog(Some("movie")).unwrap_or_default();
            let count = results.len();
            let items: Vec<CatalogItem> = results
                .into_iter()
                .map(|m| CatalogItem {
                    id: m.id.into(),
                    title: m.name.into(),
                    year: m.year.unwrap_or_default().into(),
                    item_type: m.item_type.unwrap_or_else(|| "movie".to_string()).into(),
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_catalog_loading(false);
                    let model = Rc::new(VecModel::from(items));
                    ui.set_catalog_items(ModelRc::from(model));
                    ui.set_status_text(format!("Quick setup complete: {} popular movies loaded", count).into());
                }
            });
        });
    });

    // 4. Card Selected: fetches detail and streams
    let app_weak = app.as_weak();
    app.on_card_selected(move |item_id| {
        let id_str = item_id.to_string();
        if let Some(ui) = app_weak.upgrade() {
            ui.set_current_view(1); // Switch to Detail view
            ui.set_detail_id(id_str.clone().into());
            ui.set_is_streams_loading(true);
            ui.set_status_text(format!("Loading details for {}...", id_str).into());
        }
        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            // First fetch detail
            let detail_res = addons::fetch_meta_detail("movie", &id_str)
                .or_else(|_| addons::fetch_meta_detail("series", &id_str));

            let (title, is_series, year, desc) = match detail_res {
                Ok(detail) => {
                    let is_ser = detail.item_type == "series";
                    (
                        detail.name,
                        is_ser,
                        detail.year.unwrap_or_default(),
                        detail.description.unwrap_or_default(),
                    )
                }
                Err(_) => (id_str.clone(), false, String::new(), String::new()),
            };

            // Fetch streams from Torrentio
            let item_type = if is_series { "series" } else { "movie" };
            let streams = addons::fetch_streams(item_type, &id_str, 1, 1).unwrap_or_default();
            *CURRENT_STREAMS.lock().unwrap() = streams.clone();

            let stream_models: Vec<StreamItem> = streams
                .into_iter()
                .enumerate()
                .map(|(i, s)| {
                    let (badge_text, color) = if s.is_mp4 {
                        ("[★ NATIVE MP4]", slint::Color::from_rgb_u8(16, 185, 129))
                    } else if s.audio_codec == "AAC" {
                        ("[★ MKV H264+AAC]", slint::Color::from_rgb_u8(6, 182, 212))
                    } else if s.audio_codec.contains("AC3") || s.audio_codec.contains("EAC3") {
                        ("[MKV H264 AC3]", slint::Color::from_rgb_u8(245, 158, 11))
                    } else {
                        ("[MKV H264]", slint::Color::from_rgb_u8(100, 116, 139))
                    };

                    StreamItem {
                        idx: i as i32,
                        display_title: s.display_title.into(),
                        badge: badge_text.into(),
                        badge_color: color,
                        quality: s.quality.into(),
                        size: s.size.into(),
                        seeders: s.seeders as i32,
                        is_mp4: s.is_mp4,
                    }
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_detail_title(title.into());
                    ui.set_detail_type((if is_series { "series" } else { "movie" }).into());
                    ui.set_detail_year(year.into());
                    ui.set_detail_desc(desc.into());
                    ui.set_detail_is_series(is_series);
                    ui.set_is_streams_loading(false);

                    let model = Rc::new(VecModel::from(stream_models));
                    ui.set_stream_items(ModelRc::from(model));
                }
            });
        });
    });

    // 5. Back to Catalog
    let app_weak = app.as_weak();
    app.on_back_to_catalog(move || {
        if let Some(ui) = app_weak.upgrade() {
            ui.set_current_view(0);
        }
    });

    // 6. Fetch Episode Streams for Series
    let app_weak = app.as_weak();
    app.on_fetch_episode_streams(move |id, s_str, e_str| {
        let id_s = id.to_string();
        let season: u32 = s_str.parse().unwrap_or(1);
        let episode: u32 = e_str.parse().unwrap_or(1);

        if let Some(ui) = app_weak.upgrade() {
            ui.set_is_streams_loading(true);
        }

        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            let streams = addons::fetch_streams("series", &id_s, season, episode).unwrap_or_default();
            *CURRENT_STREAMS.lock().unwrap() = streams.clone();

            let stream_models: Vec<StreamItem> = streams
                .into_iter()
                .enumerate()
                .map(|(i, s)| {
                    let (badge_text, color) = if s.is_mp4 {
                        ("[★ NATIVE MP4]", slint::Color::from_rgb_u8(16, 185, 129))
                    } else if s.audio_codec == "AAC" {
                        ("[★ MKV H264+AAC]", slint::Color::from_rgb_u8(6, 182, 212))
                    } else {
                        ("[MKV H264]", slint::Color::from_rgb_u8(100, 116, 139))
                    };

                    StreamItem {
                        idx: i as i32,
                        display_title: s.display_title.into(),
                        badge: badge_text.into(),
                        badge_color: color,
                        quality: s.quality.into(),
                        size: s.size.into(),
                        seeders: s.seeders as i32,
                        is_mp4: s.is_mp4,
                    }
                })
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_streams_loading(false);
                    let model = Rc::new(VecModel::from(stream_models));
                    ui.set_stream_items(ModelRc::from(model));
                }
            });
        });
    });

    // 7. PLAY STREAM CLICKED: Immediate switch to in-player loading view (Stremio style)!
    let app_weak = app.as_weak();
    app.on_play_stream_clicked(move |stream_idx| {
        let streams = CURRENT_STREAMS.lock().unwrap().clone();
        if let Some(stream) = streams.get(stream_idx as usize) {
            let stream_clone = stream.clone();
            log_player(&format!("Starting playback for: {}", stream.display_title));

            // Set loading state in engine
            engine::set_stream_loading(&stream.display_title);

            // Ensure child video HWND is created using Slint window
            let v_hwnd_target = if let Some(ui) = app_weak.upgrade() {
                ensure_video_hwnd(&ui.window())
            } else {
                *VIDEO_HWND.lock().unwrap() as HWND
            };
            if !v_hwnd_target.is_null() {
                unsafe { ShowWindow(v_hwnd_target, SW_HIDE); }
            }

            let (ui_detail_id, ui_detail_type, ui_season, ui_episode) = if let Some(ui) = app_weak.upgrade() {
                ui.set_current_view(2); // Switch directly to Player view!
                ui.set_is_player_loading(true);
                ui.set_player_title(stream.display_title.clone().into());
                ui.set_player_status_msg("Connecting to swarm & resolving metadata...".into());
                ui.set_player_peers_str("0 peers".into());
                ui.set_player_speed_str("0.0 KB/s".into());
                ui.set_player_progress_str("0.00 MB".into());

                (
                    ui.get_detail_id().to_string(),
                    ui.get_detail_type().to_string(),
                    ui.get_series_season().parse::<u32>().unwrap_or(1),
                    ui.get_series_episode().parse::<u32>().unwrap_or(1),
                )
            } else {
                (String::new(), String::new(), 1, 1)
            };

            // Spawn subtitle fetcher in background
            if !ui_detail_id.is_empty() {
                std::thread::spawn(move || {
                    let tracks = subtitles::fetch_subtitle_tracks(&ui_detail_type, &ui_detail_id, Some(ui_season), Some(ui_episode)).unwrap_or_default();
                    *SUBTITLE_TRACKS.lock().unwrap() = tracks.clone();
                    if let Some(first) = tracks.first() {
                        if let Ok(entries) = subtitles::download_and_parse_srt(&first.url) {
                            subtitles::set_active_subtitles(entries, &first.lang);
                            log_player(&format!("Auto-loaded subtitle track: {}", first.lang));
                        }
                    }
                });
            }

            let app_weak2 = app_weak.clone();
            std::thread::spawn(move || {
                match engine::start_stream_sync(
                    stream_clone.info_hash.clone(),
                    stream_clone.file_idx,
                    stream_clone.trackers.clone(),
                ) {
                    Ok(proxy_url) => {
                        *ACTIVE_STREAM_URL.lock().unwrap() = proxy_url.clone();
                        log_player(&format!("Stream ready at {}. Initializing embedded player on UI thread...", proxy_url));

                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = app_weak2.upgrade() {
                                ui.set_is_player_loading(false);
                                let v_hwnd = ensure_video_hwnd(&ui.window());
                                let slint_hwnd = get_slint_hwnd(&ui.window());

                                if !v_hwnd.is_null() && !slint_hwnd.is_null() {
                                    unsafe {
                                        let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                                        windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                                        let w = (rc.right - rc.left).max(100);
                                        let h = (rc.bottom - rc.top).max(100);
                                        let top_bar_h = 44;
                                        let bot_bar_h = 60;
                                        let viewport_h = (h - top_bar_h - bot_bar_h).max(100);

                                        *LAST_RECT.lock().unwrap() = (0, top_bar_h, w, viewport_h);

                                        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                                            v_hwnd,
                                            windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP,
                                            0,
                                            top_bar_h,
                                            w,
                                            viewport_h,
                                            windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                                                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                                        );
                                    }
                                }

                                match player::init_embedded_player(v_hwnd, &proxy_url) {
                                    Ok(()) => {}
                                    Err(e) => {
                                        ui.set_player_status_msg(format!("Player Error: {}", e).into());
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => {
                        log_err(&format!("Torrent engine stream failed: {}", e));
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = app_weak2.upgrade() {
                                ui.set_player_status_msg(format!("Error: {}", e).into());
                            }
                        });
                    }
                }
            });
        }
    });

    // 8. Play VLC External clicked
    app.on_play_vlc_clicked(move |stream_idx| {
        let streams = CURRENT_STREAMS.lock().unwrap().clone();
        if let Some(stream) = streams.get(stream_idx as usize) {
            let stream_clone = stream.clone();
            std::thread::spawn(move || {
                if let Ok(proxy_url) = engine::start_stream_sync(
                    stream_clone.info_hash,
                    stream_clone.file_idx,
                    stream_clone.trackers,
                ) {
                    let _ = player::play_stream_in_vlc(&proxy_url, std::ptr::null_mut());
                }
            });
        }
    });

    // 9. Player Back clicked
    let app_weak = app.as_weak();
    app.on_player_back(move || {
        engine::cancel_stream_loading();
        player::stop_embedded_player();
        let v_hwnd = *VIDEO_HWND.lock().unwrap() as HWND;
        if !v_hwnd.is_null() {
            unsafe { ShowWindow(v_hwnd, SW_HIDE); }
        }
        if let Some(ui) = app_weak.upgrade() {
            ui.set_current_view(0); // Return to catalog
            ui.set_is_player_loading(false);
        }
    });

    // 10. Player Play / Pause
    let app_weak = app.as_weak();
    app.on_player_play_pause(move || {
        if let Ok(is_paused) = player::toggle_pause() {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_paused(is_paused);
            }
        }
    });

    // 11. Seek Backward & Forward
    app.on_player_seek_backward(move || {
        let _ = player::seek_relative(-15);
    });

    app.on_player_seek_forward(move || {
        let _ = player::seek_relative(30);
    });

    // 12. Fullscreen Toggle
    let app_weak = app.as_weak();
    app.on_player_toggle_fullscreen(move || {
        if let Ok(is_fs) = player::toggle_fullscreen() {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_fullscreen(is_fs);
                let slint_hwnd = get_slint_hwnd(&ui.window());
                if !slint_hwnd.is_null() {
                    unsafe {
                        if is_fs {
                            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                                slint_hwnd,
                                windows_sys::Win32::UI::WindowsAndMessaging::SW_MAXIMIZE,
                            );
                        } else {
                            windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                                slint_hwnd,
                                windows_sys::Win32::UI::WindowsAndMessaging::SW_RESTORE,
                            );
                        }
                    }
                }
            }
        }
    });

    // 13. Cycle Subtitles
    let app_weak = app.as_weak();
    app.on_player_cycle_subtitles(move || {
        let tracks = SUBTITLE_TRACKS.lock().unwrap().clone();
        if tracks.is_empty() {
            log_player("No subtitle tracks loaded.");
            return;
        }
        let mut idx_guard = CURRENT_SUB_INDEX.lock().unwrap();
        *idx_guard = (*idx_guard + 1) % (tracks.len() + 1);
        let curr_idx = *idx_guard;

        if curr_idx == tracks.len() {
            subtitles::clear_active_subtitles();
            log_player("Subtitles disabled.");
            if let Some(ui) = app_weak.upgrade() {
                ui.set_subtitle_text("".into());
            }
        } else if let Some(track) = tracks.get(curr_idx) {
            let track_clone = track.clone();
            std::thread::spawn(move || {
                if let Ok(entries) = subtitles::download_and_parse_srt(&track_clone.url) {
                    subtitles::set_active_subtitles(entries, &track_clone.lang);
                    log_player(&format!("Subtitle activated: {}", track_clone.lang));
                }
            });
        }
    });

    // 14. Logs Drawer Toggle & Clear
    let app_weak = app.as_weak();
    app.on_toggle_logs(move || {
        if let Some(ui) = app_weak.upgrade() {
            let next = !ui.get_logs_visible();
            ui.set_logs_visible(next);
        }
    });

    let app_weak = app.as_weak();
    app.on_clear_logs(move || {
        let _ = logger::drain_ui_logs();
        if let Some(ui) = app_weak.upgrade() {
            let empty: Vec<SharedString> = Vec::new();
            ui.set_log_lines(ModelRc::from(Rc::new(VecModel::from(empty))));
        }
    });

    // 15. Player Open External VLC
    app.on_player_open_vlc(move || {
        let stream_url = ACTIVE_STREAM_URL.lock().unwrap().clone();
        if !stream_url.is_empty() {
            let _ = player::play_stream_in_vlc(&stream_url, std::ptr::null_mut());
        }
    });

    // 16. Periodic UI update timer (250 ms)
    let app_weak = app.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(250),
        move || {
            if let Some(ui) = app_weak.upgrade() {
                // Update stream loading telemetry
                let stream_status = engine::get_stream_status();
                if stream_status.is_loading && ui.get_current_view() == 2 {
                    ui.set_is_player_loading(true);
                    ui.set_player_status_msg(stream_status.message.into());
                    ui.set_player_peers_str(format!("{} peers", stream_status.peers).into());
                    ui.set_player_speed_str(format!("{:.1} KB/s", stream_status.speed_kbs).into());
                    ui.set_player_progress_str(format!(
                        "{:.2} MB ({}s / 60s)",
                        stream_status.progress_mb, stream_status.elapsed_secs
                    ).into());
                }

                // If player is active and playing
                if ui.get_current_view() == 2 && !ui.get_is_player_loading() {
                    let v_hwnd = ensure_video_hwnd(&ui.window());
                    let slint_hwnd = get_slint_hwnd(&ui.window());
                    if !v_hwnd.is_null() && !slint_hwnd.is_null() {
                        unsafe {
                            let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                            let w = (rc.right - rc.left).max(100);
                            let h = (rc.bottom - rc.top).max(100);

                            let (target_y, target_h) = if ui.get_is_fullscreen() {
                                (0, h)
                            } else {
                                let top_bar_h = 44;
                                let bot_bar_h = 60;
                                // Si el drawer de logs está abierto abajo (140px), descontarlo también:
                                let logs_h = if ui.get_logs_visible() { 140 } else { 0 };
                                (top_bar_h, (h - top_bar_h - bot_bar_h - logs_h).max(100))
                            };

                            let new_bounds = (0, target_y, w, target_h);
                            let mut last_b = LAST_RECT.lock().unwrap();
                            if *last_b != new_bounds {
                                *last_b = new_bounds;
                                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                                    v_hwnd,
                                    windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP,
                                    0,
                                    target_y,
                                    w,
                                    target_h,
                                    windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
                                );
                                player::update_video_position(0, target_y, w, target_h);
                            }
                        }
                    }

                    // Track & log player state changes
                    let st = player::get_player_state();
                    let mut last_st = LAST_PLAYER_STATE.lock().unwrap();
                    if *last_st != st {
                        *last_st = st;
                        let state_name = match st {
                            0 => "Empty (Resolving Source)",
                            1 => "Stopped",
                            2 => "Playing (Active Hardware Video)",
                            3 => "Paused",
                            4 => "Shutdown",
                            _ => "Unknown",
                        };
                        log_player(&format!("MFPlay State Changed -> {} (State ID: {})", state_name, st));
                    }

                    // Update time and subtitles
                    ui.set_time_str(player::get_playback_time_str().into());
                    if let Some(sub) = player::get_current_subtitle() {
                        ui.set_subtitle_text(sub.into());
                    } else {
                        ui.set_subtitle_text("".into());
                    }
                } else if ui.get_current_view() != 2 {
                    // Hide video HWND when not in player
                    let v_hwnd = *VIDEO_HWND.lock().unwrap() as HWND;
                    if !v_hwnd.is_null() {
                        unsafe { ShowWindow(v_hwnd, SW_HIDE); }
                    }
                }

                // Drain new logs for diagnostic drawer
                let new_logs = logger::drain_ui_logs();
                if !new_logs.is_empty() {
                    let mut current: Vec<SharedString> = Vec::new();
                    let model = ui.get_log_lines();
                    for idx in 0..model.row_count() {
                        if let Some(s) = model.row_data(idx) {
                            current.push(s);
                        }
                    }
                    for l in new_logs {
                        current.push(l.into());
                    }
                    if current.len() > 100 {
                        current.drain(0..current.len() - 100);
                    }
                    ui.set_log_lines(ModelRc::from(Rc::new(VecModel::from(current))));
                }
            }
        },
    );

    // Initial setup: load popular movies
    let app_weak = app.as_weak();
    std::thread::spawn(move || {
        let results = addons::load_popular_catalog(Some("movie")).unwrap_or_default();
        let count = results.len();
        let items: Vec<CatalogItem> = results
            .into_iter()
            .map(|m| CatalogItem {
                id: m.id.into(),
                title: m.name.into(),
                year: m.year.unwrap_or_default().into(),
                item_type: m.item_type.unwrap_or_else(|| "movie".to_string()).into(),
            })
            .collect();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app_weak.upgrade() {
                let model = Rc::new(VecModel::from(items));
                ui.set_catalog_items(ModelRc::from(model));
                ui.set_status_text(format!("Loaded {} popular titles", count).into());
            }
        });
    });

    log_info("Running Slint main event loop...");
    app.run().map_err(|e| format!("Slint event loop error: {}", e))
}
