use super::config::JellyfinServerConfig;
use super::types::{
    AuthResponse, ItemsResponse, JellyfinItem, PublicSystemInfo, QuickConnectResult, UserView,
    UserViewsResponse,
};
use crate::addons::{get_shared_agent, percent_encode};
use crate::logger::{log_err, log_net};
use crate::net::{classify_ureq_error, read_body_capped, validate_transport_policy};
use crate::redact::{extract_host, redact_url};
use std::time::{SystemTime, UNIX_EPOCH};

const CLIENT_NAME: &str = "Strawberry32";
const DEVICE_NAME: &str = "SurfaceRT";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn normalize_server_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Please enter a Jellyfin server URL.".to_string());
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        let host = extract_host(trimmed);
        if crate::redact::is_private_host(&host) {
            format!("http://{}", trimmed)
        } else {
            format!("https://{}", trimmed)
        }
    };
    validate_transport_policy(&with_scheme).map_err(|e| e.user_message)?;
    Ok(with_scheme)
}

fn device_auth_header(device_id: &str) -> String {
    format!(
        "MediaBrowser Client=\"{}\", Device=\"{}\", DeviceId=\"{}\", Version=\"{}\"",
        CLIENT_NAME, DEVICE_NAME, device_id, CLIENT_VERSION
    )
}

fn token_auth_header(token: &str) -> String {
    format!("MediaBrowser Token=\"{}\"", token)
}

fn generate_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}_{:x}", prefix, nanos)
}

/// Calls `GET /System/Info/Public` (no auth required).
pub fn test_connection(server_url: &str) -> Result<PublicSystemInfo, String> {
    let base = normalize_server_url(server_url)?;
    let url = format!("{}/System/Info/Public", base);
    log_net(&format!("Testing Jellyfin connection: {}", redact_url(&url)));

    let resp = get_shared_agent().get(&url).call().map_err(|e| {
        let ce = classify_ureq_error(&e);
        log_err(&format!("Jellyfin test_connection failed: {}", ce.detail));
        ce.user_message
    })?;

    let bytes = read_body_capped(resp.into_reader(), 1024 * 1024).map_err(|e| e.user_message)?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid JSON from Jellyfin /System/Info/Public.".to_string())
}

/// Authenticates with username + password via `POST /Users/AuthenticateByName`.
/// Never persists the password.
pub fn authenticate_by_name(
    server_url: &str,
    username: &str,
    password: &str,
) -> Result<JellyfinServerConfig, String> {
    let base = normalize_server_url(server_url)?;
    let info = test_connection(&base)?;
    let device_id = generate_id("sb32dev");
    let url = format!("{}/Users/AuthenticateByName", base);
    log_net(&format!("Authenticating to Jellyfin: {}", redact_url(&url)));

    let payload = serde_json::json!({
        "Username": username.trim(),
        "Pw": password,
    });

    let resp = get_shared_agent()
        .post(&url)
        .set("Authorization", &device_auth_header(&device_id))
        .send_json(payload)
        .map_err(|e| {
            let ce = classify_ureq_error(&e);
            log_err(&format!("Jellyfin auth failed: {}", ce.detail));
            if matches!(ce.kind, crate::net::NetErrorKind::HttpUnauthorized) {
                "Invalid Jellyfin username or password.".to_string()
            } else {
                ce.user_message
            }
        })?;

    let bytes = read_body_capped(resp.into_reader(), 1024 * 1024).map_err(|e| e.user_message)?;
    let auth: AuthResponse = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid JSON from Jellyfin /Users/AuthenticateByName.".to_string())?;

    let server_name = if info.server_name.trim().is_empty() {
        extract_host(&base)
    } else {
        info.server_name
    };

    Ok(JellyfinServerConfig {
        id: generate_id("jf"),
        name: server_name,
        server_url: base,
        user_id: auth.user.id,
        username: auth.user.name,
        access_token: auth.access_token,
        device_id,
    })
}

/// Checks if Quick Connect is enabled on the server (`GET /QuickConnect/Enabled`).
pub fn quick_connect_enabled(server_url: &str) -> Result<bool, String> {
    let base = normalize_server_url(server_url)?;
    let url = format!("{}/QuickConnect/Enabled", base);
    let resp = get_shared_agent()
        .get(&url)
        .call()
        .map_err(|e| classify_ureq_error(&e).user_message)?;
    let bytes = read_body_capped(resp.into_reader(), 64 * 1024).map_err(|e| e.user_message)?;
    serde_json::from_slice::<bool>(&bytes).map_err(|_| "Invalid QuickConnect response.".to_string())
}

/// Initiates Quick Connect (`POST /QuickConnect/Initiate`) and returns `(secret, code, device_id)`.
pub fn quick_connect_initiate(server_url: &str) -> Result<(QuickConnectResult, String), String> {
    let base = normalize_server_url(server_url)?;
    let device_id = generate_id("sb32dev");
    let url = format!("{}/QuickConnect/Initiate", base);
    let resp = get_shared_agent()
        .post(&url)
        .set("Authorization", &device_auth_header(&device_id))
        .call()
        .map_err(|e| classify_ureq_error(&e).user_message)?;
    let bytes = read_body_capped(resp.into_reader(), 64 * 1024).map_err(|e| e.user_message)?;
    let res: QuickConnectResult = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid QuickConnect initiate response.".to_string())?;
    Ok((res, device_id))
}

/// Polls `GET /QuickConnect/Connect?secret=...` and exchanges for an access token if authenticated.
pub fn quick_connect_poll_and_auth(
    server_url: &str,
    secret: &str,
    device_id: &str,
) -> Result<Option<JellyfinServerConfig>, String> {
    let base = normalize_server_url(server_url)?;
    let check_url = format!("{}/QuickConnect/Connect?secret={}", base, percent_encode(secret));
    let resp = get_shared_agent()
        .get(&check_url)
        .call()
        .map_err(|e| classify_ureq_error(&e).user_message)?;
    let bytes = read_body_capped(resp.into_reader(), 64 * 1024).map_err(|e| e.user_message)?;
    let status: QuickConnectResult = serde_json::from_slice(&bytes)
        .map_err(|_| "Invalid QuickConnect status response.".to_string())?;
    if !status.authenticated {
        return Ok(None);
    }

    let info = test_connection(&base)?;
    let auth_url = format!("{}/Users/AuthenticateWithQuickConnect", base);
    let resp = get_shared_agent()
        .post(&auth_url)
        .set("Authorization", &device_auth_header(device_id))
        .send_json(serde_json::json!({ "Secret": secret }))
        .map_err(|e| classify_ureq_error(&e).user_message)?;
    let auth_bytes = read_body_capped(resp.into_reader(), 1024 * 1024).map_err(|e| e.user_message)?;
    let auth: AuthResponse = serde_json::from_slice(&auth_bytes)
        .map_err(|_| "Invalid QuickConnect auth response.".to_string())?;

    let server_name = if info.server_name.trim().is_empty() {
        extract_host(&base)
    } else {
        info.server_name
    };

    Ok(Some(JellyfinServerConfig {
        id: generate_id("jf"),
        name: server_name,
        server_url: base,
        user_id: auth.user.id,
        username: auth.user.name,
        access_token: auth.access_token,
        device_id: device_id.to_string(),
    }))
}

fn jf_get_json<T: serde::de::DeserializeOwned>(
    server: &JellyfinServerConfig,
    path_and_query: &str,
) -> Result<T, String> {
    let url = format!("{}{}", server.server_url.trim_end_matches('/'), path_and_query);
    log_net(&format!("Jellyfin GET: {}", redact_url(&url)));
    let resp = get_shared_agent()
        .get(&url)
        .set("Authorization", &token_auth_header(&server.access_token))
        .call()
        .map_err(|e| {
            let ce = classify_ureq_error(&e);
            log_err(&format!("Jellyfin GET failed: {}", ce.detail));
            ce.user_message
        })?;
    let bytes = read_body_capped(resp.into_reader(), 8 * 1024 * 1024).map_err(|e| e.user_message)?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid Jellyfin JSON response: {}", e))
}

/// Fetches the user's Jellyfin library views (`GET /UserViews?userId=...`).
pub fn fetch_user_views(server: &JellyfinServerConfig) -> Result<Vec<UserView>, String> {
    let path = format!("/UserViews?userId={}", percent_encode(&server.user_id));
    let res: UserViewsResponse = jf_get_json(server, &path)?;
    Ok(res.items)
}

/// Fetches paginated items from Jellyfin (`GET /Items`).
#[allow(clippy::too_many_arguments)]
pub fn fetch_items(
    server: &JellyfinServerConfig,
    parent_id: Option<&str>,
    start_index: u32,
    limit: u32,
    search_term: Option<&str>,
    genre: Option<&str>,
    sort_by: Option<&str>,
    sort_order: Option<&str>,
) -> Result<ItemsResponse, String> {
    let mut path = format!(
        "/Items?userId={}&recursive=true&includeItemTypes=Movie,Series&startIndex={}&limit={}&fields=Overview,Genres,ProviderIds,CommunityRating,OfficialRating,RunTimeTicks,MediaSources,MediaStreams",
        percent_encode(&server.user_id),
        start_index,
        limit
    );
    if let Some(pid) = parent_id {
        if !pid.is_empty() {
            path.push_str(&format!("&parentId={}", percent_encode(pid)));
        }
    }
    if let Some(q) = search_term {
        let qt = q.trim();
        if !qt.is_empty() {
            path.push_str(&format!("&searchTerm={}", percent_encode(qt)));
        }
    }
    if let Some(g) = genre {
        let gt = g.trim();
        if !gt.is_empty() && !gt.eq_ignore_ascii_case("all") {
            path.push_str(&format!("&genres={}", percent_encode(gt)));
        }
    }
    let sb = sort_by.unwrap_or("SortName");
    let so = sort_order.unwrap_or("Ascending");
    path.push_str(&format!("&sortBy={}&sortOrder={}", percent_encode(sb), percent_encode(so)));

    jf_get_json(server, &path)
}

/// Fetches a single item's full details (including `MediaSources` and `MediaStreams`).
pub fn fetch_item_detail(server: &JellyfinServerConfig, item_id: &str) -> Result<JellyfinItem, String> {
    let path = format!(
        "/Items?userId={}&ids={}&fields=Overview,Genres,ProviderIds,CommunityRating,OfficialRating,RunTimeTicks,MediaSources,MediaStreams",
        percent_encode(&server.user_id),
        percent_encode(item_id)
    );
    let res: ItemsResponse = jf_get_json(server, &path)?;
    res.items
        .into_iter()
        .next()
        .ok_or_else(|| format!("Jellyfin item '{}' not found", item_id))
}

/// Fetches seasons for a series (`GET /Shows/{seriesId}/Seasons`).
pub fn fetch_seasons(server: &JellyfinServerConfig, series_id: &str) -> Result<Vec<JellyfinItem>, String> {
    let path = format!(
        "/Shows/{}/Seasons?userId={}",
        percent_encode(series_id),
        percent_encode(&server.user_id)
    );
    let res: ItemsResponse = jf_get_json(server, &path)?;
    Ok(res.items)
}

/// Fetches episodes for a series (optionally filtered by `seasonId`) via `GET /Shows/{seriesId}/Episodes`.
pub fn fetch_episodes(
    server: &JellyfinServerConfig,
    series_id: &str,
    season_id: Option<&str>,
) -> Result<Vec<JellyfinItem>, String> {
    let mut path = format!(
        "/Shows/{}/Episodes?userId={}&fields=Overview,MediaSources,MediaStreams",
        percent_encode(series_id),
        percent_encode(&server.user_id)
    );
    if let Some(sid) = season_id {
        if !sid.is_empty() {
            path.push_str(&format!("&seasonId={}", percent_encode(sid)));
        }
    }
    let res: ItemsResponse = jf_get_json(server, &path)?;
    Ok(res.items)
}

