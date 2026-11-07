use crate::addons::{self, CompatibleStream};
use crate::engine;
use crate::logger::{self, log_err, log_info, log_player};
use crate::player;
use crate::state;
use crate::subtitles;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, IsWindow, RegisterClassExW, CS_HREDRAW,
    CS_VREDRAW, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS,
};

#[link(name = "imm32")]
extern "system" {}

slint::include_modules!();

static CURRENT_STREAMS: Mutex<Vec<CompatibleStream>> = Mutex::new(Vec::new());
static ACTIVE_STREAM_URL: Mutex<String> = Mutex::new(String::new());
static SUBTITLE_TRACKS: Mutex<Vec<state::SubtitleTrack>> = Mutex::new(Vec::new());
static CURRENT_SUB_INDEX: Mutex<usize> = Mutex::new(0);

// Global child video HWND stored as isize for thread safety
static VIDEO_HWND: Mutex<isize> = Mutex::new(0);
static LAST_RECT: Mutex<(i32, i32, i32, i32)> = Mutex::new((0, 0, 0, 0));
static LAST_PLAYER_STATE: Mutex<u32> = Mutex::new(999);
static FULLSCREEN_HANDLER: Mutex<Option<Box<dyn Fn() + Send + 'static>>> = Mutex::new(None);
static LAST_MOUSE_POS: Mutex<(i16, i16)> = Mutex::new((-1, -1));

unsafe extern "system" fn video_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    const WM_ERASEBKGND: u32 = 0x0014;
    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;

    if msg == WM_ERASEBKGND {
        return 1; // Evita que GDI borre el fotograma de hardware DXVA2
    }
    if msg == WM_MOUSEMOVE {
        let x = (lparam & 0xFFFF) as i16;
        let y = ((lparam >> 16) & 0xFFFF) as i16;
        let mut guard = LAST_MOUSE_POS.lock().unwrap();
        if *guard != (x, y) {
            *guard = (x, y);
            drop(guard);
            crate::ui::overlay::notify_user_activity();
        }
        return 0;
    }
    if msg == WM_LBUTTONDBLCLK {
        let _ = slint::invoke_from_event_loop(|| {
            if let Some(h) = FULLSCREEN_HANDLER.lock().unwrap().as_ref() {
                h();
            }
        });
        return 0;
    }
    if msg == WM_LBUTTONUP {
        if !crate::ui::overlay::get_controls_visible() {
            crate::ui::overlay::notify_user_activity();
        } else {
            crate::ui::overlay::toggle_controls_visible();
        }
        return 0;
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Safely destroys the child video HWND and resets its tracking state
pub fn destroy_video_hwnd() {
    let mut guard = VIDEO_HWND.lock().unwrap();
    if *guard != 0 {
        let h = *guard as HWND;
        unsafe {
            if IsWindow(h) != 0 {
                DestroyWindow(h);
            }
        }
        *guard = 0;
        *LAST_RECT.lock().unwrap() = (0, 0, 0, 0);
        log_info("Video HWND cleanly destroyed and reset.");
    }
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
            style: CS_HREDRAW | CS_VREDRAW | 0x0008, // CS_DBLCLKS for double-tap/click
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
            0,
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

/// Loads the full multi-row catalog (Popular Movies, Trending Series, Featured Classics)
/// across spike_row1, spike_row2, and spike_row3, then requests posters asynchronously.
pub fn load_multi_row_home_catalog(app_weak: slint::Weak<MainWindow>) {
    crate::ui::images::bump_generation();
    if let Some(ui) = app_weak.upgrade() {
        ui.set_is_catalog_loading(true);
        ui.set_status_text("Loading home catalog from Cinemeta (Movies, Series, Classics)...".into());
    }

    let app_weak2 = app_weak.clone();
    std::thread::spawn(move || {
        let _ = state::setup_dev_default_addons();
        let sample_movies = addons::load_popular_catalog(Some("movie")).unwrap_or_default();
        let sample_series = addons::load_popular_catalog(Some("series")).unwrap_or_default();

        // Row 1: Popular Movies (20 cards)
        let mut row1_desc: Vec<(String, String, String, String)> = Vec::new();
        for i in 0..20 {
            if let Some(m) = sample_movies.get(i) {
                row1_desc.push((
                    m.id.clone(),
                    m.name.clone(),
                    m.year.clone().unwrap_or_else(|| "2024".to_string()),
                    "movie".to_string(),
                ));
            } else if let Some(m) = sample_movies.get(i % sample_movies.len().max(1)) {
                row1_desc.push((
                    m.id.clone(),
                    m.name.clone(),
                    m.year.clone().unwrap_or_else(|| "2024".to_string()),
                    "movie".to_string(),
                ));
            }
        }

        // Row 2: Trending Series (20 cards)
        let mut row2_desc: Vec<(String, String, String, String)> = Vec::new();
        for i in 0..20 {
            if let Some(s) = sample_series.get(i) {
                row2_desc.push((
                    s.id.clone(),
                    s.name.clone(),
                    s.year.clone().unwrap_or_else(|| "2023".to_string()),
                    "series".to_string(),
                ));
            } else if let Some(s) = sample_series.get(i % sample_series.len().max(1)) {
                row2_desc.push((
                    s.id.clone(),
                    s.name.clone(),
                    s.year.clone().unwrap_or_else(|| "2023".to_string()),
                    "series".to_string(),
                ));
            }
        }

        // Row 3: Featured Classics / Alternate (20 cards)
        let mut row3_desc: Vec<(String, String, String, String)> = Vec::new();
        let offset = if sample_movies.len() > 5 { 5 } else { 0 };
        for i in 0..20 {
            let idx = (i + offset) % sample_movies.len().max(1);
            if let Some(m) = sample_movies.get(idx) {
                row3_desc.push((
                    m.id.clone(),
                    m.name.clone(),
                    m.year.clone().unwrap_or_else(|| "2020".to_string()),
                    "movie".to_string(),
                ));
            }
        }

        let r1_init = row1_desc.clone();
        let r2_init = row2_desc.clone();
        let r3_init = row3_desc.clone();
        let app_weak_init = app_weak2.clone();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app_weak_init.upgrade() {
                ui.set_is_catalog_loading(false);

                let items1: Vec<CatalogItem> = r1_init
                    .into_iter()
                    .map(|(id, title, year, itype)| CatalogItem {
                        id: id.into(),
                        title: title.into(),
                        year: year.into(),
                        item_type: itype.into(),
                        poster: slint::Image::default(),
                        has_poster: false,
                    })
                    .collect();
                let items2: Vec<CatalogItem> = r2_init
                    .into_iter()
                    .map(|(id, title, year, itype)| CatalogItem {
                        id: id.into(),
                        title: title.into(),
                        year: year.into(),
                        item_type: itype.into(),
                        poster: slint::Image::default(),
                        has_poster: false,
                    })
                    .collect();
                let items3: Vec<CatalogItem> = r3_init
                    .into_iter()
                    .map(|(id, title, year, itype)| CatalogItem {
                        id: id.into(),
                        title: title.into(),
                        year: year.into(),
                        item_type: itype.into(),
                        poster: slint::Image::default(),
                        has_poster: false,
                    })
                    .collect();

                let m1 = Rc::new(VecModel::from(items1.clone()));
                ui.set_catalog_items(ModelRc::from(m1));
                ui.set_spike_row1(ModelRc::from(Rc::new(VecModel::from(items1))));
                ui.set_spike_row2(ModelRc::from(Rc::new(VecModel::from(items2))));
                ui.set_spike_row3(ModelRc::from(Rc::new(VecModel::from(items3))));
                ui.set_status_text("Home catalog ready: 60 titles loaded across 3 rows.".into());
            }
        });

        // Asynchronously request posters for Row 1
        for (idx, (id, _, _, _)) in row1_desc.iter().enumerate() {
            let app_weak_img = app_weak2.clone();
            let url = format!("https://images.metahub.space/poster/small/{}/img.jpg", id);
            crate::ui::images::request_image(url, 138, 207, move |opt_img| {
                if let Some(img) = opt_img {
                    if let Some(ui) = app_weak_img.upgrade() {
                        let model = ui.get_spike_row1();
                        if let Some(mut cur) = model.row_data(idx) {
                            cur.poster = img.clone();
                            cur.has_poster = true;
                            model.set_row_data(idx, cur);
                        }
                        let cat_model = ui.get_catalog_items();
                        if let Some(mut cur) = cat_model.row_data(idx) {
                            cur.poster = img;
                            cur.has_poster = true;
                            cat_model.set_row_data(idx, cur);
                        }
                    }
                }
            });
        }

        // Asynchronously request posters for Row 2
        for (idx, (id, _, _, _)) in row2_desc.iter().enumerate() {
            let app_weak_img = app_weak2.clone();
            let url = format!("https://images.metahub.space/poster/small/{}/img.jpg", id);
            crate::ui::images::request_image(url, 138, 207, move |opt_img| {
                if let Some(img) = opt_img {
                    if let Some(ui) = app_weak_img.upgrade() {
                        let model = ui.get_spike_row2();
                        if let Some(mut cur) = model.row_data(idx) {
                            cur.poster = img;
                            cur.has_poster = true;
                            model.set_row_data(idx, cur);
                        }
                    }
                }
            });
        }

        // Asynchronously request posters for Row 3
        for (idx, (id, _, _, _)) in row3_desc.iter().enumerate() {
            let app_weak_img = app_weak2.clone();
            let url = format!("https://images.metahub.space/poster/small/{}/img.jpg", id);
            crate::ui::images::request_image(url, 138, 207, move |opt_img| {
                if let Some(img) = opt_img {
                    if let Some(ui) = app_weak_img.upgrade() {
                        let model = ui.get_spike_row3();
                        if let Some(mut cur) = model.row_data(idx) {
                            cur.poster = img;
                            cur.has_poster = true;
                            model.set_row_data(idx, cur);
                        }
                    }
                }
            });
        }
    });
}

pub fn run_app() -> Result<(), String> {
    log_info("Starting Slint GUI window...");
    let app = MainWindow::new().map_err(|e| format!("Failed to create Slint MainWindow: {}", e))?;

    // 1. Search callback
    let app_weak = app.as_weak();
    app.on_search_submitted(move |query| {
        let query_str = query.to_string();
        crate::ui::images::bump_generation();
        if let Some(ui) = app_weak.upgrade() {
            ui.set_is_catalog_loading(true);
            ui.set_status_text(format!("Searching for '{}'...", query_str).into());
        }
        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            let results = addons::search_cinemeta(&query_str, None).unwrap_or_default();
            let count = results.len();
            let poster_urls: Vec<(usize, String)> = results
                .iter()
                .enumerate()
                .filter_map(|(i, m)| m.poster.as_ref().map(|p| (i, p.clone())))
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_catalog_loading(false);
                    let items: Vec<CatalogItem> = results
                        .into_iter()
                        .map(|m| CatalogItem {
                            id: m.id.into(),
                            title: m.name.into(),
                            year: m.year.unwrap_or_default().into(),
                            item_type: m.item_type.unwrap_or_else(|| "movie".to_string()).into(),
                            poster: slint::Image::default(),
                            has_poster: false,
                        })
                        .collect();
                    let model = Rc::new(VecModel::from(items));
                    ui.set_catalog_items(ModelRc::from(model));
                    ui.set_status_text(format!("Found {} titles for '{}'", count, query_str).into());

                    // Request posters
                    for (idx, poster_url) in poster_urls {
                        let app_weak_poster = app_weak2.clone();
                        crate::ui::images::request_image(poster_url, 138, 207, move |opt_img| {
                            if let Some(img) = opt_img {
                                if let Some(ui) = app_weak_poster.upgrade() {
                                    let model = ui.get_catalog_items();
                                    if let Some(mut cur) = model.row_data(idx) {
                                        cur.poster = img;
                                        cur.has_poster = true;
                                        model.set_row_data(idx, cur);
                                    }
                                }
                            }
                        });
                    }
                }
            });
        });
    });

    // 2. Filter clicked callback
    let app_weak = app.as_weak();
    app.on_filter_clicked(move |filter| {
        let f_str = filter.to_string();
        crate::ui::images::bump_generation();
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
            let poster_urls: Vec<(usize, String)> = results
                .iter()
                .enumerate()
                .filter_map(|(i, m)| m.poster.as_ref().map(|p| (i, p.clone())))
                .collect();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_catalog_loading(false);
                    let items: Vec<CatalogItem> = results
                        .into_iter()
                        .map(|m| CatalogItem {
                            id: m.id.into(),
                            title: m.name.into(),
                            year: m.year.unwrap_or_default().into(),
                            item_type: m.item_type.unwrap_or_else(|| "movie".to_string()).into(),
                            poster: slint::Image::default(),
                            has_poster: false,
                        })
                        .collect();
                    let model = Rc::new(VecModel::from(items));
                    ui.set_catalog_items(ModelRc::from(model));
                    ui.set_status_text(format!("Loaded {} popular titles", count).into());

                    // Request posters
                    for (idx, poster_url) in poster_urls {
                        let app_weak_poster = app_weak2.clone();
                        crate::ui::images::request_image(poster_url, 138, 207, move |opt_img| {
                            if let Some(img) = opt_img {
                                if let Some(ui) = app_weak_poster.upgrade() {
                                    let model = ui.get_catalog_items();
                                    if let Some(mut cur) = model.row_data(idx) {
                                        cur.poster = img;
                                        cur.has_poster = true;
                                        model.set_row_data(idx, cur);
                                    }
                                }
                            }
                        });
                    }
                }
            });
        });
    });

    // 3. Quick Dev Setup: registers Cinemeta + Torrentio and fetches Multi-Row Catalog
    let app_weak = app.as_weak();
    app.on_quick_dev_setup(move || {
        let app_weak2 = app_weak.clone();
        load_multi_row_home_catalog(app_weak2);
    });

    // Multi-Row Catalog loader
    let app_weak = app.as_weak();
    app.on_run_image_spike(move || {
        let app_weak2 = app_weak.clone();
        load_multi_row_home_catalog(app_weak2);
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

            // First: hide and cleanup any existing playback and destroy old video HWND
            crate::ui::overlay::hide_overlay();
            player::stop_embedded_player();
            destroy_video_hwnd();

            // Set loading state in engine
            engine::set_stream_loading(&stream.display_title);

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

                                let mut w = 800;
                                let mut h = 450;

                                if !slint_hwnd.is_null() {
                                    unsafe {
                                        let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                                        windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                                        w = (rc.right - rc.left).max(100);
                                        h = (rc.bottom - rc.top).max(100);
                                    }
                                }
                                *LAST_RECT.lock().unwrap() = (0, 0, w, h);

                                match player::init_embedded_player(v_hwnd, &proxy_url) {
                                    Ok(()) => {
                                        // Player Overlay: launch owned borderless WS_CHILD overlay with SetWindowRgn
                                        match crate::ui::overlay::ensure_overlay(slint_hwnd, 0, 0, w, h) {
                                            Ok(overlay) => {
                                                let ov_hwnd = crate::ui::overlay::get_overlay_hwnd();
                                                if !v_hwnd.is_null() && !ov_hwnd.is_null() {
                                                    unsafe {
                                                        crate::ui::overlay::position_overlay(slint_hwnd, 0, 0, w, h);
                                                        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                                                            v_hwnd,
                                                            ov_hwnd,
                                                            0,
                                                            0,
                                                            w,
                                                            h,
                                                            windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                                                                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                                                        );
                                                    }
                                                }

                                                overlay.set_title_text(stream_clone.display_title.clone().into());
                                                overlay.set_time_str("00:00 / 00:00".into());
                                                overlay.set_is_paused(false);
                                                overlay.set_is_fullscreen(ui.window().is_fullscreen());

                                                overlay.on_user_activity(move || {
                                                    crate::ui::overlay::notify_user_activity();
                                                });

                                                let app_weak_ov = app_weak2.clone();
                                                overlay.on_play_pause_clicked(move || {
                                                    crate::ui::overlay::notify_user_activity();
                                                    if let Ok(is_paused) = player::toggle_pause() {
                                                        if let Some(ui) = app_weak_ov.upgrade() {
                                                            ui.set_is_paused(is_paused);
                                                        }
                                                    }
                                                });

                                                overlay.on_seek_relative(move |delta| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let _ = player::seek_relative(delta);
                                                });

                                                overlay.on_seek_percent(move |pct| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let _ = player::seek_percent(pct);
                                                });

                                                let app_weak_close = app_weak2.clone();
                                                overlay.on_close_clicked(move || {
                                                    crate::ui::overlay::hide_overlay();
                                                    engine::cancel_stream_loading();
                                                    player::stop_embedded_player();
                                                    destroy_video_hwnd();
                                                    if let Some(ui) = app_weak_close.upgrade() {
                                                        ui.set_current_view(0);
                                                        ui.set_is_player_loading(false);
                                                    }
                                                });

                                                let app_weak_fs = app_weak2.clone();
                                                overlay.on_toggle_fullscreen_clicked(move || {
                                                    crate::ui::overlay::notify_user_activity();
                                                    if let Some(ui) = app_weak_fs.upgrade() {
                                                        let next_fs = !ui.window().is_fullscreen();
                                                        ui.window().set_fullscreen(next_fs);
                                                        ui.set_is_fullscreen(next_fs);
                                                        let slint_h = get_slint_hwnd(&ui.window());
                                                        let v_hwnd = *VIDEO_HWND.lock().unwrap() as HWND;
                                                        if !slint_h.is_null() && !v_hwnd.is_null() {
                                                            unsafe {
                                                                let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                                                                windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_h, &mut rc);
                                                                let w = (rc.right - rc.left).max(100);
                                                                let h = (rc.bottom - rc.top).max(100);
                                                                let ov_hwnd = crate::ui::overlay::get_overlay_hwnd();
                                                                crate::ui::overlay::position_overlay(slint_h, 0, 0, w, h);
                                                                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                                                                    v_hwnd,
                                                                    if !ov_hwnd.is_null() { ov_hwnd } else { windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP },
                                                                    0,
                                                                    0,
                                                                    w,
                                                                    h,
                                                                    windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                                                                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                                                                );
                                                                player::update_video_position(0, 0, w, h);
                                                            }
                                                        }
                                                    }
                                                });

                                                let app_weak_subs = app_weak2.clone();
                                                overlay.on_cycle_subtitles(move || {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let tracks = SUBTITLE_TRACKS.lock().unwrap().clone();
                                                    if tracks.is_empty() { return; }
                                                    let mut idx_guard = CURRENT_SUB_INDEX.lock().unwrap();
                                                    *idx_guard = (*idx_guard + 1) % (tracks.len() + 1);
                                                    let curr_idx = *idx_guard;

                                                    if curr_idx == tracks.len() {
                                                        subtitles::clear_active_subtitles();
                                                        if let Some(ui) = app_weak_subs.upgrade() {
                                                            ui.set_subtitle_text("".into());
                                                        }
                                                    } else if let Some(track) = tracks.get(curr_idx) {
                                                        let track_clone = track.clone();
                                                        std::thread::spawn(move || {
                                                            if let Ok(entries) = subtitles::download_and_parse_srt(&track_clone.url) {
                                                                subtitles::set_active_subtitles(entries, &track_clone.lang);
                                                            }
                                                        });
                                                    }
                                                });
                                            }
                                            Err(e) => {
                                                log_err(&format!("Overlay creation failed: {}; using emergency fallback", e));
                                                ui.set_overlay_failed(true);
                                            }
                                        }
                                    }
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
        crate::ui::overlay::hide_overlay();
        engine::cancel_stream_loading();
        player::stop_embedded_player();
        destroy_video_hwnd();
        if let Some(ui) = app_weak.upgrade() {
            ui.set_current_view(0); // Return to catalog
            ui.set_is_player_loading(false);
        }
    });

    // 10. Player Play / Pause
    let app_weak = app.as_weak();
    app.on_player_play_pause(move || {
        crate::ui::overlay::notify_user_activity();
        if let Ok(is_paused) = player::toggle_pause() {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_paused(is_paused);
            }
        }
    });

    // 11. Seek Backward & Forward
    app.on_player_seek_backward(move || {
        crate::ui::overlay::notify_user_activity();
        let _ = player::seek_relative(-15);
    });

    app.on_player_seek_forward(move || {
        crate::ui::overlay::notify_user_activity();
        let _ = player::seek_relative(30);
    });

    // 12. Fullscreen Toggle (Real Slint fullscreen)
    let app_weak_fs = app.as_weak();
    let toggle_fullscreen = Arc::new(move || {
        if let Some(ui) = app_weak_fs.upgrade() {
            crate::ui::overlay::notify_user_activity();
            let next_fs = !ui.window().is_fullscreen();
            ui.window().set_fullscreen(next_fs);
            ui.set_is_fullscreen(next_fs);
            let slint_hwnd = get_slint_hwnd(&ui.window());
            let v_hwnd = *VIDEO_HWND.lock().unwrap() as HWND;
            if !slint_hwnd.is_null() && !v_hwnd.is_null() {
                unsafe {
                    let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                    windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                    let w = (rc.right - rc.left).max(100);
                    let h = (rc.bottom - rc.top).max(100);
                    let ov_hwnd = crate::ui::overlay::get_overlay_hwnd();
                    crate::ui::overlay::position_overlay(slint_hwnd, 0, 0, w, h);
                    windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                        v_hwnd,
                        if !ov_hwnd.is_null() { ov_hwnd } else { windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP },
                        0,
                        0,
                        w,
                        h,
                        windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                            | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                    );
                    player::update_video_position(0, 0, w, h);
                }
            }
        }
    });

    let tg_clone = Arc::clone(&toggle_fullscreen);
    *FULLSCREEN_HANDLER.lock().unwrap() = Some(Box::new(move || tg_clone()));

    let tg_clone2 = Arc::clone(&toggle_fullscreen);
    app.on_player_toggle_fullscreen(move || {
        crate::ui::overlay::notify_user_activity();
        tg_clone2();
    });

    // Handle Escape Key: exits fullscreen if fullscreen, else returns to catalog
    let app_weak_esc = app.as_weak();
    app.on_handle_key_escape(move || {
        if let Some(ui) = app_weak_esc.upgrade() {
            crate::ui::overlay::notify_user_activity();
            if ui.window().is_fullscreen() {
                ui.window().set_fullscreen(false);
                ui.set_is_fullscreen(false);
                let slint_hwnd = get_slint_hwnd(&ui.window());
                let v_hwnd = *VIDEO_HWND.lock().unwrap() as HWND;
                if !slint_hwnd.is_null() && !v_hwnd.is_null() {
                    unsafe {
                        let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                        windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                        let w = (rc.right - rc.left).max(100);
                        let h = (rc.bottom - rc.top).max(100);
                        let ov_hwnd = crate::ui::overlay::get_overlay_hwnd();
                        crate::ui::overlay::position_overlay(slint_hwnd, 0, 0, w, h);
                        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                            v_hwnd,
                            if !ov_hwnd.is_null() { ov_hwnd } else { windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP },
                            0,
                            0,
                            w,
                            h,
                            windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                                | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                        );
                        player::update_video_position(0, 0, w, h);
                    }
                }
            } else if ui.get_current_view() == 2 {
                crate::ui::overlay::hide_overlay();
                engine::cancel_stream_loading();
                player::stop_embedded_player();
                destroy_video_hwnd();
                ui.set_current_view(0);
                ui.set_is_player_loading(false);
            }
        }
    });

    // 13. Cycle Subtitles
    let app_weak = app.as_weak();
    app.on_player_cycle_subtitles(move || {
        crate::ui::overlay::notify_user_activity();
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

                let slint_hwnd = get_slint_hwnd(&ui.window());

                // If player is active and playing
                if ui.get_current_view() == 2 && !ui.get_is_player_loading() {
                    let v_hwnd = ensure_video_hwnd(&ui.window());
                    if !v_hwnd.is_null() && !slint_hwnd.is_null() {
                        unsafe {
                            // Check if minimized
                            if windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(slint_hwnd) != 0 {
                                crate::ui::overlay::hide_overlay();
                            } else {
                                crate::ui::overlay::show_overlay();
                            }

                            let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                            let w = (rc.right - rc.left).max(100);
                            let h = (rc.bottom - rc.top).max(100);

                            let new_bounds = (0, 0, w, h);
                            let mut last_b = LAST_RECT.lock().unwrap();
                            if *last_b != new_bounds {
                                *last_b = new_bounds;
                                let ov_hwnd = crate::ui::overlay::get_overlay_hwnd();
                                crate::ui::overlay::position_overlay(slint_hwnd, 0, 0, w, h);
                                windows_sys::Win32::UI::WindowsAndMessaging::SetWindowPos(
                                    v_hwnd,
                                    if !ov_hwnd.is_null() { ov_hwnd } else { windows_sys::Win32::UI::WindowsAndMessaging::HWND_TOP },
                                    0,
                                    0,
                                    w,
                                    h,
                                    windows_sys::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW
                                        | windows_sys::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE,
                                );
                                player::update_video_position(0, 0, w, h);
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

                    // Update time and subtitles on both MainWindow and PlayerOverlay
                    let time_str = player::get_playback_time_str();
                    let (_, _, progress) = player::get_playback_progress();
                    let is_paused = player::get_player_state() == 3;
                    let sub_text = player::get_current_subtitle().unwrap_or_default();
                    let is_fs = ui.window().is_fullscreen();
                    ui.set_time_str(time_str.clone().into());
                    ui.set_subtitle_text(sub_text.clone().into());
                    ui.set_is_fullscreen(is_fs);
                    crate::ui::overlay::update_overlay_state(&time_str, progress, is_paused, &sub_text, is_fs);

                    // Check auto-hide inactivity
                    crate::ui::overlay::check_auto_hide(is_paused);
                } else if ui.get_current_view() != 2 {
                    // Hide overlay and destroy video HWND when not in player
                    crate::ui::overlay::hide_overlay();
                    destroy_video_hwnd();
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

    // Initial setup: load multi-row home catalog (Movies, Series, Classics)
    let app_weak_init = app.as_weak();
    load_multi_row_home_catalog(app_weak_init);

    log_info("Running Slint main event loop...");
    app.run().map_err(|e| format!("Slint event loop error: {}", e))
}
