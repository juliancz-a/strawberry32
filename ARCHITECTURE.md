# Strawberry32 (RT-Stream ARM32) - Arquitectura y Estructura del Código

Aplicación de streaming en Rust para **Windows RT 8.1 (ARM32)**, diseñada específicamente para dispositivos **Microsoft Surface RT** con procesadores Nvidia Tegra 3/4 (2 GB RAM, pantalla táctil 1366×768). Integra cliente de add-ons de Stremio (protocolo Stremio v3), motor de streaming BitTorrent HTTP Range nativo (`librqbit 8.0`), interfaz gráfica declarativa moderna en **Slint** (`renderer-software` + `backend-winit`), pipeline de imágenes con caché en disco y RAM LRU, y **reproductor de video embebido con Media Foundation (MFPlay)** acelerado por hardware Nvidia Tegra 3 (DXVA2) con ventana de controles overlay superpuesta mediante layered color-key.

---

## 1. Diagrama de Arquitectura Modular

```
┌────────────────────────────────────────────────────────────────────────┐
│             Slint UI (ui/app.slint, ui/theme.slint)                    │
│   Shell moderna Nuvio/Stremio: Barra lateral, Catálogo, Detalle, Player│
│                                                                        │
│  [ui/app.slint - Ventana Principal Slint]                              │
│    - Barra Superior / Lateral con Tokens del Theme (#FC5A8D, etc.)     │
│    - Catálogo con Posters e Indicadores de Estado                      │
│    - Vista Detalle Fusionada (Metadatos + Streams compatibles)         │
│    - Modo Spike de Imágenes (3 filas × 20 tarjetas horizontales)       │
│                                                                        │
│  [ui/overlay/player_overlay.slint - Ventana Overlay Layered Color-Key] │
│    - Superpuesta exactamente sobre el HWND nativo de video             │
│    - WS_EX_LAYERED + LWA_COLORKEY (#010002)                            │
│    - Controles de reproducción táctiles, barra de progreso y subtítulos│
│    - No roba foco (WS_EX_NOACTIVATE) y funciona en ventana/fullscreen  │
└──────────────┬───────────────────────────┬──────────────────────┬──────┘
               │                           │                      │
       invoke_from_event_loop      std::thread::spawn       Media Foundation
               │                           │                 (MFPlay COM)
               ▼                           ▼                      ▼
┌──────────────────────────┐  ┌──────────────────────────┐  ┌───────────┐
│     src/logger.rs        │  │     src/addons.rs        │  │src/player │
│ Telemetría multihilo:    │  │ Cliente Cinemeta/Torrent │  │ Engine de │
│ 1. stdout                │  │ - Schannel TLS nativo    │  │ Video     │
│ 2. strawberry.log        │  │ - Priorización [★ MP4]   │  │ Nativo    │
│ 3. crash.log (panics)    │  │ - Filtro Tegra 3 DXVA2   │  │ Media     │
│ 4. Cola en vivo para GUI │  │ - Series (SxxExx) / Pelis│  │ Foundation│
└──────────────────────────┘  └──────────────────────────┘  │ (HWND)    │
                                                            └─────┬─────┘
                                                                  │
                                      MFPCreateMediaPlayer       │
                                      adjunto al Panel HWND       │
                                                                  ▼
┌───────────────────────────────────────────────────────────┐  ┌─────────┐
│                     src/engine.rs                         │  │ PANEL   │
│  Motor de streaming BitTorrent en hilo tokio dedicado:    │◄─┤ HWND    │
│  - Perfil de baja memoria (<150 MB RAM para Tegra 3)      │  │ (Media  │
│  - Sin defer_writes_up_to (prohibido en 32-bit: pánico)   │  │  Found- │
│  - Inyección de trackers HTTP + UDP en Magnet y Options   │  │  ation) │
│  - Pre-fetching inteligente: Cabecera 1MB + Cola moov 512K│  └─────────┘
│  - Medición y registro de Time to First Byte (TTFB)       │
│  - HttpApi embebido en http://127.0.0.1:3030              │
└───────────────────────────────────────────────────────────┘
```

---

## 2. Descripción de Archivos y Módulos

### `src/main.rs` — Punto de Entrada
- **Responsabilidad:** Coordina el arranque secuencial de todos los subsistemas e instala el gancho de pánico (`std::panic::set_hook`) para registrar fallos fatales en `<storage_root>\logs\crash.log`.
- **Flujo de Ejecución:**
  1. Instala el manejador de pánicos para volcar ubicación y carga útil a `crash.log`.
  2. Inicializa el logger global (`logger::init()`), creando el archivo de log persistente.
  3. Resuelve la raíz de almacenamiento canónica (`state::get_app_storage_root()`), verificando permisos de escritura en `%LOCALAPPDATA%\Strawberry32` con fallback automático a `%TEMP%\Strawberry32`.
  4. Lanza el motor de streaming `librqbit` en un hilo de fondo (`engine::start_engine()`).
  5. Inicia la interfaz de usuario Slint en el hilo principal (`gui::run_app()`).
  6. Al cerrar la ventana, ejecuta `std::process::exit(0)` garantizando la terminación limpia de sockets y subprocesos.

### `src/logger.rs` — Telemetría, Diagnósticos y Sink para GUI
- **Responsabilidad:** Centraliza el registro de eventos de la aplicación con soporte para hilos concurrentes.
- **Categorías:** `[INFO]`, `[NET]`, `[TORRENT]`, `[PLAYER]`, `[ERR]`.
- **Canales de Salida Simultáneos:**
  1. **Consola estándar:** `println!` (para depuración en desarrollo).
  2. **Archivo persistente:** `logs/strawberry.log` y `logs/crash.log`.
  3. **Interfaz gráfica en vivo:** Inserta las líneas en una cola protegida por mutex, con notificación no bloqueante a la UI de Slint.

### `src/addons.rs` — Cliente de Stremio y Priorización de MP4
- **Responsabilidad:** Consultas a Cinemeta (catálogos, metadatos, pósters) y extracción/filtrado de streams en Torrentio para Películas y Series.
- **Características:**
  - **Schannel TLS Nativo:** Emplea `ureq` configurado con `native-tls` (Windows Schannel), garantizando compatibilidad con los certificados TLS de Windows 8.1 sin depender de `ring` ni `rustls`.
  - **Filtro Estricto para Nvidia Tegra 3/4:**
    - **Rechaza tajantemente:** `2160p`, `4K`, `UHD`, `1080p`, `FHD`, `HEVC`, `x265`, `265`, `10bit`, `HDR`, `DV` (Dolby Vision), `AV1`.
    - **Acepta:** `720p`, `480p`, `BDRip`, `DVDRip` codificados en **H.264 / AVC**.
  - **Priorización de MP4 Nativo:**
    - Los streams `.mp4` se marcan con `[★ NATIVO MP4]` y se sitúan obligatoriamente en la cima de la lista ordenados por seeders.
    - Los streams `.mkv` con H.264+AAC se marcan para remuxing o reproducción directa.
  - **Soporte de Series:**
    - Consulta de episodios en `/stream/series/{id}:{season}:{episode}.json`.

### `src/engine.rs` — Motor `librqbit 8.0` y Pre-fetching de Átomo `moov`
- **Responsabilidad:** Gestiona descargas BitTorrent y expone el archivo de video como un servidor HTTP local (`127.0.0.1:3030`).
- **Características:**
  - **Perfil de Baja Memoria:** Limita conexiones simultáneas (`concurrent_init_limit: Some(1)`).
  - **Nota crítica de estabilidad:** `defer_writes_up_to` **NUNCA** debe configurarse (debe permanecer en `None`), ya que en arquitecturas de 32 bits como ARM32 PE produce un desbordamiento/pánico inmediato en librqbit.
  - **Pre-fetching Concurrente Cabecera + Cola (moov atom):**
    - Petición 1: `Range: bytes=0-1048575` (primer megabyte del archivo con las cabeceras del contenedor).
    - Petición 2: `Range: bytes={file_len - 524288}-{file_len - 1}` (últimos 512 KB donde muchos MP4 guardan el índice `moov`).
  - **Telemetría en Vivo:** Emite peers activos, velocidad de descarga y progreso para alimentar la pantalla de carga del reproductor.

### `src/player.rs` — Reproductor Embebido Nativo Media Foundation
- **Responsabilidad:** Gestionar la reproducción de video directamente dentro del panel child `HWND` de la interfaz gráfica utilizando aceleración por hardware Nvidia Tegra 3 (DXVA2).
- **Implementación COM Nativa (Media Foundation / MFPlay):**
  - En Windows RT 8.1, DirectShow tradicional (`CLSID_FilterGraph`) no está registrado en el subsistema Win32 de escritorio (`0x80040154`). El pipeline nativo oficial y plenamente acelerado es **Media Foundation** (`mfplay.dll` / `mfplat.dll`):
    - `MFStartup(0x00020070, 0)`: Inicializa la plataforma Media Foundation.
    - `MFPCreateMediaPlayer(url, fStartPlayback = 1, 0, NULL, video_hwnd, &p_player)`: Conecta el stream HTTP/Range local directamente al renderizador acelerado por hardware de Tegra 3 embebido en el HWND.
    - `IMFPMediaPlayer::Play()`, `Pause()`, `Stop()`, `Shutdown()`, `Release()`.
    - `IMFPMediaPlayer::GetPosition()`, `GetDuration()` y `SetPosition()` mediante `PROPVARIANT` en unidades de 100ns para saltos temporales y visualización de tiempo.
    - `IMFPMediaPlayer::UpdateVideo()`: Llamado ante redimensionamiento de ventana para mantener la escala del video.

### `src/ui/` — Módulos de Interfaz Slint y Win32
- **`src/ui/mod.rs`**: Conecta los submódulos de UI.
- **`src/ui/overlay.rs`**: Gestiona la ventana secundaria Slint `PlayerOverlay` (`no-frame: true`, `WS_POPUP`, `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`) con `SetLayeredWindowAttributes(hwnd, 0x00020001, 0, LWA_COLORKEY)`. Resuelve el problema de espacio aéreo ("airspace") permitiendo que los controles y subtítulos se dibujen sobre el video nativo sin robar foco y sincronizados con el tamaño de ventana o pantalla completa.
- **`src/ui/images.rs`**: Pipeline de imágenes con 2 hilos de trabajo dedicados, decodificación pura en Rust vía `image` (JPEG/PNG), reducción bilineal rápida (filtro Triangle), caché en disco (`cache/images/<hash>_<w>x<h>.raw`) y caché en memoria RAM acotada a ≤ 48 MB mediante política LRU.

---

## 3. Patches y Adaptación a ARM32 (`windows-sys` y `windows`)

El proyecto incluye árboles parcheados en `vendor/` para habilitar compilación en `thumbv7a-pc-windows-msvc`:
- `vendor/windows-sys-0.61/`: Versión 0.61.2 principal con enlaces a funciones de usuario, GDI, shell y threading.
- `vendor/windows-sys-0.52/`: Versión 0.52 requerida por librqbit y dependencias indirectas.
- `vendor/windows-0.62/`: Bindings de Windows de nivel superior.

Corrigen la omisión de `target_arch = "arm"` en upstream para sockets, memoria, shell y controles comunes en Windows RT ARM32 PE/COFF.
