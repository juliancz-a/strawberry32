use crate::addons::{
    addon_supports_resource, build_catalog_url, install_addon_from_url_with_warning,
    parse_manifest_bytes,
};
use crate::net::{
    classify_io_error, classify_ureq_error, connect_with_resolver, NetErrorKind,
};
use crate::redact::{is_loopback_host, is_private_host, redact_secrets, redact_url};
use crate::sources::jellyfin::client::{
    authenticate_by_name, fetch_episodes, fetch_item_detail, fetch_items, fetch_seasons,
    fetch_user_views, test_connection,
};
use crate::sources::jellyfin::mapping::{
    map_item_to_meta_detail, map_items_to_meta_items, map_media_streams_to_subtitles,
};
use crate::sources::jellyfin::playback::{choose_playback, DeviceCaps, PlaybackDecision};
use crate::sources::jellyfin::types::{JellyfinItem, MediaSource, MediaStream};
use std::io::{Error, ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::time::Duration;

#[test]
fn u1_redact_url_and_secrets() {
    let fake_b64 = "eyJBdXRoVG9rZW4iOiJTRUNSRVRfVE9LRU5fMTIzNDUiLCJTZXJ2ZXJOYW1lIjoiamVsbHktc3YifQ==";
    let fake_addon_url = format!("http://192.168.1.50:8096/jellio/{}/manifest.json", fake_b64);
    let r1 = redact_url(&fake_addon_url);
    assert!(!r1.contains(fake_b64), "base64 AuthToken leaked: {}", r1);
    assert!(!r1.contains("SECRET_TOKEN_12345"));
    assert!(r1.starts_with("http://192.168.1.50:8096/jellio/***"));

    let q_url = "https://jf.example.com:8096/Videos/abc/stream?static=true&api_key=MY_SECRET_KEY&X-Emby-Token=EMBY_TOK_99&token=TOK_88";
    let r2 = redact_url(q_url);
    assert!(!r2.contains("MY_SECRET_KEY"), "api_key leaked: {}", r2);
    assert!(!r2.contains("EMBY_TOK_99"), "X-Emby-Token leaked: {}", r2);
    assert!(!r2.contains("TOK_88"), "token leaked: {}", r2);
    assert!(r2.contains("api_key=***"));
    assert!(r2.contains("X-Emby-Token=***"));

    let err_txt = format!(
        "Failed installing add-on: Network error fetching manifest: {}: Connection Failed; Authorization: MediaBrowser Token=\"HEADER_TOK_77\"",
        fake_addon_url
    );
    let r3 = redact_secrets(&err_txt);
    assert!(!r3.contains(fake_b64), "base64 leaked in error text: {}", r3);
    assert!(!r3.contains("HEADER_TOK_77"), "header token leaked: {}", r3);
}

#[test]
fn u2_manifest_parser() {
    let real_jellio = r#"{
        "id":"com.stremio.jellio",
        "version":"0.0.1",
        "name":"Jellio",
        "description":"Play movies and series from jelly-sv: Pel\u00EDculas",
        "resources":["catalog","stream",{"name":"meta","types":["movie","series"],"idPrefixes":["jellio"]}],
        "types":["movie","series"],
        "idPrefixes":["tt","jellio"],
        "contactEmail":"support@jellio.stream",
        "behaviorHints":{"configurable":true},
        "catalogs":[{"type":"movie","id":"ed2a2528-6c55-8a96-e142-4971742ca250","name":"Pel\u00EDculas | jelly-sv","extra":[{"name":"skip","isRequired":false},{"name":"search","isRequired":false}]}]
    }"#;
    let m = parse_manifest_bytes(real_jellio.as_bytes()).expect("real jellio manifest should parse");
    assert_eq!(m.id, "com.stremio.jellio");
    assert_eq!(m.resources.len(), 3);
    assert_eq!(m.catalogs[0].extra.len(), 2);

    let plain_stremio = r#"{
        "id":"org.example.addon",
        "version":"1.2.3",
        "name":"Plain Addon",
        "resources":["catalog","meta","stream"],
        "types":["movie"],
        "extraUnknownField": 42
    }"#;
    let m2 = parse_manifest_bytes(plain_stremio.as_bytes()).expect("plain manifest should parse");
    assert_eq!(m2.id, "org.example.addon");

    let invalid = r#"{"name":"Missing Fields"}"#;
    let err = parse_manifest_bytes(invalid.as_bytes()).unwrap_err();
    assert_eq!(err.kind, NetErrorKind::InvalidManifest);
    assert!(err.user_message.contains("id"));
    assert!(err.user_message.contains("version"));
    assert!(err.user_message.contains("resources"));
    assert!(err.user_message.contains("types"));
}

#[test]
fn u3_router_and_catalog_extra() {
    let real_jellio = r#"{
        "id":"com.stremio.jellio",
        "version":"0.0.1",
        "name":"Jellio",
        "resources":["catalog","stream",{"name":"meta","types":["movie","series"],"idPrefixes":["jellio"]}],
        "types":["movie","series"],
        "idPrefixes":["tt","jellio"],
        "catalogs":[]
    }"#;
    let m = parse_manifest_bytes(real_jellio.as_bytes()).unwrap();

    assert!(addon_supports_resource(&m, "meta", "movie", Some("jellio:abc")));
    assert!(!addon_supports_resource(&m, "meta", "movie", Some("tt123")));
    assert!(addon_supports_resource(&m, "stream", "movie", Some("tt123")));
    assert!(addon_supports_resource(&m, "stream", "movie", Some("jellio:abc")));
    assert!(!addon_supports_resource(&m, "stream", "anime", Some("tt123")));

    let url1 = build_catalog_url(
        "http://127.0.0.1:8096/jellio/tok/manifest.json",
        "movie",
        "cat1",
        &[("skip", "20"), ("search", "la casa azul")],
    );
    assert_eq!(
        url1,
        "http://127.0.0.1:8096/jellio/tok/catalog/movie/cat1/skip=20&search=la%20casa%20azul.json"
    );

    let url2 = build_catalog_url(
        "http://127.0.0.1:8096/jellio/tok",
        "movie",
        "cat1",
        &[("search", "canción")],
    );
    assert_eq!(
        url2,
        "http://127.0.0.1:8096/jellio/tok/catalog/movie/cat1/search=canci%C3%B3n.json"
    );
}

#[test]
fn u4_error_classification() {
    let refused = classify_io_error(&Error::new(ErrorKind::ConnectionRefused, "os error 10061"));
    assert_eq!(refused.kind, NetErrorKind::ConnectionRefused);

    let timeout = classify_io_error(&Error::new(ErrorKind::TimedOut, "timed out"));
    assert_eq!(timeout.kind, NetErrorKind::Timeout);

    let dns = classify_io_error(&Error::new(ErrorKind::Other, "failed to lookup address information"));
    assert_eq!(dns.kind, NetErrorKind::DnsFailure);

    let tls = classify_ureq_error(&ureq::Error::Transport(
        ureq::Agent::new()
            .get("https://127.0.0.1:1/test")
            .call()
            .unwrap_err()
            .into_transport()
            .unwrap(),
    ));
    assert!(matches!(
        tls.kind,
        NetErrorKind::ConnectionRefused | NetErrorKind::TlsError | NetErrorKind::Other
    ));

    let resp_401 = ureq::Response::new(401, "Unauthorized", "").unwrap();
    assert_eq!(
        classify_ureq_error(&ureq::Error::Status(401, resp_401)).kind,
        NetErrorKind::HttpUnauthorized
    );
    let resp_403 = ureq::Response::new(403, "Forbidden", "").unwrap();
    assert_eq!(
        classify_ureq_error(&ureq::Error::Status(403, resp_403)).kind,
        NetErrorKind::HttpUnauthorized
    );
    let resp_404 = ureq::Response::new(404, "Not Found", "").unwrap();
    assert_eq!(
        classify_ureq_error(&ureq::Error::Status(404, resp_404)).kind,
        NetErrorKind::HttpNotFound
    );
    let resp_500 = ureq::Response::new(500, "Internal Server Error", "").unwrap();
    assert_eq!(
        classify_ureq_error(&ureq::Error::Status(500, resp_500)).kind,
        NetErrorKind::HttpServerError
    );

    let bad_json = parse_manifest_bytes(b"{not valid json").unwrap_err();
    assert_eq!(bad_json.kind, NetErrorKind::InvalidJson);
}

#[test]
fn u5_loopback_and_private_host_detection() {
    for h in ["localhost", "127.0.0.1", "::1"] {
        assert!(is_loopback_host(h), "{} should be loopback", h);
        assert!(is_private_host(h), "{} should be private", h);
    }
    for h in [
        "10.0.0.5",
        "172.16.0.1",
        "172.31.255.254",
        "192.168.1.10",
        "169.254.10.20",
    ] {
        assert!(!is_loopback_host(h), "{} should not be loopback", h);
        assert!(is_private_host(h), "{} should be private", h);
    }
    for h in ["8.8.8.8", "example.com", "172.32.0.1", "11.0.0.1"] {
        assert!(!is_private_host(h), "{} should be public", h);
    }
}

#[test]
fn u6_dual_stack_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
    let port = listener.local_addr().unwrap().port();
    let v6_addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port);
    let v4_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);

    let stream = connect_with_resolver("localhost", port, Duration::from_secs(2), |_, _| {
        Ok(vec![v6_addr, v4_addr])
    })
    .expect("should fall back from ::1 to 127.0.0.1");
    assert_eq!(stream.peer_addr().unwrap().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
}

#[test]
fn u7_choose_playback() {
    let caps = DeviceCaps::default(); // 1280x720, 8-bit H.264 L4.1, AAC/MP3

    let make_ms = |container: &str, v_codec: &str, bit: u32, w: u32, h: u32, a_codec: &str| MediaSource {
        id: "ms1".to_string(),
        name: None,
        container: Some(container.to_string()),
        size: Some(1_000_000_000),
        bitrate: None,
        media_streams: vec![
            MediaStream {
                stream_type: "Video".to_string(),
                codec: v_codec.to_string(),
                bit_depth: Some(bit),
                width: Some(w),
                height: Some(h),
                level: Some(40.0),
                ..Default::default()
            },
            MediaStream {
                stream_type: "Audio".to_string(),
                codec: a_codec.to_string(),
                ..Default::default()
            },
        ],
    };

    assert!(matches!(
        choose_playback(&make_ms("mp4", "h264", 8, 1280, 720, "aac"), &caps),
        PlaybackDecision::Direct { .. }
    ));
    assert!(matches!(
        choose_playback(&make_ms("mkv", "h264", 8, 1280, 720, "aac"), &caps),
        PlaybackDecision::Transcode(_)
    ));
    assert!(matches!(
        choose_playback(&make_ms("mp4", "hevc", 8, 1280, 720, "aac"), &caps),
        PlaybackDecision::Transcode(_)
    ));
    assert!(matches!(
        choose_playback(&make_ms("mp4", "h264", 10, 1280, 720, "aac"), &caps),
        PlaybackDecision::Transcode(_)
    ));
    assert!(matches!(
        choose_playback(&make_ms("mp4", "h264", 8, 1280, 720, "dts"), &caps),
        PlaybackDecision::Transcode(_)
    ));
    match choose_playback(&make_ms("mp4", "h264", 8, 1920, 1080, "eac3"), &caps) {
        PlaybackDecision::Transcode(p) => {
            assert_eq!(p.max_width, 1280);
            assert_eq!(p.max_height, 720);
            assert_eq!(p.video_bitrate, caps.video_bitrate);
            assert_eq!(p.audio_bitrate, caps.audio_bitrate);
        }
        _ => panic!("expected transcode for 1080p + eac3 with 720p cap"),
    }
}

#[test]
fn u8_jellyfin_response_mapping() {
    let items_json = r#"{
        "Items": [
            {
                "Id": "fake_movie_1",
                "Name": "Home Video 2025",
                "Type": "Movie",
                "ProductionYear": 2025,
                "Overview": "Family trip",
                "Genres": ["Documentary"],
                "CommunityRating": 8.5,
                "RunTimeTicks": 36000000000,
                "ImageTags": {"Primary": "tag1"},
                "BackdropImageTags": ["bg1"],
                "MediaSources": [{
                    "Id": "ms_1",
                    "Container": "mp4",
                    "MediaStreams": [
                        {"Type": "Video", "Codec": "h264", "BitDepth": 8, "Width": 1280, "Height": 720},
                        {"Type": "Audio", "Codec": "aac", "Language": "eng"},
                        {"Type": "Subtitle", "Codec": "subrip", "Language": "spa", "DisplayTitle": "Spanish", "Index": 2}
                    ]
                }]
            }
        ],
        "TotalRecordCount": 1
    }"#;
    let parsed: crate::sources::jellyfin::types::ItemsResponse =
        serde_json::from_str(items_json).unwrap();
    let mapped = map_items_to_meta_items(
        "srv1",
        "http://192.168.1.10:8096",
        "FAKE_TOK",
        &parsed.items,
        280,
    );
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0].id, "jellyfin:srv1:fake_movie_1");
    assert!(mapped[0].poster.as_ref().unwrap().contains("maxWidth=280"));

    let ep = JellyfinItem {
        id: "ep_1".to_string(),
        name: "Pilot".to_string(),
        item_type: "Episode".to_string(),
        parent_index_number: Some(1),
        index_number: Some(1),
        ..Default::default()
    };
    let detail = map_item_to_meta_detail(
        "srv1",
        "http://192.168.1.10:8096",
        "FAKE_TOK",
        &parsed.items[0],
        &[ep],
    );
    assert_eq!(detail.videos.len(), 1);
    assert_eq!(detail.videos[0].season, 1);

    let subs = map_media_streams_to_subtitles(
        "http://192.168.1.10:8096",
        "FAKE_TOK",
        "fake_movie_1",
        "ms_1",
        &parsed.items[0].media_sources[0].media_streams,
    );
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].lang, "spa");
    assert!(subs[0].url.contains("/Subtitles/2/Stream.srt"));
}

fn load_dotenv_var(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        let t = v.trim().trim_matches('"').trim_matches('\'').to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Ok(content) = std::fs::read_to_string(".env") {
        for line in content.lines() {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == key {
                    let t = v.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !t.is_empty() {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

#[test]
#[ignore]
fn live_l1_server_check() {
    let Some(url) = load_dotenv_var("JELLYFIN_URL") else {
        println!("SKIPPED: JELLYFIN_URL not set");
        return;
    };
    let info = test_connection(&url).expect("L1 /System/Info/Public failed");
    println!("L1 Server: {} v{}", info.server_name, info.version);
    assert!(!info.version.is_empty());
}

#[test]
#[ignore]
fn live_l2_dual_stack_localhost_install() {
    let Some(addon_url) = load_dotenv_var("JELLIO_ADDON_URL") else {
        println!("SKIPPED: JELLIO_ADDON_URL not set");
        return;
    };
    let localhost_url = addon_url.replace("127.0.0.1", "localhost");
    let prev_cfg = crate::state::load_addons_config();
    let (desc, warning) =
        install_addon_from_url_with_warning(&localhost_url).expect("L2 localhost install failed");
    crate::state::save_addons_config(&prev_cfg);
    println!("L2 Installed '{}' | warning={:?}", desc.manifest.name, warning);
    assert!(warning.is_some());
}

#[test]
#[ignore]
fn live_l3_addon_protocol() {
    let Some(addon_url) = load_dotenv_var("JELLIO_ADDON_URL") else {
        println!("SKIPPED: JELLIO_ADDON_URL not set");
        return;
    };
    let base = addon_url.trim_end_matches("/manifest.json").trim_end_matches('/');
    let agent = crate::addons::create_agent().unwrap();
    let m_resp = agent.get(&format!("{}/manifest.json", base)).call().expect("manifest GET");
    let m_bytes = crate::net::read_body_capped(m_resp.into_reader(), 2 * 1024 * 1024).unwrap();
    let manifest = parse_manifest_bytes(&m_bytes).expect("L3 manifest parse");
    println!("L3 Manifest OK: {} ({} catalogs)", manifest.name, manifest.catalogs.len());

    if let Some(cat) = manifest.catalogs.first() {
        let cat_url = build_catalog_url(base, &cat.catalog_type, &cat.id, &[]);
        match agent.get(&cat_url).call() {
            Ok(r) => {
                println!("L3 Catalog HTTP {}", r.status());
            }
            Err(ureq::Error::Status(500, _)) => {
                println!("L3 Catalog returned HTTP 500 on Jellyfin 12.4.0 (known Jellio+ 10.11.x plugin incompatibility)");
            }
            Err(e) => panic!("L3 Catalog unexpected error: {}", crate::net::classify_ureq_error(&e).user_message),
        }
    }
}

#[test]
#[ignore]
fn live_l4_not_running_behavior() {
    let err = install_addon_from_url_with_warning("http://127.0.0.1:59999/jellio/eyJBdXRoVG9rZW4iOiJTRUNSRVRfMTIzIn0=/manifest.json")
        .unwrap_err();
    println!("L4 Error message: {}", err);
    assert!(err.to_ascii_lowercase().contains("not reachable") || err.to_ascii_lowercase().contains("refused"));
    assert!(!err.contains("SECRET_123"));
    assert!(!err.contains("eyJBdXRo"));
}

#[test]
#[ignore]
fn live_l5_native_login_views_items_images() {
    let (Some(url), Some(user), Some(pass)) = (
        load_dotenv_var("JELLYFIN_URL"),
        load_dotenv_var("JELLYFIN_USER"),
        load_dotenv_var("JELLYFIN_PASS"),
    ) else {
        println!("SKIPPED: JELLYFIN_URL / JELLYFIN_USER / JELLYFIN_PASS not all set");
        return;
    };

    let srv = authenticate_by_name(&url, &user, &pass).expect("L5 auth failed");
    let views = fetch_user_views(&srv).expect("L5 UserViews failed");
    println!("L5 Views: {}", views.len());
    assert!(!views.is_empty());

    let items = fetch_items(&srv, Some(&views[0].id), 0, 40, None, None, None, None)
        .expect("L5 Items failed");
    println!("L5 Items: {} / {}", items.items.len(), items.total_record_count);
    assert!(!items.items.is_empty());

    let first = &items.items[0];
    let detail = fetch_item_detail(&srv, &first.id).expect("L5 detail failed");
    assert_eq!(detail.id, first.id);

    let img_url = crate::sources::jellyfin::mapping::build_image_url(
        &srv.server_url,
        &first.id,
        "Primary",
        280,
        &srv.access_token,
    );
    let img_resp = crate::addons::get_shared_agent().get(&img_url).call().expect("L5 image GET");
    assert_eq!(img_resp.status(), 200);

    if let Some(series) = items.items.iter().find(|i| i.item_type.eq_ignore_ascii_case("Series")) {
        let seasons = fetch_seasons(&srv, &series.id).expect("L5 seasons");
        let episodes = fetch_episodes(&srv, &series.id, None).expect("L5 episodes");
        println!("L5 Series '{}' seasons={}, episodes={}", series.name, seasons.len(), episodes.len());
    }
}

#[test]
#[ignore]
fn live_l6_playback_urls() {
    let (Some(url), Some(user), Some(pass)) = (
        load_dotenv_var("JELLYFIN_URL"),
        load_dotenv_var("JELLYFIN_USER"),
        load_dotenv_var("JELLYFIN_PASS"),
    ) else {
        println!("SKIPPED: JELLYFIN_URL / JELLYFIN_USER / JELLYFIN_PASS not all set");
        return;
    };
    let srv = authenticate_by_name(&url, &user, &pass).expect("L6 auth failed");
    let items = fetch_items(&srv, None, 0, 10, None, None, None, None).expect("L6 items failed");
    let item = items.items.first().expect("L6 needs at least 1 item");
    let ms = item.media_sources.first().expect("L6 needs MediaSource");
    let caps = DeviceCaps::default();
    let decision = choose_playback(ms, &caps);
    let agent = crate::addons::get_shared_agent();

    let play_url = crate::sources::jellyfin::playback::build_playback_url(
        &srv.server_url,
        &item.id,
        &ms.id,
        &decision,
        &srv.access_token,
        None,
    );
    match decision {
        PlaybackDecision::Direct { .. } => {
            let r = agent.get(&play_url).set("Range", "bytes=0-1").call().expect("L6 direct range");
            assert_eq!(r.status(), 206);
        }
        PlaybackDecision::Transcode(_) => {
            let r = agent.get(&play_url).call().expect("L6 transcode GET");
            assert_eq!(r.status(), 200);
            let ct = r.header("Content-Type").unwrap_or("").to_string();
            assert!(ct.contains("video/mp4"), "unexpected Content-Type: {}", ct);
            let mut buf = vec![0u8; 64 * 1024];
            let mut reader = r.into_reader();
            let mut total = 0;
            while total < 64 * 1024 {
                let n = reader.read(&mut buf[total..]).unwrap_or(0);
                if n == 0 { break; }
                total += n;
            }
            assert!(total > 0, "transcode stream returned 0 bytes");

            let seek_url = crate::sources::jellyfin::playback::build_playback_url(
                &srv.server_url,
                &item.id,
                &ms.id,
                &decision,
                &srv.access_token,
                Some(100_000_000),
            );
            let r2 = agent.get(&seek_url).call().expect("L6 transcode seek GET");
            assert_eq!(r2.status(), 200);
            println!("L6 Transcode OK ({} bytes read, seek 200 OK)", total);
        }
    }
}

#[test]
#[ignore]
fn live_l7_subtitles() {
    let (Some(url), Some(user), Some(pass)) = (
        load_dotenv_var("JELLYFIN_URL"),
        load_dotenv_var("JELLYFIN_USER"),
        load_dotenv_var("JELLYFIN_PASS"),
    ) else {
        println!("SKIPPED: JELLYFIN_URL / JELLYFIN_USER / JELLYFIN_PASS not all set");
        return;
    };
    let srv = authenticate_by_name(&url, &user, &pass).expect("L7 auth failed");
    let items = fetch_items(&srv, None, 0, 10, None, None, None, None).expect("L7 items failed");
    for item in &items.items {
        if let Some(ms) = item.media_sources.first() {
            let tracks = map_media_streams_to_subtitles(
                &srv.server_url,
                &srv.access_token,
                &item.id,
                &ms.id,
                &ms.media_streams,
            );
            if let Some(first_sub) = tracks.first() {
                let sub_agent = ureq::AgentBuilder::new()
                    .resolver(crate::net::DualStackResolver)
                    .timeout_connect(Duration::from_secs(3))
                    .timeout_read(Duration::from_secs(35))
                    .build();
                let r = sub_agent
                    .get(&first_sub.url)
                    .call()
                    .map_err(|e| classify_ureq_error(&e).user_message)
                    .expect("L7 sub fetch failed");
                assert_eq!(r.status(), 200);
                println!("L7 Fetched subtitle '{}' OK", first_sub.label);
                return;
            }
        }
    }
    println!("L7: No text subtitle tracks found in library");
}

#[test]
#[ignore]
fn live_l8_secret_leakage_scan() {
    let mut secrets: Vec<String> = Vec::new();
    if let Some(p) = load_dotenv_var("JELLYFIN_PASS") {
        secrets.push(p);
    }
    if let Some(addon_url) = load_dotenv_var("JELLIO_ADDON_URL") {
        if let Some(idx) = addon_url.find("/jellio/") {
            let rest = &addon_url[idx + "/jellio/".len()..];
            let b64 = rest.split('/').next().unwrap_or("");
            if !b64.is_empty() {
                secrets.push(b64.to_string());
            }
        }
        let prev = crate::state::load_addons_config();
        let _ = install_addon_from_url_with_warning(&addon_url);
        crate::state::save_addons_config(&prev);
    }
    if let (Some(u), Some(usr), Some(pwd)) = (
        load_dotenv_var("JELLYFIN_URL"),
        load_dotenv_var("JELLYFIN_USER"),
        load_dotenv_var("JELLYFIN_PASS"),
    ) {
        if let Ok(srv) = authenticate_by_name(&u, &usr, &pwd) {
            secrets.push(srv.access_token);
        }
    }

    let recent = crate::logger::get_recent_logs();
    for line in &recent {
        for sec in &secrets {
            assert!(!line.contains(sec), "Secret leaked in recent logs!");
        }
    }
    let log_file = crate::state::get_logs_dir().join("strawberry32_debug.log");
    if let Ok(content) = std::fs::read_to_string(&log_file) {
        for sec in &secrets {
            assert!(!content.contains(sec), "Secret leaked in log file!");
        }
    }
    println!("L8 Secret scan passed across {} recent log lines (0 leaks).", recent.len());
}

#[test]
fn u9_metadetail_null_arrays() {
    let raw = r#"{
        "id": "tt1234567",
        "name": "Null Fields Movie",
        "type": "movie",
        "year": 2025,
        "cast": null,
        "director": null,
        "genres": null,
        "videos": null
    }"#;
    let parsed: crate::state::MetaDetail = serde_json::from_str(raw).expect("MetaDetail should tolerate null arrays");
    assert!(parsed.cast.is_empty());
    assert!(parsed.director.is_empty());
    assert!(parsed.genres.is_empty());
    assert!(parsed.videos.is_empty());
}

#[test]
fn u10_search_deduplication_and_jellyfin_only() {
    use crate::addons::MetaItem;
    let make_item = |id: &str, name: &str, year: &str, ty: &str| MetaItem {
        id: id.to_string(),
        name: name.to_string(),
        item_type: Some(ty.to_string()),
        year: Some(year.to_string()),
        poster: None,
        background: None,
        description: None,
        release_info: Some(year.to_string()),
        imdb_rating: None,
        runtime: None,
        genres: Vec::new(),
    };

    let cm = vec![
        make_item("tt0074991", "Obsession", "1976", "movie"),
        make_item("tt9999999", "Obsession", "2019", "movie"),
        make_item("tt37287335", "Obsession", "2025", "movie"),
        make_item("tt0000001", "Other Movie", "2024", "movie"),
    ];
    let jf = vec![
        make_item("jellyfin:srv1:abc", "Obsession", "2025", "movie"),
        make_item("jellyfin:srv1:home1", "Family Trip", "2023", "movie"),
    ];

    let combined = crate::sources::jellyfin::deduplicate_search_results(cm.clone(), jf.clone(), false);
    assert_eq!(combined.len(), 5);
    assert_eq!(combined[2].id, "tt37287335");
    assert_eq!(combined[4].id, "jellyfin:srv1:home1");

    let jf_only = crate::sources::jellyfin::deduplicate_search_results(cm, jf, true);
    assert_eq!(jf_only.len(), 2);
    assert_eq!(jf_only[0].id, "tt37287335");
    assert_eq!(jf_only[1].id, "jellyfin:srv1:home1");
}

