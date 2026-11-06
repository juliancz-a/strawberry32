use crate::logger::{log_err, log_torrent};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct FileMeta {
    pub file_name: String,
    pub file_len: u64,
}

static FILE_REGISTRY: OnceLock<Mutex<HashMap<(usize, usize), FileMeta>>> = OnceLock::new();
static PROXY_STARTED: OnceLock<bool> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<(usize, usize), FileMeta>> {
    FILE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_file_info(torrent_id: usize, file_idx: usize, file_name: &str, file_len: u64) {
    let mut guard = registry().lock().unwrap();
    guard.insert(
        (torrent_id, file_idx),
        FileMeta {
            file_name: file_name.to_string(),
            file_len,
        },
    );
    log_torrent(&format!(
        "[PROXY] Registrado torrent={} idx={} ({:.1} MB, '{}')",
        torrent_id,
        file_idx,
        file_len as f64 / 1_048_576.0,
        file_name
    ));
}

pub fn start_proxy_server() -> Result<(), String> {
    if PROXY_STARTED.get().is_some() {
        return Ok(());
    }
    let listener = TcpListener::bind("127.0.0.1:3031")
        .map_err(|e| format!("No se pudo abrir proxy en 127.0.0.1:3031: {}", e))?;
    let _ = PROXY_STARTED.set(true);

    std::thread::Builder::new()
        .name("mf-remux-proxy".to_string())
        .spawn(move || {
            log_torrent("[PROXY] Servidor HTTP/fMP4 para Media Foundation activo en http://127.0.0.1:3031");
            for stream in listener.incoming().flatten() {
                // Cada conexión de Media Foundation DEBE correr en su propio hilo
                // porque MF abre una 2da conexión paralela para buscar el átomo 'moov' al final
                // sin cerrar la 1ra conexión.
                std::thread::spawn(move || {
                    let _ = handle_client(stream);
                });
            }
        })
        .map_err(|e| format!("Error creando hilo proxy: {}", e))?;

    Ok(())
}

fn parse_path(path: &str) -> Option<(usize, usize)> {
    // Formato esperado: /play/{torrent_id}/{file_idx}/movie.mp4
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 3 && parts[0] == "play" {
        let tid = parts[1].parse::<usize>().ok()?;
        let fidx = parts[2].parse::<usize>().ok()?;
        return Some((tid, fidx));
    }
    None
}

fn handle_client(mut client: TcpStream) -> Result<(), String> {
    let _ = client.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = client.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = client.set_nodelay(true);

    let mut req_buf = [0u8; 4096];
    let n = match client.read(&mut req_buf) {
        Ok(0) | Err(_) => return Ok(()),
        Ok(n) => n,
    };

    let req_str = String::from_utf8_lossy(&req_buf[..n]);
    let mut lines = req_str.lines();
    let first_line = lines.next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    let (torrent_id, file_idx) = match parse_path(path) {
        Some(ids) => ids,
        None => {
            let _ = client.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            return Ok(());
        }
    };

    let mut range_header: Option<String> = None;
    for line in lines {
        if line.len() > 6 && line[..6].eq_ignore_ascii_case("range:") {
            range_header = Some(line[6..].trim().to_string());
        }
    }

    let meta = {
        let guard = registry().lock().unwrap();
        guard.get(&(torrent_id, file_idx)).cloned()
    };

    let (file_name, file_len) = match meta {
        Some(m) if m.file_len > 0 => (m.file_name, m.file_len),
        _ => (String::from("movie.mp4"), 0),
    };

    let is_mkv = file_name.to_lowercase().ends_with(".mkv")
        || file_name.to_lowercase().ends_with(".webm");

    let upstream_url = format!(
        "http://127.0.0.1:3030/torrents/{}/stream/{}",
        torrent_id, file_idx
    );

    if is_mkv {
        log_torrent(&format!(
            "[PROXY] Detectado contenedor MKV ('{}') -> Iniciando Remux en vivo MKV a fMP4 para Media Foundation",
            file_name
        ));
        return serve_mkv_as_fmp4(&mut client, method, &upstream_url);
    }

    // --- FLUJO NATIVO MP4 (Sin truncamiento de 4MB que rompía Media Foundation) ---
    let (start, end, is_partial) = if let Some(ref r) = range_header {
        parse_http_range(r, file_len)
    } else {
        (0, file_len.saturating_sub(1), false)
    };

    let content_len = if file_len > 0 && end >= start {
        end - start + 1
    } else {
        0
    };

    log_torrent(&format!(
        "[PROXY] MP4 {} {} | Range: {:?} -> bytes {}-{}/{}",
        method, path, range_header, start, end, file_len
    ));

    if method.eq_ignore_ascii_case("HEAD") {
        let resp_hdr = if is_partial {
            format!(
                "HTTP/1.1 206 Partial Content\r\n\
                 Content-Type: video/mp4\r\n\
                 Accept-Ranges: bytes\r\n\
                 Content-Range: bytes {}-{}/{}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                start, end, file_len, content_len
            )
        } else {
            format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: video/mp4\r\n\
                 Accept-Ranges: bytes\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                file_len
            )
        };
        let _ = client.write_all(resp_hdr.as_bytes());
        let _ = client.flush();
        return Ok(());
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .build();

    let mut req = agent.get(&upstream_url);
    if file_len > 0 {
        req = req.set("Range", &format!("bytes={}-{}", start, end));
    } else if let Some(ref r) = range_header {
        req = req.set("Range", r);
    }

    let upstream_resp = match req.call() {
        Ok(r) => r,
        Err(e) => {
            log_err(&format!("[PROXY] Error conectando a librqbit: {}", e));
            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
            return Err(e.to_string());
        }
    };

    // Enviamos el header exacto que exige el MPEG4ByteStreamHandler de Windows RT 8.1
    let resp_hdr = if is_partial && file_len > 0 {
        format!(
            "HTTP/1.1 206 Partial Content\r\n\
             Content-Type: video/mp4\r\n\
             Accept-Ranges: bytes\r\n\
             Content-Range: bytes {}-{}/{}\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            start, end, file_len, content_len
        )
    } else {
        format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: video/mp4\r\n\
             Accept-Ranges: bytes\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            content_len
        )
    };

    if client.write_all(resp_hdr.as_bytes()).is_err() {
        return Ok(());
    }
    let _ = client.flush();

    let mut reader = upstream_resp.into_reader();
    let mut buf = [0u8; 32768];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read_n) => {
                // Si Media Foundation cierra el socket inicial tras leer los primeros 64KB
                // para saltar al átomo 'moov' del final, salimos limpiamente sin error.
                if client.write_all(&buf[..read_n]).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    Ok(())
}

fn parse_http_range(range_hdr: &str, total_len: u64) -> (u64, u64, bool) {
    if total_len == 0 {
        return (0, 0, false);
    }
    let s = range_hdr.trim();
    if !s.starts_with("bytes=") {
        return (0, total_len - 1, false);
    }
    let spec = &s[6..];
    let mut split = spec.splitn(2, '-');
    let start_str = split.next().unwrap_or("").trim();
    let end_str = split.next().unwrap_or("").trim();

    if start_str.is_empty() {
        // Rango sufijo: bytes=-524288
        if let Ok(suffix) = end_str.parse::<u64>() {
            let start = total_len.saturating_sub(suffix);
            return (start, total_len - 1, true);
        }
        return (0, total_len - 1, false);
    }

    let start = start_str.parse::<u64>().unwrap_or(0).min(total_len - 1);
    let end = if end_str.is_empty() {
        total_len - 1
    } else {
        end_str
            .parse::<u64>()
            .unwrap_or(total_len - 1)
            .min(total_len - 1)
    };

    (start, end.max(start), true)
}

// =================================================================================================
// REMUXER MATROSKA (.MKV) -> FRAGMENTED MP4 (fMP4) EN VIVO PARA WINDOWS RT MEDIA FOUNDATION
// =================================================================================================

struct MkvTrackInfo {
    video_track_num: u64,
    width: u16,
    height: u16,
    avcc_box_payload: Vec<u8>,
}

fn serve_mkv_as_fmp4(
    client: &mut TcpStream,
    method: &str,
    upstream_url: &str,
) -> Result<(), String> {
    let hdr = "HTTP/1.1 200 OK\r\n\
               Content-Type: video/mp4\r\n\
               Accept-Ranges: none\r\n\
               Connection: close\r\n\r\n";
    if client.write_all(hdr.as_bytes()).is_err() || method.eq_ignore_ascii_case("HEAD") {
        return Ok(());
    }
    let _ = client.flush();

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .build();

    let resp = agent
        .get(upstream_url)
        .call()
        .map_err(|e| format!("Error abriendo MKV en librqbit: {}", e))?;

    let mut reader = resp.into_reader();
    let mut mkv_info = MkvTrackInfo {
        video_track_num: 1,
        width: 1280,
        height: 720,
        avcc_box_payload: Vec::new(),
    };

    let mut init_sent = false;
    let mut seq_number: u32 = 1;
    let mut current_cluster_tc: u64 = 0;

    // Parseamos el flujo EBML de Matroska en vivo
    while let Some((elem_id, elem_size)) = read_ebml_element_header(&mut reader) {
        match elem_id {
            // Contenedores Master de Matroska: entramos adentro sin saltar su contenido
            0x18538067 // Segment
            | 0x1654AE6B // Tracks
            | 0xAE       // TrackEntry
            | 0xE0       // Video
            | 0x1F43B675 // Cluster
            => continue,

            // TrackNumber (0xD7)
            0xD7 => {
                let _ = read_uint(&mut reader, elem_size as usize);
            }
            // PixelWidth (0xB0)
            0xB0 => {
                if let Some(w) = read_uint(&mut reader, elem_size as usize) {
                    if w > 0 && w <= 4096 {
                        mkv_info.width = w as u16;
                    }
                }
            }
            // PixelHeight (0xBA)
            0xBA => {
                if let Some(h) = read_uint(&mut reader, elem_size as usize) {
                    if h > 0 && h <= 4096 {
                        mkv_info.height = h as u16;
                    }
                }
            }
            // CodecPrivate (0x63A2): Contiene la configuración H.264 avcC (SPS/PPS)
            0x63A2 => {
                let mut data = vec![0u8; elem_size as usize];
                if reader.read_exact(&mut data).is_ok() {
                    // El avcC de H.264 empieza con configurationVersion == 0x01
                    if data.len() >= 7 && data[0] == 1 {
                        mkv_info.avcc_box_payload = data;
                        log_torrent(&format!(
                            "[REMUX] Extraída cabecera H.264 avcC del MKV ({} bytes, {}x{})",
                            mkv_info.avcc_box_payload.len(),
                            mkv_info.width,
                            mkv_info.height
                        ));
                    }
                }
            }
            // Timecode del Cluster (0xE7)
            0xE7 => {
                if let Some(tc) = read_uint(&mut reader, elem_size as usize) {
                    current_cluster_tc = tc;
                }
            }
            // SimpleBlock (0xA3) o Block (0xA1): Contiene los cuadros H.264
            0xA3 | 0xA1 => {
                if !init_sent {
                    if mkv_info.avcc_box_payload.is_empty() {
                        log_err("[REMUX] El MKV no contiene video H.264 (avcC) compatible.");
                        return Err("MKV sin pista H.264 avcC".to_string());
                    }
                    let init_seg = build_fmp4_init_segment(&mkv_info);
                    if client.write_all(&init_seg).is_err() {
                        return Ok(());
                    }
                    let _ = client.flush();
                    init_sent = true;
                    log_torrent("[REMUX] Segmento inicial fMP4 (ftyp + moov) enviado a Media Foundation.");
                }

                if elem_size < 4 || elem_size > 16 * 1024 * 1024 {
                    skip_bytes(&mut reader, elem_size);
                    continue;
                }

                let mut block = vec![0u8; elem_size as usize];
                if reader.read_exact(&mut block).is_err() {
                    break;
                }

                let (track_num, vlen) = parse_ebml_vint(&block);
                if vlen == 0 || block.len() < vlen + 3 {
                    continue;
                }

                // Solo empaquetamos la pista de video H.264 (generalmente pista 1)
                if track_num == mkv_info.video_track_num {
                    let rel_tc = i16::from_be_bytes([block[vlen], block[vlen + 1]]);
                    let flags = block[vlen + 2];
                    let is_keyframe = (flags & 0x80) != 0;
                    let lacing = (flags >> 1) & 0x03;

                    // Si no usa lacing complejo, enviamos el frame H.264 como fragmento moof+mdat
                    if lacing == 0 {
                        let nal_payload = &block[vlen + 3..];
                        let dts_ms = (current_cluster_tc as i64 + rel_tc as i64).max(0) as u64;
                        let frag = build_fmp4_video_fragment(
                            seq_number,
                            dts_ms * 90, // Escala de tiempo 90kHz
                            3000,        // ~33.3ms por cuadro (29.97/30 fps)
                            is_keyframe,
                            nal_payload,
                        );
                        seq_number = seq_number.wrapping_add(1);
                        if client.write_all(&frag).is_err() {
                            break;
                        }
                    }
                }
            }
            _ => {
                skip_bytes(&mut reader, elem_size);
            }
        }
    }

    Ok(())
}

fn read_ebml_element_header<R: Read>(r: &mut R) -> Option<(u32, u64)> {
    let id = read_vint_id(r)?;
    let size = read_vint_size(r)?;
    Some((id, size))
}

fn read_vint_id<R: Read>(r: &mut R) -> Option<u32> {
    let mut b0 = [0u8; 1];
    r.read_exact(&mut b0).ok()?;
    let first = b0[0];
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return None;
    }
    let mut val = first as u32;
    for _ in 1..len {
        let mut bn = [0u8; 1];
        r.read_exact(&mut bn).ok()?;
        val = (val << 8) | (bn[0] as u32);
    }
    Some(val)
}

fn read_vint_size<R: Read>(r: &mut R) -> Option<u64> {
    let mut b0 = [0u8; 1];
    r.read_exact(&mut b0).ok()?;
    let first = b0[0];
    if first == 0 {
        return None;
    }
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let mask = (1u8 << (8 - len)) - 1;
    let mut val = (first & mask) as u64;
    for _ in 1..len {
        let mut bn = [0u8; 1];
        r.read_exact(&mut bn).ok()?;
        val = (val << 8) | (bn[0] as u64);
    }
    Some(val)
}

fn parse_ebml_vint(buf: &[u8]) -> (u64, usize) {
    if buf.is_empty() || buf[0] == 0 {
        return (0, 0);
    }
    let len = buf[0].leading_zeros() as usize + 1;
    if len > 8 || buf.len() < len {
        return (0, 0);
    }
    let mask = (1u8 << (8 - len)) - 1;
    let mut val = (buf[0] & mask) as u64;
    for i in 1..len {
        val = (val << 8) | (buf[i] as u64);
    }
    (val, len)
}

fn read_uint<R: Read>(r: &mut R, n: usize) -> Option<u64> {
    if n == 0 || n > 8 {
        skip_bytes(r, n as u64);
        return None;
    }
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf[..n]).ok()?;
    let mut val = 0u64;
    for i in 0..n {
        val = (val << 8) | (buf[i] as u64);
    }
    Some(val)
}

fn skip_bytes<R: Read>(r: &mut R, mut count: u64) {
    let mut scratch = [0u8; 16384];
    while count > 0 {
        let to_read = (count as usize).min(scratch.len());
        match r.read(&mut scratch[..to_read]) {
            Ok(0) | Err(_) => break,
            Ok(n) => count -= n as u64,
        }
    }
}

fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let total = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    out
}

fn build_fmp4_init_segment(info: &MkvTrackInfo) -> Vec<u8> {
    // 1. ftyp box
    let ftyp = mp4_box(b"ftyp", b"isom\x00\x00\x02\x00isomiso6dashmp41");

    // 2. mvhd box (timescale = 90000)
    let mut mvhd_p = vec![0u8; 100];
    mvhd_p[12..16].copy_from_slice(&90000u32.to_be_bytes());
    mvhd_p[20..24].copy_from_slice(&0x00010000u32.to_be_bytes()); // rate 1.0
    mvhd_p[24..26].copy_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
    mvhd_p[36..40].copy_from_slice(&0x00010000u32.to_be_bytes());
    mvhd_p[52..56].copy_from_slice(&0x00010000u32.to_be_bytes());
    mvhd_p[68..72].copy_from_slice(&0x40000000u32.to_be_bytes());
    mvhd_p[96..100].copy_from_slice(&2u32.to_be_bytes()); // next_track_ID
    let mvhd = mp4_box(b"mvhd", &mvhd_p);

    // 3. tkhd box (Track 1)
    let mut tkhd_p = vec![0u8; 84];
    tkhd_p[3] = 0x03; // Track enabled + in movie
    tkhd_p[12..16].copy_from_slice(&1u32.to_be_bytes()); // track_ID = 1
    tkhd_p[40..44].copy_from_slice(&0x00010000u32.to_be_bytes());
    tkhd_p[56..60].copy_from_slice(&0x00010000u32.to_be_bytes());
    tkhd_p[72..76].copy_from_slice(&0x40000000u32.to_be_bytes());
    tkhd_p[76..80].copy_from_slice(&((info.width as u32) << 16).to_be_bytes());
    tkhd_p[80..84].copy_from_slice(&((info.height as u32) << 16).to_be_bytes());
    let tkhd = mp4_box(b"tkhd", &tkhd_p);

    // 4. mdia -> mdhd + hdlr + minf
    let mut mdhd_p = vec![0u8; 24];
    mdhd_p[12..16].copy_from_slice(&90000u32.to_be_bytes());
    mdhd_p[20..22].copy_from_slice(&0x55C4u16.to_be_bytes()); // 'und'
    let mdhd = mp4_box(b"mdhd", &mdhd_p);

    let mut hdlr_p = vec![0u8; 25];
    hdlr_p[8..12].copy_from_slice(b"vide");
    hdlr_p[24] = 0;
    let hdlr = mp4_box(b"hdlr", &hdlr_p);

    let vmhd = mp4_box(b"vmhd", &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    let url_box = mp4_box(b"url ", &[0, 0, 0, 1]);
    let mut dref_p = vec![0, 0, 0, 0, 0, 0, 0, 1];
    dref_p.extend_from_slice(&url_box);
    let dinf = mp4_box(b"dinf", &mp4_box(b"dref", &dref_p));

    // avc1 + avcC sample entry
    let avcc = mp4_box(b"avcC", &info.avcc_box_payload);
    let mut avc1_p = vec![0u8; 78];
    avc1_p[6..8].copy_from_slice(&1u16.to_be_bytes()); // data_reference_index = 1
    avc1_p[24..26].copy_from_slice(&info.width.to_be_bytes());
    avc1_p[26..28].copy_from_slice(&info.height.to_be_bytes());
    avc1_p[28..32].copy_from_slice(&0x00480000u32.to_be_bytes()); // 72 dpi
    avc1_p[32..36].copy_from_slice(&0x00480000u32.to_be_bytes());
    avc1_p[40..42].copy_from_slice(&1u16.to_be_bytes()); // frame_count = 1
    avc1_p[74..76].copy_from_slice(&0x0018u16.to_be_bytes()); // depth = 24
    avc1_p[76..78].copy_from_slice(&0xFFFFu16.to_be_bytes());
    avc1_p.extend_from_slice(&avcc);
    let avc1 = mp4_box(b"avc1", &avc1_p);

    let mut stsd_p = vec![0, 0, 0, 0, 0, 0, 0, 1];
    stsd_p.extend_from_slice(&avc1);
    let stsd = mp4_box(b"stsd", &stsd_p);
    let empty_tbl = [0u8; 8];
    let stts = mp4_box(b"stts", &empty_tbl);
    let stsc = mp4_box(b"stsc", &empty_tbl);
    let stsz = mp4_box(b"stsz", &[0u8; 12]);
    let stco = mp4_box(b"stco", &empty_tbl);

    let mut stbl_p = Vec::new();
    stbl_p.extend_from_slice(&stsd);
    stbl_p.extend_from_slice(&stts);
    stbl_p.extend_from_slice(&stsc);
    stbl_p.extend_from_slice(&stsz);
    stbl_p.extend_from_slice(&stco);
    let stbl = mp4_box(b"stbl", &stbl_p);

    let mut minf_p = Vec::new();
    minf_p.extend_from_slice(&vmhd);
    minf_p.extend_from_slice(&dinf);
    minf_p.extend_from_slice(&stbl);
    let minf = mp4_box(b"minf", &minf_p);

    let mut mdia_p = Vec::new();
    mdia_p.extend_from_slice(&mdhd);
    mdia_p.extend_from_slice(&hdlr);
    mdia_p.extend_from_slice(&minf);
    let mdia = mp4_box(b"mdia", &mdia_p);

    let mut trak_p = Vec::new();
    trak_p.extend_from_slice(&tkhd);
    trak_p.extend_from_slice(&mdia);
    let trak = mp4_box(b"trak", &trak_p);

    // 5. mvex -> trex (Habilita Fragmented MP4 en Media Foundation)
    let mut trex_p = vec![0u8; 24];
    trex_p[4..8].copy_from_slice(&1u32.to_be_bytes()); // track_ID = 1
    trex_p[8..12].copy_from_slice(&1u32.to_be_bytes()); // default_sample_description_index = 1
    let mvex = mp4_box(b"mvex", &mp4_box(b"trex", &trex_p));

    let mut moov_p = Vec::new();
    moov_p.extend_from_slice(&mvhd);
    moov_p.extend_from_slice(&trak);
    moov_p.extend_from_slice(&mvex);
    let moov = mp4_box(b"moov", &moov_p);

    let mut out = ftyp;
    out.extend_from_slice(&moov);
    out
}

fn build_fmp4_video_fragment(
    seq: u32,
    base_dts: u64,
    duration: u32,
    is_keyframe: bool,
    nal_data: &[u8],
) -> Vec<u8> {
    // mfhd
    let mut mfhd_p = vec![0u8; 8];
    mfhd_p[4..8].copy_from_slice(&seq.to_be_bytes());
    let mfhd = mp4_box(b"mfhd", &mfhd_p);

    // tfhd (0x020000 = default-base-is-moof)
    let mut tfhd_p = vec![0, 0x02, 0x00, 0x00];
    tfhd_p.extend_from_slice(&1u32.to_be_bytes()); // track_ID = 1
    let tfhd = mp4_box(b"tfhd", &tfhd_p);

    // tfdt (version 1, 64-bit baseMediaDecodeTime)
    let mut tfdt_p = vec![1, 0, 0, 0];
    tfdt_p.extend_from_slice(&base_dts.to_be_bytes());
    let tfdt = mp4_box(b"tfdt", &tfdt_p);

    // trun (flags 0x000701: data-offset + duration + size + flags)
    // El tamaño total de moof será: 8 (moof) + 16 (mfhd) + 8 (traf) + 16 (tfhd) + 20 (tfdt) + 32 (trun) = 100 bytes
    // Más 8 bytes de cabecera de mdat = data_offset = 108 bytes.
    let data_offset: i32 = 108;
    let sample_flags: u32 = if is_keyframe { 0x02000000 } else { 0x01010000 };

    let mut trun_p = vec![0, 0x00, 0x07, 0x01];
    trun_p.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
    trun_p.extend_from_slice(&data_offset.to_be_bytes());
    trun_p.extend_from_slice(&duration.to_be_bytes());
    trun_p.extend_from_slice(&(nal_data.len() as u32).to_be_bytes());
    trun_p.extend_from_slice(&sample_flags.to_be_bytes());
    let trun = mp4_box(b"trun", &trun_p);

    let mut traf_p = Vec::new();
    traf_p.extend_from_slice(&tfhd);
    traf_p.extend_from_slice(&tfdt);
    traf_p.extend_from_slice(&trun);
    let traf = mp4_box(b"traf", &traf_p);

    let mut moof_p = Vec::new();
    moof_p.extend_from_slice(&mfhd);
    moof_p.extend_from_slice(&traf);
    let moof = mp4_box(b"moof", &moof_p);

    let mdat = mp4_box(b"mdat", nal_data);

    let mut out = moof;
    out.extend_from_slice(&mdat);
    out
}