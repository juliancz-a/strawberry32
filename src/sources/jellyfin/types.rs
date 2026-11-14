use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PublicSystemInfo {
    #[serde(rename = "ServerName", default)]
    pub server_name: String,
    #[serde(rename = "Version", default)]
    pub version: String,
    #[serde(rename = "ProductName", default)]
    pub product_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AuthUser {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name", default)]
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AuthResponse {
    #[serde(rename = "AccessToken")]
    pub access_token: String,
    #[serde(rename = "User")]
    pub user: AuthUser,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UserView {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "CollectionType", default)]
    pub collection_type: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UserViewsResponse {
    #[serde(rename = "Items", default)]
    pub items: Vec<UserView>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct MediaStream {
    #[serde(rename = "Type", default)]
    pub stream_type: String,
    #[serde(rename = "Codec", default)]
    pub codec: String,
    #[serde(rename = "Language", default)]
    pub language: Option<String>,
    #[serde(rename = "DisplayTitle", default)]
    pub display_title: Option<String>,
    #[serde(rename = "Index", default)]
    pub index: i32,
    #[serde(rename = "IsExternal", default)]
    pub is_external: bool,
    #[serde(rename = "DeliveryUrl", default)]
    pub delivery_url: Option<String>,
    #[serde(rename = "Width", default)]
    pub width: Option<u32>,
    #[serde(rename = "Height", default)]
    pub height: Option<u32>,
    #[serde(rename = "BitDepth", default)]
    pub bit_depth: Option<u32>,
    #[serde(rename = "Profile", default)]
    pub profile: Option<String>,
    #[serde(rename = "Level", default)]
    pub level: Option<f64>,
    #[serde(rename = "Channels", default)]
    pub channels: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct MediaSource {
    #[serde(rename = "Id", default)]
    pub id: String,
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "Container", default)]
    pub container: Option<String>,
    #[serde(rename = "Size", default)]
    pub size: Option<u64>,
    #[serde(rename = "Bitrate", default)]
    pub bitrate: Option<u64>,
    #[serde(rename = "MediaStreams", default)]
    pub media_streams: Vec<MediaStream>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Default)]
pub struct JellyfinItem {
    #[serde(rename = "Id")]
    pub id: String,
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Type", default)]
    pub item_type: String,
    #[serde(rename = "ProductionYear", default)]
    pub production_year: Option<i32>,
    #[serde(rename = "Overview", default)]
    pub overview: Option<String>,
    #[serde(rename = "Genres", default)]
    pub genres: Vec<String>,
    #[serde(rename = "CommunityRating", default)]
    pub community_rating: Option<f32>,
    #[serde(rename = "OfficialRating", default)]
    pub official_rating: Option<String>,
    #[serde(rename = "RunTimeTicks", default)]
    pub run_time_ticks: Option<u64>,
    #[serde(rename = "ProviderIds", default)]
    pub provider_ids: HashMap<String, String>,
    #[serde(rename = "MediaSources", default)]
    pub media_sources: Vec<MediaSource>,
    #[serde(rename = "MediaStreams", default)]
    pub media_streams: Vec<MediaStream>,
    #[serde(rename = "IndexNumber", default)]
    pub index_number: Option<u32>,
    #[serde(rename = "ParentIndexNumber", default)]
    pub parent_index_number: Option<u32>,
    #[serde(rename = "PremiereDate", default)]
    pub premiere_date: Option<String>,
    #[serde(rename = "ImageTags", default)]
    pub image_tags: HashMap<String, String>,
    #[serde(rename = "BackdropImageTags", default)]
    pub backdrop_image_tags: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ItemsResponse {
    #[serde(rename = "Items", default)]
    pub items: Vec<JellyfinItem>,
    #[serde(rename = "TotalRecordCount", default)]
    pub total_record_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuickConnectResult {
    #[serde(rename = "Secret", default)]
    pub secret: String,
    #[serde(rename = "Code", default)]
    pub code: String,
    #[serde(rename = "Authenticated", default)]
    pub authenticated: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PlaybackInfoResponse {
    #[serde(rename = "MediaSources", default)]
    pub media_sources: Vec<MediaSource>,
}

