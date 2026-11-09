use crate::logger::log_info;
use crate::state::get_app_storage_root;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};

/// EBML Element IDs for Matroska
mod ebml_ids {
    pub const CUES: u32 = 0x1C53BB6B;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerMetadata {
    pub container_type: String, // "mp4", "mkv", "unknown"
    pub moov_range: Option<(u64, u64)>, // (offset, size)
    pub cues_offset: Option<u64>,
}

static META_CACHE: OnceLock<Mutex<HashMap<String, ContainerMetadata>>> = OnceLock::new();

fn get_cache_map() -> &'static Mutex<HashMap<String, ContainerMetadata>> {
    META_CACHE.get_or_init(|| {
        let path = get_meta_cache_path();
        if path.is_file() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(map) = serde_json::from_str::<HashMap<String, ContainerMetadata>>(&data) {
                    return Mutex::new(map);
                }
            }
        }
        Mutex::new(HashMap::new())
    })
}

fn get_meta_cache_path() -> PathBuf {
    get_app_storage_root().join("container_meta.json")
}

fn save_cache_to_disk() {
    let map = get_cache_map().lock().unwrap();
    if let Ok(json) = serde_json::to_string_pretty(&*map) {
        let _ = fs::write(get_meta_cache_path(), json);
    }
}

pub struct MetadataInspector;

impl MetadataInspector {
    /// Checks cache first; if not found, inspects stream and persists result to disk.
    pub async fn get_or_inspect<R>(
        info_hash: &str,
        file_idx: usize,
        filename: &str,
        total_size: u64,
        reader: &mut R,
    ) -> ContainerMetadata
    where
        R: AsyncRead + AsyncSeek + Unpin,
    {
        let key = format!("{}_{}", info_hash.to_lowercase(), file_idx);
        {
            let map = get_cache_map().lock().unwrap();
            if let Some(meta) = map.get(&key) {
                return meta.clone();
            }
        }

        let meta = Self::inspect(filename, total_size, reader).await;
        {
            let mut map = get_cache_map().lock().unwrap();
            map.insert(key, meta.clone());
        }
        save_cache_to_disk();
        meta
    }

    pub async fn inspect<R>(filename: &str, total_size: u64, reader: &mut R) -> ContainerMetadata
    where
        R: AsyncRead + AsyncSeek + Unpin,
    {
        let lower = filename.to_lowercase();
        if lower.ends_with(".mp4") || lower.ends_with(".m4v") || lower.ends_with(".mov") {
            let moov_range = Self::inspect_mp4(reader, total_size).await.ok().and_then(|r| r.first().copied());
            ContainerMetadata {
                container_type: "mp4".to_string(),
                moov_range,
                cues_offset: None,
            }
        } else if lower.ends_with(".mkv") || lower.ends_with(".webm") {
            let cues_offset = Self::find_mkv_cues_offset(reader, total_size).await.ok().flatten();
            ContainerMetadata {
                container_type: "mkv".to_string(),
                moov_range: None,
                cues_offset,
            }
        } else {
            ContainerMetadata {
                container_type: "unknown".to_string(),
                moov_range: None,
                cues_offset: None,
            }
        }
    }

    // =========================================================================
    // MP4 Top-Level Atom Parser
    // =========================================================================
    async fn inspect_mp4<R>(reader: &mut R, total_size: u64) -> anyhow::Result<Vec<(u64, u64)>>
    where
        R: AsyncRead + AsyncSeek + Unpin,
    {
        let mut ranges = Vec::new();
        let mut pos = 0u64;
        let mut buffer = [0u8; 16];
        let mut iterations = 0;

        while pos + 8 <= total_size && iterations < 80 {
            iterations += 1;
            reader.seek(SeekFrom::Start(pos)).await?;

            if reader.read(&mut buffer[..8]).await? < 8 {
                break;
            }

            let box_size = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as u64;
            let box_type = [buffer[4], buffer[5], buffer[6], buffer[7]];

            let actual_size = if box_size == 1 {
                if reader.read(&mut buffer[8..16]).await? < 8 {
                    break;
                }
                u64::from_be_bytes([
                    buffer[8], buffer[9], buffer[10], buffer[11], buffer[12], buffer[13],
                    buffer[14], buffer[15],
                ])
            } else if box_size == 0 {
                total_size.saturating_sub(pos)
            } else {
                box_size
            };

            if &box_type == b"moov" {
                log_info(&format!("MetadataInspector: Found fast-start 'moov' at {} (size: {} KB)", pos, actual_size / 1024));
                ranges.push((pos, actual_size.min(30_000_000)));
                return Ok(ranges);
            }

            if &box_type == b"mdat" && actual_size > 5_000_000 {
                // Large media data precedes moov: search tail of file for moov
                if let Ok(end_ranges) = Self::check_mp4_end(reader, total_size).await {
                    if !end_ranges.is_empty() {
                        return Ok(end_ranges);
                    }
                }
            }

            if actual_size < 8 || actual_size > total_size.saturating_sub(pos) {
                break;
            }
            pos += actual_size;
        }

        // Fallback: check last 3 MB of the file
        if ranges.is_empty() && total_size > 3_000_000 {
            ranges.push((total_size - 3_000_000, 3_000_000));
        }

        Ok(ranges)
    }

    async fn check_mp4_end<R>(reader: &mut R, total_size: u64) -> anyhow::Result<Vec<(u64, u64)>>
    where
        R: AsyncRead + AsyncSeek + Unpin,
    {
        let check_size = 5_000_000.min(total_size);
        let start_pos = total_size - check_size;
        reader.seek(SeekFrom::Start(start_pos)).await?;

        let mut buffer = vec![0u8; check_size as usize];
        let bytes_read = reader.read(&mut buffer).await?;
        buffer.truncate(bytes_read);

        for i in 0..(buffer.len().saturating_sub(8)) {
            if &buffer[i + 4..i + 8] == b"moov" {
                let box_size =
                    u32::from_be_bytes([buffer[i], buffer[i + 1], buffer[i + 2], buffer[i + 3]]) as u64;
                let global_pos = start_pos + i as u64;
                log_info(&format!(
                    "MetadataInspector: Found tail 'moov' at byte {} (size: {} KB)",
                    global_pos,
                    box_size / 1024
                ));
                return Ok(vec![(global_pos, box_size.min(30_000_000))]);
            }
        }

        Ok(vec![])
    }

    // =========================================================================
    // MKV EBML & SeekHead Cues Locator
    // =========================================================================
    async fn find_mkv_cues_offset<R>(reader: &mut R, total_size: u64) -> anyhow::Result<Option<u64>>
    where
        R: AsyncRead + AsyncSeek + Unpin,
    {
        reader.seek(SeekFrom::Start(0)).await?;

        let read_size = 102_400.min(total_size) as usize;
        let mut buffer = vec![0u8; read_size];
        let n = reader.read(&mut buffer).await?;
        buffer.truncate(n);

        let mut pos = 0;

        // Skip EBML header (0x1A45DFA3)
        while pos + 4 < buffer.len() {
            if buffer[pos] == 0x1A && buffer[pos + 1] == 0x45 && buffer[pos + 2] == 0xDF && buffer[pos + 3] == 0xA3 {
                if let Some((_, size)) = Self::parse_ebml_element_header(&buffer[pos..]) {
                    pos += size;
                    break;
                }
            }
            pos += 1;
        }

        // Locate Segment (0x18538067)
        let mut segment_data_start = 0u64;
        while pos + 4 < buffer.len() {
            if buffer[pos] == 0x18 && buffer[pos + 1] == 0x53 && buffer[pos + 2] == 0x80 && buffer[pos + 3] == 0x67 {
                if let Some((_, header_size)) = Self::parse_ebml_element_header(&buffer[pos..]) {
                    segment_data_start = (pos + header_size) as u64;
                    pos += header_size;
                    break;
                }
            }
            pos += 1;
        }

        // Locate SeekHead (0x114D9B74)
        while pos + 4 < buffer.len() {
            if buffer[pos] == 0x11 && buffer[pos + 1] == 0x4D && buffer[pos + 2] == 0x9B && buffer[pos + 3] == 0x74 {
                if let Some((size, header_size)) = Self::parse_ebml_element_header(&buffer[pos..]) {
                    let seek_head_data = &buffer[pos + header_size..];
                    let seek_head_size = (size as usize).min(seek_head_data.len());
                    if let Some(cues_pos) = Self::parse_seek_head(&seek_head_data[..seek_head_size]) {
                        let absolute_cues = segment_data_start + cues_pos;
                        log_info(&format!("MetadataInspector: Located MKV Cues at byte {}", absolute_cues));
                        return Ok(Some(absolute_cues));
                    }
                }
            }

            // Stop if we hit clusters (0x1F43B675)
            if buffer[pos] == 0x1F && buffer[pos + 1] == 0x43 && buffer[pos + 2] == 0xB6 && buffer[pos + 3] == 0x75 {
                break;
            }

            pos += 1;
        }

        Ok(None)
    }

    fn parse_ebml_element_header(data: &[u8]) -> Option<(u64, usize)> {
        if data.is_empty() {
            return None;
        }
        let (_, id_len) = Self::parse_vint(data)?;
        if data.len() < id_len {
            return None;
        }
        let (size, size_len) = Self::parse_vint(&data[id_len..])?;
        Some((size, id_len + size_len))
    }

    fn parse_vint(data: &[u8]) -> Option<(u64, usize)> {
        if data.is_empty() {
            return None;
        }
        let first = data[0];
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || data.len() < len {
            return None;
        }

        let mut value = (first as u64) & ((1 << (8 - len)) - 1);
        for &byte in data.iter().take(len).skip(1) {
            value = (value << 8) | u64::from(byte);
        }
        Some((value, len))
    }

    fn parse_seek_head(data: &[u8]) -> Option<u64> {
        let mut pos = 0;
        while pos + 2 < data.len() {
            if data[pos] == 0x4D && data[pos + 1] == 0xBB {
                if let Some((size, header_size)) = Self::parse_ebml_element_header(&data[pos..]) {
                    let seek_data = &data[pos + header_size..];
                    let seek_size = (size as usize).min(seek_data.len());
                    if let Some(cues_pos) = Self::parse_seek_element(&seek_data[..seek_size]) {
                        return Some(cues_pos);
                    }
                    pos += header_size + size as usize;
                    continue;
                }
            }
            pos += 1;
        }
        None
    }

    fn parse_seek_element(data: &[u8]) -> Option<u64> {
        let mut pos = 0;
        let mut seek_id: Option<u32> = None;
        let mut seek_position: Option<u64> = None;

        while pos + 2 < data.len() {
            // SeekID (0x53AB)
            if data[pos] == 0x53 && data[pos + 1] == 0xAB {
                if let Some((size, header_size)) = Self::parse_ebml_element_header(&data[pos..]) {
                    let id_data = &data[pos + header_size..];
                    if size as usize <= id_data.len() {
                        let mut id = 0u32;
                        for &byte in id_data.iter().take(size as usize) {
                            id = (id << 8) | u32::from(byte);
                        }
                        seek_id = Some(id);
                    }
                    pos += header_size + size as usize;
                    continue;
                }
            }

            // SeekPosition (0x53AC)
            if data[pos] == 0x53 && data[pos + 1] == 0xAC {
                if let Some((size, header_size)) = Self::parse_ebml_element_header(&data[pos..]) {
                    let pos_data = &data[pos + header_size..];
                    if size as usize <= pos_data.len() {
                        let mut position = 0u64;
                        for &byte in pos_data.iter().take(size as usize) {
                            position = (position << 8) | u64::from(byte);
                        }
                        seek_position = Some(position);
                    }
                    pos += header_size + size as usize;
                    continue;
                }
            }

            pos += 1;
        }

        if let (Some(id), Some(position)) = (seek_id, seek_position) {
            if id == ebml_ids::CUES {
                return Some(position);
            }
        }

        None
    }
}
