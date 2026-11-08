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
static PAGE_STACK: Mutex<Vec<i32>> = Mutex::new(Vec::new());

// In-memory catalog metadata & episode cache
static CATALOG_CACHE: Mutex<Option<std::collections::HashMap<String, addons::MetaItem>>> = Mutex::new(None);
static HERO_ITEMS: Mutex<Vec<addons::MetaItem>> = Mutex::new(Vec::new());
static CURRENT_HERO_IDX: Mutex<usize> = Mutex::new(0);
static CURRENT_SERIES_VIDEOS: Mutex<Vec<state::MetaVideo>> = Mutex::new(Vec::new());

fn cache_insert_all(items: impl IntoIterator<Item = addons::MetaItem>) {
    let mut guard = CATALOG_CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    for item in items {
        map.insert(item.id.clone(), item);
    }
}

fn cache_get(id: &str) -> Option<addons::MetaItem> {
    let guard = CATALOG_CACHE.lock().unwrap();
    guard.as_ref().and_then(|m| m.get(id).cloned())
}

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
        return 1; // Prevent GDI from erasing DXVA2 hardware frame
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
    std::ptr::null_mut()
}

pub fn ensure_video_hwnd(window: &slint::Window) -> HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetParent, IsWindow};

    let slint_hwnd = get_slint_hwnd(window);
    if slint_hwnd.is_null() {
        return std::ptr::null_mut();
    }

    let mut guard = VIDEO_HWND.lock().unwrap();
    if *guard != 0 {
        let h = *guard as HWND;
        unsafe {
            if IsWindow(h) != 0 && GetParent(h) == slint_hwnd {
                return h;
            }
            if IsWindow(h) != 0 {
                DestroyWindow(h);
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

/// Populates the Library / Continue Watching model from persistent watch history
pub fn refresh_library(ui: &MainWindow) {
    let history = state::load_watch_history();
    let lib_items: Vec<CatalogItem> = history
        .items
        .into_iter()
        .map(|item| CatalogItem {
            id: item.meta_id.into(),
            title: item.title.into(),
            year: if let (Some(s), Some(e)) = (item.season, item.episode) {
                format!("S{}:E{}", s, e).into()
            } else {
                "".into()
            },
            item_type: item.item_type.into(),
            poster: slint::Image::default(),
            has_poster: false,
        })
        .collect();
    ui.set_library_items(ModelRc::from(Rc::new(VecModel::from(lib_items))));
}

pub fn refresh_settings(ui: &MainWindow) {
    let settings = state::load_settings();
    let cfg = state::load_addons_config();

    ui.set_settings_max_res(settings.playback.max_height as i32);
    ui.set_settings_prefer_mp4(settings.playback.prefer_mp4);
    ui.set_settings_sub_lang(settings.playback.sub_lang.into());
    ui.set_settings_audio_lang(settings.playback.audio_lang.into());
    ui.set_settings_autoplay_next(settings.playback.autoplay_next);
    ui.set_settings_seek_step(settings.playback.seek_step_s as i32);

    let max_gb = (settings.cache.max_bytes / (1024 * 1024 * 1024)).max(1) as i32;
    ui.set_settings_max_cache_gb(max_gb);
    ui.set_settings_keep_playback(settings.cache.keep_after_playback);

    let torrent_used = state::get_torrent_cache_size();
    let torrent_used_mb = torrent_used / (1024 * 1024);
    let max_mb = settings.cache.max_bytes / (1024 * 1024);
    ui.set_settings_torrent_cache_str(format!("{} MB / {} MB", torrent_used_mb, max_mb).into());

    let img_used = state::get_image_cache_size();
    let img_used_mb = img_used / (1024 * 1024);
    let img_max_mb = settings.cache.image_cache_bytes / (1024 * 1024);
    ui.set_settings_image_cache_str(format!("{} MB / {} MB", img_used_mb, img_max_mb).into());

    ui.set_settings_reduce_anim(settings.ui.reduce_animations);
    ui.set_settings_hero_rotate(settings.ui.hero_autorotate);
    ui.set_settings_card_size(settings.ui.card_size.into());
    ui.set_settings_storage_path(state::get_app_storage_root().display().to_string().into());

    let addon_models: Vec<AddonItem> = cfg
        .addons
        .into_iter()
        .map(|a| AddonItem {
            id: a.manifest.id.into(),
            name: a.manifest.name.into(),
            version: a.manifest.version.into(),
            description: a.manifest.description.unwrap_or_default().into(),
            enabled: a.enabled,
        })
        .collect();
    ui.set_settings_addon_items(ModelRc::from(Rc::new(VecModel::from(addon_models))));
}

fn convert_streams_to_models(streams: &[addons::CompatibleStream]) -> Vec<StreamItem> {
    streams
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (badge_text, color) = if s.is_mp4 {
                ("[★ MP4 NATIVE]", slint::Color::from_rgb_u8(16, 185, 129))
            } else if s.audio_codec == "AAC" {
                ("[★ MKV AAC]", slint::Color::from_rgb_u8(6, 182, 212))
            } else if s.audio_codec.contains("AC3") || s.audio_codec.contains("EAC3") {
                ("[MKV AC3]", slint::Color::from_rgb_u8(245, 158, 11))
            } else if s.audio_codec.contains("DTS") {
                ("[MKV DTS]", slint::Color::from_rgb_u8(239, 68, 68))
            } else {
                ("[MKV H264]", slint::Color::from_rgb_u8(100, 116, 139))
            };

            StreamItem {
                idx: i as i32,
                display_title: s.display_title.clone().into(),
                badge: badge_text.into(),
                badge_color: color,
                quality: s.quality.clone().into(),
                size: s.size.clone().into(),
                audio_codec: s.audio_codec.clone().into(),
                seeders: s.seeders as i32,
                is_mp4: s.is_mp4,
            }
        })
        .collect()
}

fn meta_to_catalog_item(item: &addons::MetaItem) -> CatalogItem {
    let itype = item.item_type.clone().unwrap_or_else(|| "movie".to_string());
    CatalogItem {
        id: item.id.clone().into(),
        title: item.name.clone().into(),
        year: item.year.clone().unwrap_or_default().into(),
        item_type: itype.into(),
        poster: slint::Image::default(),
        has_poster: false,
    }
}

/// Loads the full 6 distinct rows on Home (Popular Movies, Trending Series, Action, Sci-Fi, Comedy, Drama)
/// and populates the Hero Banner with rich backdrop and top 5 trending carousel.
pub fn load_multi_row_home_catalog(app_weak: slint::Weak<MainWindow>) {
    crate::ui::images::bump_generation();
    if let Some(ui) = app_weak.upgrade() {
        ui.set_is_catalog_loading(true);
        ui.set_status_text("Loading home catalogs from Cinemeta...".into());
    }

    let app_weak2 = app_weak.clone();
    std::thread::spawn(move || {
        let _ = state::setup_dev_default_addons();

        // Fetch 6 distinct real catalogs from Cinemeta
        let r1 = addons::fetch_cinemeta_catalog("movie", "top", None).unwrap_or_default();
        let r2 = addons::fetch_cinemeta_catalog("series", "top", None).unwrap_or_default();
        let r3 = addons::fetch_cinemeta_catalog("movie", "top", Some("genre=Action")).unwrap_or_default();
        let r4 = addons::fetch_cinemeta_catalog("movie", "top", Some("genre=Sci-Fi")).unwrap_or_default();
        let r5 = addons::fetch_cinemeta_catalog("movie", "top", Some("genre=Comedy")).unwrap_or_default();
        let r6 = addons::fetch_cinemeta_catalog("series", "top", Some("genre=Drama")).unwrap_or_default();

        // Cache all items in memory
        cache_insert_all(
            r1.iter()
                .chain(r2.iter())
                .chain(r3.iter())
                .chain(r4.iter())
                .chain(r5.iter())
                .chain(r6.iter())
                .cloned(),
        );

        // Store top 5 trending for hero banner
        let hero_candidates: Vec<addons::MetaItem> = r1.iter().take(5).cloned().collect();
        *HERO_ITEMS.lock().unwrap() = hero_candidates.clone();
        *CURRENT_HERO_IDX.lock().unwrap() = 0;

        let app_weak_init = app_weak2.clone();
        let first_hero = hero_candidates.first().cloned();

        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = app_weak_init.upgrade() {
                ui.set_is_app_initializing(false);
                ui.set_is_catalog_loading(false);

                let items1: Vec<CatalogItem> = r1.iter().map(meta_to_catalog_item).collect();
                let items2: Vec<CatalogItem> = r2.iter().map(meta_to_catalog_item).collect();
                let items3: Vec<CatalogItem> = r3.iter().map(meta_to_catalog_item).collect();
                let items4: Vec<CatalogItem> = r4.iter().map(meta_to_catalog_item).collect();
                let items5: Vec<CatalogItem> = r5.iter().map(meta_to_catalog_item).collect();
                let items6: Vec<CatalogItem> = r6.iter().map(meta_to_catalog_item).collect();

                ui.set_home_row1(ModelRc::from(Rc::new(VecModel::from(items1))));
                ui.set_home_row2(ModelRc::from(Rc::new(VecModel::from(items2))));
                ui.set_home_row3(ModelRc::from(Rc::new(VecModel::from(items3))));
                ui.set_home_row4(ModelRc::from(Rc::new(VecModel::from(items4))));
                ui.set_home_row5(ModelRc::from(Rc::new(VecModel::from(items5))));
                ui.set_home_row6(ModelRc::from(Rc::new(VecModel::from(items6))));

                // Setup Initial Hero Banner
                if let Some(hero) = first_hero {
                    ui.set_hero_title(hero.name.clone().into());
                    let year = hero.year.clone().unwrap_or_else(|| "2024".to_string());
                    let rating = hero.imdb_rating.clone().unwrap_or_else(|| "8.2".to_string());
                    let itype = hero.item_type.clone().unwrap_or_else(|| "movie".to_string());
                    ui.set_hero_meta(format!("{} • ★ {} • {}", year, rating, itype.to_uppercase()).into());
                    ui.set_hero_desc(hero.description.clone().unwrap_or_else(|| "Stream now with hardware acceleration.".to_string()).into());
                    ui.set_hero_id(hero.id.clone().into());
                    ui.set_hero_type(itype.into());
                    ui.set_hero_index(0);
                    ui.set_hero_total(5);

                    // Fetch backdrop for hero
                    let bg_url = hero.background.clone().or_else(|| hero.poster.clone());
                    if let Some(url) = bg_url {
                        let app_weak_bg = app_weak_init.clone();
                        crate::ui::images::request_image(url, 640, 360, move |opt_img| {
                            if let Some(img) = opt_img {
                                if let Some(u) = app_weak_bg.upgrade() {
                                    u.set_hero_backdrop(img);
                                    u.set_has_hero_backdrop(true);
                                }
                            }
                        });
                    }
                }

                ui.set_status_text("Home catalog ready: 6 categories loaded.".into());

                // Trigger progressive loading of posters for all 6 rows:
                // Phase 1: Immediately visible posters (0..8) across all rows load first with highest priority
                for r in 0..6 {
                    ui.invoke_request_lazy_range(r, 0, 8);
                }
                // Phase 2: Remaining offscreen / horizontally scrollable posters (8..100) are queued right after
                // so the background worker threads seamlessly download and cache them before the user scrolls
                for r in 0..6 {
                    ui.invoke_request_lazy_range(r, 8, 100);
                }
            }
        });
    });
}

fn rotate_hero(ui: &MainWindow, delta: i32) {
    let guard = HERO_ITEMS.lock().unwrap();
    if guard.is_empty() { return; }
    let mut idx_guard = CURRENT_HERO_IDX.lock().unwrap();
    let len = guard.len();
    if delta > 0 {
        *idx_guard = (*idx_guard + 1) % len;
    } else {
        *idx_guard = (*idx_guard + len - 1) % len;
    }
    let curr = *idx_guard;
    if let Some(item) = guard.get(curr) {
        ui.set_hero_title(item.name.clone().into());
        let year = item.year.clone().unwrap_or_else(|| "2024".to_string());
        let rating = item.imdb_rating.clone().unwrap_or_else(|| "8.2".to_string());
        let itype = item.item_type.clone().unwrap_or_else(|| "movie".to_string());
        ui.set_hero_meta(format!("{} • ★ {} • {}", year, rating, itype.to_uppercase()).into());
        ui.set_hero_desc(item.description.clone().unwrap_or_else(|| "Stream now with hardware acceleration.".to_string()).into());
        ui.set_hero_id(item.id.clone().into());
        ui.set_hero_type(itype.into());
        ui.set_hero_index(curr as i32);
        ui.set_hero_total(len as i32);
        ui.set_has_hero_backdrop(false);

        let bg_url = item.background.clone().or_else(|| item.poster.clone());
        if let Some(url) = bg_url {
            let app_weak_hero = ui.as_weak();
            crate::ui::images::request_image(url, 640, 360, move |opt_img| {
                if let Some(img) = opt_img {
                    if let Some(u) = app_weak_hero.upgrade() {
                        u.set_hero_backdrop(img);
                        u.set_has_hero_backdrop(true);
                    }
                }
            });
        }
    }
}

pub fn run_app() -> Result<(), String> {
    log_info("Starting Slint GUI window...");
    let app = MainWindow::new().map_err(|e| format!("Failed to create Slint MainWindow: {}", e))?;

    // Load initial Library / Watch History and Settings
    refresh_library(&app);
    refresh_settings(&app);

    // 1. Search callback (Dedicated SearchView)
    let app_weak = app.as_weak();
    app.on_search_submitted(move |query| {
        let query_str = query.to_string();
        crate::ui::images::bump_generation();
        let app_weak2 = app_weak.clone();

        if let Some(ui) = app_weak.upgrade() {
            ui.set_is_searching(true);
            ui.set_status_text(format!("Searching for '{}'...", query_str).into());
        }

        std::thread::spawn(move || {
            let results = addons::search_cinemeta(&query_str, None).unwrap_or_default();
            let mut movies: Vec<addons::MetaItem> = Vec::new();
            let mut series: Vec<addons::MetaItem> = Vec::new();

            cache_insert_all(results.iter().cloned());

            for item in results {
                let is_ser = item.item_type.as_deref() == Some("series");
                if is_ser {
                    series.push(item);
                } else {
                    movies.push(item);
                }
            }

            let m_count = movies.len();
            let s_count = series.len();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_searching(false);
                    let movie_items: Vec<CatalogItem> = movies.iter().map(meta_to_catalog_item).collect();
                    let series_items: Vec<CatalogItem> = series.iter().map(meta_to_catalog_item).collect();
                    ui.set_search_movie_results(ModelRc::from(Rc::new(VecModel::from(movie_items))));
                    ui.set_search_series_results(ModelRc::from(Rc::new(VecModel::from(series_items))));
                    ui.set_status_text(format!("Found {} movies and {} series for '{}'", m_count, s_count, query_str).into());

                    // Request posters for all search results (visible 0..8 first, then rest)
                    ui.invoke_request_lazy_range(10, 0, 8);
                    ui.invoke_request_lazy_range(11, 0, 8);
                    ui.invoke_request_lazy_range(10, 8, 100);
                    ui.invoke_request_lazy_range(11, 8, 100);
                }
            });
        });
    });

    // 2. Filter clicked callback
    let app_weak = app.as_weak();
    app.on_filter_clicked(move |filter| {
        let f_str = filter.to_string();
        if let Some(ui) = app_weak.upgrade() {
            ui.set_active_filter(f_str.into());
        }
    });

    // 3. Quick Dev Setup: registers Cinemeta + Torrentio and fetches Multi-Row Catalog
    let app_weak = app.as_weak();
    app.on_quick_dev_setup(move || {
        let app_weak2 = app_weak.clone();
        let _ = state::setup_dev_default_addons();
        if let Some(ui) = app_weak.upgrade() {
            refresh_library(&ui);
            refresh_settings(&ui);
        }
        load_multi_row_home_catalog(app_weak2);
    });

    // Clear Watch History
    let app_weak_wh = app.as_weak();
    app.on_clear_watch_history(move || {
        state::clear_watch_history();
        if let Some(ui) = app_weak_wh.upgrade() {
            refresh_library(&ui);
            ui.set_status_text("Watch history cleared.".into());
        }
    });

    // --- Settings View Callbacks ---
    let app_weak_tab = app.as_weak();
    app.on_settings_tab_changed(move |tab| {
        if let Some(ui) = app_weak_tab.upgrade() {
            ui.set_settings_active_tab(tab);
            refresh_settings(&ui);
        }
    });

    let app_weak_inst = app.as_weak();
    app.on_settings_install_addon(move |url| {
        let u_str = url.to_string();
        let app_weak2 = app_weak_inst.clone();
        std::thread::spawn(move || {
            match addons::install_addon_from_url(&u_str) {
                Ok(desc) => {
                    log_info(&format!("Add-on '{}' installed successfully.", desc.manifest.name));
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = app_weak2.upgrade() {
                            refresh_settings(&ui);
                            ui.set_status_text(format!("Add-on '{}' installed.", desc.manifest.name).into());
                        }
                    });
                }
                Err(err) => {
                    log_err(&format!("Failed installing add-on: {}", err));
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = app_weak2.upgrade() {
                            ui.set_status_text(format!("Error installing add-on: {}", err).into());
                        }
                    });
                }
            }
        });
    });

    let app_weak_tgl = app.as_weak();
    app.on_settings_toggle_addon(move |id| {
        let _ = addons::toggle_addon(&id);
        if let Some(ui) = app_weak_tgl.upgrade() {
            refresh_settings(&ui);
        }
    });

    let app_weak_rm = app.as_weak();
    app.on_settings_remove_addon(move |id| {
        let _ = addons::remove_addon(&id);
        if let Some(ui) = app_weak_rm.upgrade() {
            refresh_settings(&ui);
        }
    });

    let app_weak_prio = app.as_weak();
    app.on_settings_priority_addon(move |id, up| {
        let _ = addons::move_addon_priority(&id, up);
        if let Some(ui) = app_weak_prio.upgrade() {
            refresh_settings(&ui);
        }
    });

    let app_weak_res = app.as_weak();
    app.on_settings_change_max_res(move |res| {
        let mut s = state::load_settings();
        s.playback.max_height = res as u32;
        state::save_settings(&s);
        if let Some(ui) = app_weak_res.upgrade() {
            ui.set_settings_max_res(res);
        }
    });

    let app_weak_mp4 = app.as_weak();
    app.on_settings_change_prefer_mp4(move |prefer| {
        let mut s = state::load_settings();
        s.playback.prefer_mp4 = prefer;
        state::save_settings(&s);
        if let Some(ui) = app_weak_mp4.upgrade() {
            ui.set_settings_prefer_mp4(prefer);
        }
    });

    let app_weak_sub = app.as_weak();
    app.on_settings_change_sub_lang(move |lang| {
        let mut s = state::load_settings();
        s.playback.sub_lang = lang.to_string();
        state::save_settings(&s);
        if let Some(ui) = app_weak_sub.upgrade() {
            ui.set_settings_sub_lang(lang);
        }
    });

    let app_weak_aud = app.as_weak();
    app.on_settings_change_audio_lang(move |lang| {
        let mut s = state::load_settings();
        s.playback.audio_lang = lang.to_string();
        state::save_settings(&s);
        if let Some(ui) = app_weak_aud.upgrade() {
            ui.set_settings_audio_lang(lang);
        }
    });

    let app_weak_ap = app.as_weak();
    app.on_settings_change_autoplay_next(move |ap| {
        let mut s = state::load_settings();
        s.playback.autoplay_next = ap;
        state::save_settings(&s);
        if let Some(ui) = app_weak_ap.upgrade() {
            ui.set_settings_autoplay_next(ap);
        }
    });

    let app_weak_ss = app.as_weak();
    app.on_settings_change_seek_step(move |step| {
        let mut s = state::load_settings();
        s.playback.seek_step_s = step as u32;
        state::save_settings(&s);
        if let Some(ui) = app_weak_ss.upgrade() {
            ui.set_settings_seek_step(step);
        }
    });

    let app_weak_mc = app.as_weak();
    app.on_settings_change_max_cache(move |gb| {
        let mut s = state::load_settings();
        s.cache.max_bytes = (gb as u64) * 1024 * 1024 * 1024;
        state::save_settings(&s);
        if let Some(ui) = app_weak_mc.upgrade() {
            refresh_settings(&ui);
        }
    });

    let app_weak_kp = app.as_weak();
    app.on_settings_change_keep_playback(move |keep| {
        let mut s = state::load_settings();
        s.cache.keep_after_playback = keep;
        state::save_settings(&s);
        if let Some(ui) = app_weak_kp.upgrade() {
            ui.set_settings_keep_playback(keep);
        }
    });

    let app_weak_ct = app.as_weak();
    app.on_settings_clear_torrent_cache(move || {
        let _ = state::clear_torrent_cache();
        if let Some(ui) = app_weak_ct.upgrade() {
            refresh_settings(&ui);
            ui.set_status_text("Torrent disk cache cleared.".into());
        }
    });

    let app_weak_ci = app.as_weak();
    app.on_settings_clear_image_cache(move || {
        let _ = state::clear_image_cache();
        if let Some(ui) = app_weak_ci.upgrade() {
            refresh_settings(&ui);
            ui.set_status_text("Image poster cache cleared.".into());
        }
    });

    let app_weak_ra = app.as_weak();
    app.on_settings_change_reduce_anim(move |red| {
        let mut s = state::load_settings();
        s.ui.reduce_animations = red;
        state::save_settings(&s);
        if let Some(ui) = app_weak_ra.upgrade() {
            ui.set_settings_reduce_anim(red);
        }
    });

    let app_weak_hr = app.as_weak();
    app.on_settings_change_hero_rotate(move |rot| {
        let mut s = state::load_settings();
        s.ui.hero_autorotate = rot;
        state::save_settings(&s);
        if let Some(ui) = app_weak_hr.upgrade() {
            ui.set_settings_hero_rotate(rot);
        }
    });

    let app_weak_cs = app.as_weak();
    app.on_settings_change_card_size(move |sz| {
        let mut s = state::load_settings();
        s.ui.card_size = sz.to_string();
        state::save_settings(&s);
        if let Some(ui) = app_weak_cs.upgrade() {
            ui.set_settings_card_size(sz);
        }
    });

    // 4. Lazy Image Range Request Callback
    let app_weak_lazy = app.as_weak();
    app.on_request_lazy_range(move |row_idx, start, end| {
        if let Some(ui) = app_weak_lazy.upgrade() {
            let model = match row_idx {
                0 => ui.get_home_row1(),
                1 => ui.get_home_row2(),
                2 => ui.get_home_row3(),
                3 => ui.get_home_row4(),
                4 => ui.get_home_row5(),
                5 => ui.get_home_row6(),
                10 => ui.get_search_movie_results(),
                11 => ui.get_search_series_results(),
                20 => ui.get_library_items(),
                _ => return,
            };

            let total = model.row_count();
            let s = (start as usize).min(total);
            let e = (end as usize).min(total);

            for idx in s..e {
                if let Some(item) = model.row_data(idx) {
                    if !item.has_poster {
                        let id_str = item.id.to_string();
                        let app_weak_img = app_weak_lazy.clone();
                        let poster_url = cache_get(&id_str)
                            .and_then(|m| m.poster)
                            .unwrap_or_else(|| format!("https://images.metahub.space/poster/small/{}/img.jpg", id_str));

                        crate::ui::images::request_image(poster_url, 138, 207, move |opt_img| {
                            if let Some(img) = opt_img {
                                if let Some(u) = app_weak_img.upgrade() {
                                    let m = match row_idx {
                                        0 => u.get_home_row1(),
                                        1 => u.get_home_row2(),
                                        2 => u.get_home_row3(),
                                        3 => u.get_home_row4(),
                                        4 => u.get_home_row5(),
                                        5 => u.get_home_row6(),
                                        10 => u.get_search_movie_results(),
                                        11 => u.get_search_series_results(),
                                        20 => u.get_library_items(),
                                        _ => return,
                                    };
                                    if let Some(mut cur) = m.row_data(idx) {
                                        cur.poster = img;
                                        cur.has_poster = true;
                                        m.set_row_data(idx, cur);
                                    }
                                }
                            }
                        });
                    }
                }
            }
        }
    });

    // 5. Hero Carousel Prev / Next Callbacks
    let app_weak_hp = app.as_weak();
    app.on_hero_prev(move || {
        if let Some(ui) = app_weak_hp.upgrade() {
            rotate_hero(&ui, -1);
        }
    });

    let app_weak_hn = app.as_weak();
    app.on_hero_next(move || {
        if let Some(ui) = app_weak_hn.upgrade() {
            rotate_hero(&ui, 1);
        }
    });

    // 6. Card Selected: fetches detail and streams with type preservation & pre-fill cache
    let app_weak = app.as_weak();
    app.on_card_selected(move |item_id, item_type| {
        let id_str = item_id.to_string();
        let type_str = if item_type.is_empty() { "movie".to_string() } else { item_type.to_string() };

        if let Some(ui) = app_weak.upgrade() {
            let prev = ui.get_current_view();
            PAGE_STACK.lock().unwrap().push(prev);
            ui.set_current_view(1); // Switch to Detail view
            ui.set_detail_id(id_str.clone().into());
            ui.set_detail_type(type_str.clone().into());
            ui.set_detail_is_series(type_str == "series");
            ui.set_selected_season(1);
            ui.set_selected_episode(1);
            ui.set_has_detail_backdrop(false);
            ui.set_detail_poster(slint::Image::default());
            ui.set_is_streams_loading(true);
            ui.set_stream_items(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_seasons_list(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_episode_items(ModelRc::from(Rc::new(VecModel::default())));

            // Immediate pre-fill from cache if present!
            if let Some(cached) = cache_get(&id_str) {
                ui.set_detail_title(cached.name.clone().into());
                ui.set_detail_year(cached.year.clone().unwrap_or_default().into());
                ui.set_detail_desc(cached.description.clone().unwrap_or_default().into());
                ui.set_detail_imdb_rating(cached.imdb_rating.clone().unwrap_or_default().into());
                ui.set_detail_runtime(cached.runtime.clone().unwrap_or_default().into());
                ui.set_detail_genres(cached.genres.join(", ").into());
            } else {
                ui.set_detail_title(id_str.clone().into());
            }
            ui.set_status_text(format!("Loading details for {}...", id_str).into());
        }

        // Request poster for detail view
        let app_weak_poster = app_weak.clone();
        let poster_url = format!("https://images.metahub.space/poster/small/{}/img.jpg", id_str);
        crate::ui::images::request_image(poster_url, 138, 207, move |opt_img| {
            if let Some(img) = opt_img {
                if let Some(ui) = app_weak_poster.upgrade() {
                    ui.set_detail_poster(img);
                }
            }
        });

        let app_weak_meta = app_weak.clone();
        let id_str_meta = id_str.clone();
        let type_str_meta = type_str.clone();

        std::thread::spawn(move || {
            // Directly query correct type (movie or series) avoiding the Cinemeta fallback bug
            let detail_res = addons::fetch_meta_detail(&type_str_meta, &id_str_meta);
            let (detail, streams) = match detail_res {
                Ok(d) => {
                    // Fetch backdrop image if present
                    if let Some(bg_url) = d.background.as_ref() {
                        let app_weak_bg = app_weak_meta.clone();
                        crate::ui::images::request_image(bg_url.clone(), 640, 360, move |opt_img| {
                            if let Some(img) = opt_img {
                                if let Some(ui) = app_weak_bg.upgrade() {
                                    ui.set_detail_backdrop(img);
                                    ui.set_has_detail_backdrop(true);
                                }
                            }
                        });
                    }

                    let is_ser = d.item_type == "series";
                    let s_streams = if is_ser {
                        *CURRENT_SERIES_VIDEOS.lock().unwrap() = d.videos.clone();
                        addons::fetch_streams("series", &id_str_meta, 1, 1).unwrap_or_default()
                    } else {
                        addons::fetch_streams("movie", &id_str_meta, 1, 1).unwrap_or_default()
                    };

                    (Some(d), s_streams)
                }
                Err(e) => {
                    log_err(&format!("Error fetching meta detail: {}", e));
                    let s_streams = addons::fetch_streams(&type_str_meta, &id_str_meta, 1, 1).unwrap_or_default();
                    (None, s_streams)
                }
            };

            *CURRENT_STREAMS.lock().unwrap() = streams.clone();
            let stream_models = convert_streams_to_models(&streams);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak_meta.upgrade() {
                    ui.set_is_streams_loading(false);
                    ui.set_stream_items(ModelRc::from(Rc::new(VecModel::from(stream_models))));

                    if let Some(d) = detail {
                        ui.set_detail_title(d.name.into());
                        ui.set_detail_type(d.item_type.clone().into());
                        ui.set_detail_year(d.year.unwrap_or_default().into());
                        ui.set_detail_desc(d.description.unwrap_or_default().into());
                        ui.set_detail_is_series(d.item_type == "series");
                        ui.set_detail_imdb_rating(d.imdb_rating.unwrap_or_default().into());
                        ui.set_detail_runtime(d.runtime.unwrap_or_default().into());
                        ui.set_detail_genres(d.genres.join(", ").into());
                        ui.set_detail_cast(d.cast.join(", ").into());
                        ui.set_detail_director(d.director.join(", ").into());

                        if d.item_type == "series" {
                            let mut seasons: Vec<i32> = d.videos.iter().map(|v| v.season as i32).filter(|&s| s > 0).collect();
                            seasons.sort();
                            seasons.dedup();
                            if seasons.is_empty() {
                                seasons = vec![1];
                            }
                            let first_season = seasons[0];
                            ui.set_seasons_list(ModelRc::from(Rc::new(VecModel::from(seasons))));
                            ui.set_selected_season(first_season);

                            let first_ep = d.videos.iter().find(|v| v.season as i32 == first_season);
                            if let Some(ep) = first_ep {
                                ui.set_selected_episode(ep.episode as i32);
                                ui.set_selected_episode_title(ep.title.clone().into());
                                ui.set_selected_episode_overview(ep.overview.clone().unwrap_or_else(|| "No synopsis available for this episode.".to_string()).into());
                                ui.set_selected_episode_released(ep.released.clone().unwrap_or_default().into());
                            } else {
                                ui.set_selected_episode(1);
                                ui.set_selected_episode_title("Episode 1".into());
                                ui.set_selected_episode_overview("".into());
                                ui.set_selected_episode_released("".into());
                            }

                            // Populate episode cards for first season
                            let ep_items: Vec<EpisodeItem> = d.videos.iter()
                                .filter(|v| v.season as i32 == first_season)
                                .map(|v| EpisodeItem {
                                    id: v.id.clone().into(),
                                    season: v.season as i32,
                                    episode: v.episode as i32,
                                    title: v.title.clone().into(),
                                    released: v.released.clone().unwrap_or_default().into(),
                                    overview: v.overview.clone().unwrap_or_default().into(),
                                })
                                .collect();
                            ui.set_episode_items(ModelRc::from(Rc::new(VecModel::from(ep_items))));
                        }
                    }
                }
            });
        });
    });

    // 7. Season Selected Callback
    let app_weak_season = app.as_weak();
    app.on_season_selected(move |season| {
        let videos = CURRENT_SERIES_VIDEOS.lock().unwrap().clone();
        let ep_items: Vec<EpisodeItem> = videos.iter()
            .filter(|v| v.season as i32 == season)
            .map(|v| EpisodeItem {
                id: v.id.clone().into(),
                season: v.season as i32,
                episode: v.episode as i32,
                title: v.title.clone().into(),
                released: v.released.clone().unwrap_or_default().into(),
                overview: v.overview.clone().unwrap_or_default().into(),
            })
            .collect();

        if let Some(ui) = app_weak_season.upgrade() {
            ui.set_selected_season(season);
            let first_ep = videos.iter().find(|v| v.season as i32 == season);
            if let Some(ep) = first_ep {
                ui.set_selected_episode(ep.episode as i32);
                ui.set_selected_episode_title(ep.title.clone().into());
                ui.set_selected_episode_overview(ep.overview.clone().unwrap_or_else(|| "No synopsis available for this episode.".to_string()).into());
                ui.set_selected_episode_released(ep.released.clone().unwrap_or_default().into());
            } else {
                ui.set_selected_episode(1);
                ui.set_selected_episode_title("Episode 1".into());
                ui.set_selected_episode_overview("".into());
                ui.set_selected_episode_released("".into());
            }
            ui.set_episode_items(ModelRc::from(Rc::new(VecModel::from(ep_items))));
            let id_str = ui.get_detail_id().to_string();
            ui.invoke_fetch_episode_streams(id_str.into(), season, 1);
        }
    });

    // 8. Back to Catalog
    let app_weak = app.as_weak();
    app.on_back_to_catalog(move || {
        if let Some(ui) = app_weak.upgrade() {
            let prev = PAGE_STACK.lock().unwrap().pop().unwrap_or(0);
            ui.set_current_view(prev);
        }
    });

    // 9. Fetch Episode Streams for Series
    let app_weak = app.as_weak();
    app.on_fetch_episode_streams(move |id, season, episode| {
        let id_s = id.to_string();

        if let Some(ui) = app_weak.upgrade() {
            ui.set_selected_season(season);
            ui.set_selected_episode(episode);
            let videos = CURRENT_SERIES_VIDEOS.lock().unwrap().clone();
            if let Some(ep) = videos.iter().find(|v| v.season as i32 == season && v.episode as i32 == episode) {
                ui.set_selected_episode_title(ep.title.clone().into());
                ui.set_selected_episode_overview(ep.overview.clone().unwrap_or_else(|| "No synopsis available for this episode.".to_string()).into());
                ui.set_selected_episode_released(ep.released.clone().unwrap_or_default().into());
            }
            ui.set_is_streams_loading(true);
            ui.set_status_text(format!("Fetching streams for S{}E{}...", season, episode).into());
        }

        let app_weak2 = app_weak.clone();
        std::thread::spawn(move || {
            let streams = addons::fetch_streams("series", &id_s, season as u32, episode as u32).unwrap_or_default();
            *CURRENT_STREAMS.lock().unwrap() = streams.clone();
            let stream_models = convert_streams_to_models(&streams);

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = app_weak2.upgrade() {
                    ui.set_is_streams_loading(false);
                    ui.set_stream_items(ModelRc::from(Rc::new(VecModel::from(stream_models))));
                }
            });
        });
    });

    // 10. PLAY STREAM CLICKED: Immediate switch to in-player loading view (Stremio style)!
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
                    ui.get_selected_season() as u32,
                    ui.get_selected_episode() as u32,
                )
            } else {
                (String::new(), String::new(), 1, 1)
            };

            // Record watch progress in persistent watch history
            if !ui_detail_id.is_empty() {
                state::record_watch_progress(
                    &ui_detail_id,
                    &ui_detail_type,
                    &stream.display_title,
                    None,
                    if ui_detail_type == "series" { Some(ui_season) } else { None },
                    if ui_detail_type == "series" { Some(ui_episode) } else { None },
                    0,
                    100,
                );
            }

            let app_weak2 = app_weak.clone();
            std::thread::spawn(move || {
                let (stream_season, stream_episode) = if ui_detail_type == "series" {
                    (Some(ui_season), Some(ui_episode))
                } else {
                    (None, None)
                };

                match engine::start_stream_sync(
                    stream_clone.info_hash.clone(),
                    stream_clone.file_idx,
                    stream_season,
                    stream_episode,
                    stream_clone.trackers.clone(),
                ) {
                    Ok(proxy_url) => {
                        *ACTIVE_STREAM_URL.lock().unwrap() = proxy_url.clone();
                        log_player(&format!("Stream ready at {}. Initializing embedded player on UI thread...", proxy_url));

                        // Spawn subtitle discovery AFTER stream sync succeeds when video hash & bundled subs are known
                        if !ui_detail_id.is_empty() {
                            let sub_type = ui_detail_type.clone();
                            let sub_id = ui_detail_id.clone();
                            std::thread::spawn(move || {
                                let (v_hash, v_size) = crate::engine::get_current_video_hash()
                                    .map(|(h, s)| (Some(h), Some(s)))
                                    .unwrap_or((None, None));

                                let mut tracks = subtitles::fetch_subtitle_tracks(
                                    &sub_type,
                                    &sub_id,
                                    Some(ui_season),
                                    Some(ui_episode),
                                    v_hash.as_deref(),
                                    v_size,
                                ).unwrap_or_default();

                                // Inject bundled torrent subtitles if found
                                let bundled = crate::engine::get_bundled_subtitles();
                                for b in bundled {
                                    let local_url = format!("http://127.0.0.1:3030/torrents/0/stream/{}", b.file_idx);
                                    let clean_name = subtitles::clean_language_name(&b.lang);
                                    tracks.insert(0, state::SubtitleTrack {
                                        id: format!("bundled_{}", b.file_idx),
                                        lang: format!("{} [Embedded]", clean_name),
                                        url: local_url,
                                    });
                                }

                                *SUBTITLE_TRACKS.lock().unwrap() = tracks.clone();

                                // Auto-select track matching user preference
                                let settings = state::load_settings();
                                let pref_lang = settings.playback.sub_lang.to_lowercase();
                                let mut chosen_idx = 0;
                                for (i, t) in tracks.iter().enumerate() {
                                    if t.lang.to_lowercase().contains(&pref_lang) {
                                        chosen_idx = i;
                                        break;
                                    }
                                }

                                if let Some(track) = tracks.get(chosen_idx) {
                                    if let Ok(entries) = subtitles::download_and_parse_subtitle(&track.url) {
                                        subtitles::set_active_subtitles(entries, &track.lang);
                                        log_player(&format!("Auto-loaded subtitle track #{}: {}", chosen_idx, track.lang));
                                    }
                                }

                                let track_names: Vec<String> = tracks.iter().map(|t| {
                                    let badge = subtitles::get_sub_format_badge(&t.url);
                                    format!("{} {}", t.lang, badge)
                                }).collect();

                                let _ = slint::invoke_from_event_loop(move || {
                                    crate::ui::overlay::set_overlay_subtitle_tracks(track_names, chosen_idx);
                                });
                            });
                        }

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
                                *LAST_RECT.lock().unwrap() = (0, 0, 0, 0);

                                match player::init_embedded_player(v_hwnd, &proxy_url) {
                                    Ok(()) => {
                                        player::update_video_position(0, 0, w, h);
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
                                                overlay.set_stream_info_text(format!("{} • Container: {} • Audio: {}", stream_clone.quality, stream_clone.container, stream_clone.audio_codec).into());
                                                overlay.set_torrent_stats_text(format!("Seeds: {} • Size: {}", stream_clone.seeders, stream_clone.size).into());
                                                overlay.set_volume_percent((player::get_volume() * 100.0) as i32);
                                                overlay.set_is_muted(player::get_muted());
                                                overlay.set_sub_delay_ms(subtitles::get_subtitle_delay_ms() as i32);
                                                overlay.set_sub_size(1);
                                                overlay.set_subtitles_enabled(true);

                                                let (has_next, next_lbl, next_s, next_e) = if let (Some(s), Some(e)) = (stream_season, stream_episode) {
                                                    let videos = CURRENT_SERIES_VIDEOS.lock().unwrap().clone();
                                                    if let Some(next_v) = videos.iter().find(|v| (v.season == s && v.episode == e + 1) || (v.season == s + 1 && v.episode == 1)) {
                                                        (true, format!("S{}E{}", next_v.season, next_v.episode), next_v.season as i32, next_v.episode as i32)
                                                    } else {
                                                        (false, String::new(), 0, 0)
                                                    }
                                                } else {
                                                    (false, String::new(), 0, 0)
                                                };
                                                overlay.set_has_next_episode(has_next);
                                                overlay.set_next_episode_label(next_lbl.into());

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
                                                let info_hash_for_close = stream_clone.info_hash.clone();
                                                overlay.on_close_clicked(move || {
                                                    crate::ui::overlay::hide_overlay();
                                                    engine::cancel_stream_loading();
                                                    player::stop_embedded_player();
                                                    destroy_video_hwnd();

                                                    // Phase 5: Delete torrent payload after playback if keep_after_playback is false
                                                    let settings = state::load_settings();
                                                    if !settings.cache.keep_after_playback {
                                                        let hash = info_hash_for_close.clone();
                                                        std::thread::spawn(move || {
                                                            std::thread::sleep(std::time::Duration::from_millis(500));
                                                            engine::delete_torrent_cache(&hash);
                                                        });
                                                    }

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

                                                overlay.on_toggle_panel(move |panel| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    crate::ui::overlay::set_active_panel(panel);
                                                });

                                                let overlay_weak_sub = overlay.as_weak();
                                                overlay.on_sub_delay_adjust(move |delta| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let new_delay = subtitles::adjust_subtitle_delay_ms(delta as i64);
                                                    if let Some(ov) = overlay_weak_sub.upgrade() {
                                                        ov.set_sub_delay_ms(new_delay as i32);
                                                    }
                                                });

                                                let overlay_weak_sz = overlay.as_weak();
                                                overlay.on_sub_size_changed(move |sz| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    if let Some(ov) = overlay_weak_sz.upgrade() {
                                                        ov.set_sub_size(sz);
                                                    }
                                                });

                                                let overlay_weak_sub_tog = overlay.as_weak();
                                                overlay.on_toggle_subtitles_enabled(move |enabled| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    if let Some(ov) = overlay_weak_sub_tog.upgrade() {
                                                        ov.set_subtitles_enabled(enabled);
                                                    }
                                                });

                                                let overlay_weak_sub_sel = overlay.as_weak();
                                                overlay.on_select_subtitle_track(move |idx| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let tracks = SUBTITLE_TRACKS.lock().unwrap().clone();
                                                    if let Some(track) = tracks.get(idx as usize) {
                                                        let track_url = track.url.clone();
                                                        let track_lang = track.lang.clone();
                                                        let ov_weak = overlay_weak_sub_sel.clone();
                                                        std::thread::spawn(move || {
                                                            if let Ok(entries) = subtitles::download_and_parse_subtitle(&track_url) {
                                                                subtitles::set_active_subtitles(entries, &track_lang);
                                                                let _ = slint::invoke_from_event_loop(move || {
                                                                    if let Some(ov) = ov_weak.upgrade() {
                                                                        ov.set_selected_subtitle_idx(idx);
                                                                    }
                                                                });
                                                            }
                                                        });
                                                    }
                                                });

                                                let overlay_weak_vset = overlay.as_weak();
                                                overlay.on_volume_set(move |pct| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let val = pct.clamp(0, 100);
                                                    let _ = player::set_volume(val as f32 / 100.0);
                                                    let _ = player::set_muted(val == 0);
                                                    if let Some(ov) = overlay_weak_vset.upgrade() {
                                                        ov.set_volume_percent(val);
                                                        ov.set_is_muted(val == 0);
                                                    }
                                                });

                                                let overlay_weak_vol = overlay.as_weak();
                                                overlay.on_volume_adjust(move |delta| {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let cur = (player::get_volume() * 100.0) as i32;
                                                    let next = (cur + delta).clamp(0, 100);
                                                    let _ = player::set_volume(next as f32 / 100.0);
                                                    if let Some(ov) = overlay_weak_vol.upgrade() {
                                                        ov.set_volume_percent(next);
                                                        ov.set_is_muted(false);
                                                    }
                                                });

                                                let overlay_weak_mute = overlay.as_weak();
                                                overlay.on_toggle_mute_clicked(move || {
                                                    crate::ui::overlay::notify_user_activity();
                                                    let cur = player::get_muted();
                                                    let _ = player::set_muted(!cur);
                                                    if let Some(ov) = overlay_weak_mute.upgrade() {
                                                        ov.set_is_muted(!cur);
                                                    }
                                                });

                                                let app_weak_next = app_weak2.clone();
                                                let detail_id_next = ui_detail_id.clone();
                                                overlay.on_next_episode_clicked(move || {
                                                    crate::ui::overlay::notify_user_activity();
                                                    if has_next {
                                                        if let Some(ui) = app_weak_next.upgrade() {
                                                            ui.invoke_fetch_episode_streams(detail_id_next.clone().into(), next_s, next_e);
                                                        }
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

    // 11. Play VLC External clicked
    app.on_play_vlc_clicked(move |stream_idx| {
        let streams = CURRENT_STREAMS.lock().unwrap().clone();
        if let Some(stream) = streams.get(stream_idx as usize) {
            let stream_clone = stream.clone();
            std::thread::spawn(move || {
                if let Ok(proxy_url) = engine::start_stream_sync(
                    stream_clone.info_hash,
                    stream_clone.file_idx,
                    None,
                    None,
                    stream_clone.trackers,
                ) {
                    let _ = player::play_stream_in_vlc(&proxy_url, std::ptr::null_mut());
                }
            });
        }
    });

    // 12. Player Back clicked
    let app_weak = app.as_weak();
    app.on_player_back(move || {
        crate::ui::overlay::hide_overlay();
        engine::cancel_stream_loading();
        player::stop_embedded_player();
        destroy_video_hwnd();
        if let Some(ui) = app_weak.upgrade() {
            let prev = PAGE_STACK.lock().unwrap().pop().unwrap_or(0);
            ui.set_current_view(prev);
            ui.set_is_player_loading(false);
            refresh_library(&ui);
        }
    });

    // 13. Player Play / Pause
    let app_weak = app.as_weak();
    app.on_player_play_pause(move || {
        crate::ui::overlay::notify_user_activity();
        if let Ok(is_paused) = player::toggle_pause() {
            if let Some(ui) = app_weak.upgrade() {
                ui.set_is_paused(is_paused);
            }
        }
    });

    // 14. Seek Backward & Forward
    app.on_player_seek_backward(move || {
        crate::ui::overlay::notify_user_activity();
        let _ = player::seek_relative(-15);
    });

    app.on_player_seek_forward(move || {
        crate::ui::overlay::notify_user_activity();
        let _ = player::seek_relative(30);
    });

    // 15. Fullscreen Toggle (Real Slint fullscreen)
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
                let prev = PAGE_STACK.lock().unwrap().pop().unwrap_or(0);
                ui.set_current_view(prev);
                ui.set_is_player_loading(false);
                refresh_library(&ui);
            } else if ui.get_current_view() != 0 {
                let prev = PAGE_STACK.lock().unwrap().pop().unwrap_or(0);
                ui.set_current_view(prev);
            }
        }
    });

    // 16. Cycle Subtitles
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
                if let Ok(entries) = subtitles::download_and_parse_subtitle(&track_clone.url) {
                    subtitles::set_active_subtitles(entries, &track_clone.lang);
                    log_player(&format!("Subtitle activated: {}", track_clone.lang));
                }
            });
        }
    });

    // 17. Clear Diagnostic Logs
    let app_weak = app.as_weak();
    app.on_clear_logs(move || {
        let _ = logger::drain_ui_logs();
        if let Some(ui) = app_weak.upgrade() {
            let empty: Vec<SharedString> = Vec::new();
            ui.set_log_lines(ModelRc::from(Rc::new(VecModel::from(empty))));
        }
    });

    // 18. Player Open External VLC
    app.on_player_open_vlc(move || {
        let stream_url = ACTIVE_STREAM_URL.lock().unwrap().clone();
        if !stream_url.is_empty() {
            let _ = player::play_stream_in_vlc(&stream_url, std::ptr::null_mut());
        }
    });

    // 19. Periodic UI update timer (250 ms)
    let app_weak = app.as_weak();
    let mut hero_rotate_ticks: u32 = 0;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(250),
        move || {
            if let Some(ui) = app_weak.upgrade() {
                // Hero banner auto-rotation (every 10s if on Home view and reduce_animations is false)
                if ui.get_current_view() == 0 {
                    hero_rotate_ticks += 1;
                    if hero_rotate_ticks >= 40 {
                        hero_rotate_ticks = 0;
                        let settings = state::load_settings();
                        if !settings.ui.reduce_animations && settings.ui.hero_autorotate {
                            rotate_hero(&ui, 1);
                        }
                    }
                } else {
                    hero_rotate_ticks = 0;
                }

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

                        // Whenever the player transitions into active playback, update video position
                        // to ensure EVR renders the first frame immediately without needing a manual window resize
                        if st >= 1 && st <= 3 {
                            let slint_hwnd = get_slint_hwnd(&ui.window());
                            if !slint_hwnd.is_null() {
                                unsafe {
                                    let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                                    windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(slint_hwnd, &mut rc);
                                    let w = (rc.right - rc.left).max(100);
                                    let h = (rc.bottom - rc.top).max(100);
                                    player::update_video_position(0, 0, w, h);
                                }
                            }
                        }
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

                    // Auto-record watch progress every 10s while actively playing
                    static WATCH_TICKS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    let tick = WATCH_TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if tick % 40 == 0 && !is_paused && progress > 0.01 {
                        let (pos_ms, dur_ms, _) = player::get_playback_progress();
                        if dur_ms > 0 {
                            let meta_id = ui.get_detail_id().to_string();
                            let itype = ui.get_detail_type().to_string();
                            let title = ui.get_detail_title().to_string();
                            let s = ui.get_selected_season();
                            let e = ui.get_selected_episode();
                            state::record_watch_progress(
                                &meta_id,
                                &itype,
                                &title,
                                None,
                                if itype == "series" { Some(s as u32) } else { None },
                                if itype == "series" { Some(e as u32) } else { None },
                                pos_ms / 1000,
                                dur_ms / 1000,
                            );
                        }
                    }

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
