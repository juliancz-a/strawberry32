# RT-Stream ARM32 — Software Design Document (SDD) & Roadmap
**Target Platform:** Microsoft Surface RT (Windows RT 8.1, Kernel NT 6.3, ARMv7 Thumb-2 `thumbv7a-pc-windows-msvc`)
**Hardware Profile:** Nvidia Tegra 3 (Quad-core Cortex-A9), 2 GB RAM, Direct3D Feature Level 9_1, Hardware H.264 DXVA2 Decoder.
**Goal:** 100% standalone, single-binary (`.exe`) native streaming application with embedded hardware-accelerated video playback (MP4 + MKV via live fMP4 remuxing) and zero external dependencies (no VLC installation required).

---

## 1. Platform & Toolchain Invariants (Strict Rules)
1. **No `ring` or `rustls`:** `ring` assembly fails on PE/COFF Windows ARM32. All HTTPS traffic (`ureq` and `librqbit`) MUST use Windows RT native Schannel (`native-tls` / `default-tls`).
2. **Patched `windows-sys`:** Maintain `[patch.crates-io]` pointing to `vendor/windows-sys` (0.60.2) and `vendor/windows-sys-0.61` (0.61.2) with `target_arch = "arm"` enabled on 32-bit structs (`WSADATA`, `MEMORY_BASIC_INFORMATION`, `SHCREATEPROCESSINFOW`, etc.).
3. **Linker & Runtime Flags:** Build with `--cfg windows_raw_dylib`, `-C target-feature=+crt-static`, and `-C link-arg=/SUBSYSTEM:CONSOLE,6.03`.
4. **`librqbit` Session Safety:** Never set `defer_writes_up_to` in `librqbit::SessionOptions` (causes zero-capacity `mpsc` panic on 32-bit calculations).
5. **Writable Paths:** Windows RT blocks root `C:\` writes (`ERROR_ACCESS_DENIED`). Resolve `%TEMP%\rt_stream_*` dynamically via `GetTempPathW` for all caches, logs, and JSON storage.

---

## 2. System Architecture (Zero External Dependencies)

```text
┌─────────────────────────────────────────────────────────────────────────┐
│                       MAIN PROCESS (rt-stream.exe)                      │
│                                                                         │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │ UI Layer: Slint (renderer-software) OR Custom Dark-Mode Win32 UI  │  │
│  │  • Home ("Continue Watching" + Popular)   • Search & Discover     │  │
│  │  • Detail View (Season/Episode selector)  • Live Debug Console    │  │
│  │  • Embedded Video Child HWND Viewport     • [⚡ DEV Quick Load]   │  │
│  └───────────────┬───────────────────────────────────────▲───────────┘  │
│                  │ Commands / Actions                    │ Events/State │
│                  ▼                                       │              │
│  ┌───────────────────────────────────────────────────────┴───────────┐  │
│  │ Core State & Stremio Protocol Engine (`src/stremio_proto/`)       │  │
│  │  • Native Manifest, Catalog, Meta, Stream & Subtitle parsers      │  │
│  │  • Local JSON Persistence (`addons.json`, `watch_history.json`)   │  │
│  │  • Tegra 3 Stream Scorer (720p/480p H.264 + AAC/MP3 prioritized)  │  │
│  └───────────────┬───────────────────────────────────────────────────┘  │
│                  │ Selected InfoHash + Trackers + FileIdx               │
│                  ▼                                                      │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │ Torrent Streaming Engine (`librqbit` on `127.0.0.1:3030`)         │  │
│  │  • Injects Torrentio `sources` + 15 HTTP/UDP public trackers      │  │
│  │  • Concurrent Header (1 MB) + Tail `moov` (512 KB) pre-fetcher    │  │
│  └───────────────┬───────────────────────────────────────────────────┘  │
│                  │ Raw Byte Stream (MP4 or MKV via HTTP Range)          │
│                  ▼                                                      │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │ Local Stream Proxy & Live MKV->fMP4 Remuxer (`127.0.0.1:3031`)    │  │
│  │  • Route: `[http://127.0.0.1:3031/stream/](http://127.0.0.1:3031/stream/){id}/{idx}/video.mp4`     │  │
│  │  • If source is `.mp4`: Direct HTTP Range pass-through with       │  │
│  │    `Content-Type: video/mp4` (Fixes Media Foundation 0xC00D36C4)  │  │
│  │  • If source is `.mkv`: Live Matroska EBML Demuxer in Rust ->     │  │
│  │    re-packages H.264 NALUs + Audio into Fragmented MP4 (`fMP4`:   │  │
│  │    `ftyp` + `moov` + `moof`/`mdat`) with zero video transcoding!  │  │
│  └───────────────┬───────────────────────────────────────────────────┘  │
│                  │ Clean `.mp4` / `fMP4` HTTP Stream                    │
│                  ▼                                                      │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │ Embedded Native Hardware Player (`src/player/`)                   │  │
│  │  • Windows RT Media Foundation (`MFPlay` / `DirectShow` COM)      │  │
│  │    rendering directly inside the app's Child `HWND` via DXVA2     │  │
│  │  • Native `.srt` Subtitle Overlay renderer (`encoding_rs`)        │  │
│  └───────────────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────────────┘
```
3. Solving the Embedded Player & MKV Problem (0xC00D36C4)
3.1 Why MFPCreateMediaPlayer threw 0xC00D36C4 (MF_E_UNSUPPORTED_BYTESTREAM_TYPE)
librqbit serves streams at http://127.0.0.1:3030/torrents/{id}/stream/{idx} without a .mp4 extension in the URL path. Windows 8.1's Media Foundation Source Resolver relies on the URL extension (.mp4) and Content-Type: video/mp4 header to instantiate the MPEG-4 ByteStream Handler.

When the torrent file is an .mkv container, Windows 8.1 lacks a native MKV demuxer (which was only added in Windows 10), even though the video stream inside the MKV is standard 8-bit H.264 (avc1) supported by the Tegra 3 hardware decoder.

3.2 Two-Layer Standalone Solution (src/engine/remux_proxy.rs)
We run a lightweight HTTP proxy server on 127.0.0.1:3031 between librqbit (:3030) and the embedded player:

Explicit .mp4 Extension & Headers: The embedded player ALWAYS connects to http://127.0.0.1:3031/play/{id}/{idx}/movie.mp4.

Direct Pass-Through for .mp4: If the torrent file ends in .mp4 or .m4v, the proxy forwards Range headers directly to librqbit (:3030) and returns 206 Partial Content / 200 OK with Content-Type: video/mp4 and Accept-Ranges: bytes.

Zero-Transcode Live MKV-to-fMP4 Remuxer for .mkv:

When the torrent file ends in .mkv or .webm, the proxy reads the Matroska stream from librqbit, parses the EBML Tracks (extracting the CodecPrivate avcC H.264 SPS/PPS header and AAC esds config) and Cluster / SimpleBlock packets.

If the MKV audio track is AC3/EAC3 (which Windows RT Media Foundation may not decode natively), either transcode just the lightweight audio track to AAC/PCM in Rust or prioritize AAC streams in the stream selector.

Emits a standard Fragmented MP4 (fMP4) byte stream (ftyp + moov initialization segment followed by streaming moof + mdat fragments for each keyframe cluster) directly to Media Foundation / DirectShow.

4. Phased Implementation Roadmap
Phase 1: Fix Embedded Native Playback & Stream Proxy (0xC00D36C4 Fix)
Implement src/engine/remux_proxy.rs on 127.0.0.1:3031 serving /play/{id}/{idx}/movie.mp4.

Wire the embedded child HWND player (DirectShow IGraphBuilder + IVideoWindow and/or MFPlay MFPCreateMediaPlayer) to the :3031/.../movie.mp4 URL.

Verify that .mp4 streams play inside the embedded child HWND with hardware acceleration and working transport controls (Play/Pause, -15s, +30s, Fullscreen, Back to Catalog).

Phase 2: Live MKV-to-fMP4 Remuxer in Pure Rust
Add a lightweight Matroska demuxer + fMP4 muxer module inside src/engine/remux_proxy.rs.

Remux H.264 (V_MPEG4/ISO/AVC) + AAC (A_AAC) .mkv streams on-the-fly into fragmented MP4 (ftyp/moov/moof/mdat) so .mkv torrents play natively inside the embedded Media Foundation/DirectShow viewport without VLC.

Phase 3: Lightweight Native Stremio Core & Addon Manager (src/stremio_proto/ & src/state.rs)
Implement clean, native Rust structs for the Stremio Addon Protocol (Manifest, CatalogResponse, MetaDetail, StreamResponse, SubtitleTrack) without pulling heavy external dependencies.

Start with zero hardcoded third-party addons by default, provide an "Add Addon URL" input persisted in %TEMP%\rt_stream_data\addons.json, and include a prominent [⚡ DEV: Load Cinemeta + Torrentio] button for instant testing.

Implement "Continue Watching" state persistence (watch_history.json) and episode watched tracking.

Implement .srt subtitle downloading (chardetng + encoding_rs for Latin-1 Spanish accents) and render subtitles directly over the transport/video bar.

Phase 4: Modern UI Polish (Slint Software Renderer or Custom Dark-Mode Win32 Canvas)
Evaluate slint (renderer-software + backend-winit) on thumbv7a-pc-windows-msvc or upgrade the responsive Win32 UI (WM_SIZE) with custom dark-mode rendering (#121212 surface, poster thumbnails cached at 150x225 in %TEMP%\rt_stream_posters\, and clear stream quality badges).