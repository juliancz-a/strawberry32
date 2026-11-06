# RT-Stream ARM32 - Arquitectura y Estructura del Código

Aplicación de escritorio 100% nativa Win32 en Rust para **Windows RT 8.1 (ARM32)**, diseñada para dispositivos **Microsoft Surface RT** con procesadores Nvidia Tegra 3/4. Integra cliente de addons de Stremio (Cinemeta y Torrentio), motor de streaming BitTorrent HTTP Range nativo (`librqbit 8.0`), pre-fetching de átomos `moov` MP4 para arranque instantáneo, y **reproductor de video DirectShow embebido directamente dentro de la ventana de la aplicación** con aceleración por hardware (Nvidia Tegra 3 DXVA2).

---

## 1. Diagrama de Arquitectura Modular

```
┌────────────────────────────────────────────────────────────────────────┐
│                        GUI Win32 (src/gui.rs)                          │
│   Conmutador de Vistas Dinámico (Catalog View <-> Embedded Player)     │
│                                                                        │
│  [Vista Catálogo]                                                      │
│    Top Bar: Búsqueda + Temporada (T) / Episodio (E) + Probar Conexión  │
│    Split Panes: Resultados Cinemeta (35%) │ Streams Torrentio (65%)    │
│    Action Bar: ▶ Reproducir (Nativo MP4)  │ Abrir en VLC (MKV)         │
│                                                                        │
│  [Vista Reproductor Embebido - Sin ventanas externas]                 │
│    Viewport: Panel child HWND con DirectShow hardware-accelerated      │
│    Transport Bar: ⏮ -15s │ ⏯ Play/Pausa │ ⏭ +30s │ ⏹ Catálogo │ ⛶     │
│                                                                        │
│  [Consola Inferior - Visible en ambas vistas]                          │
│    Telemetría en tiempo real, TTFB, Peers, Velocidad y Diagnósticos   │
└──────────────┬───────────────────────────┬──────────────────────┬──────┘
               │                           │                      │
       PostMessageW(WM_APP_*)      std::thread::spawn       DirectShow COM
               │                           │                      │
               ▼                           ▼                      ▼
┌──────────────────────────┐  ┌──────────────────────────┐  ┌───────────┐
│     src/logger.rs        │  │     src/addons.rs        │  │src/player │
│ Telemetría multihilo:    │  │ Cliente Cinemeta/Torrent │  │ Engine de │
│ 1. stdout                │  │ - Schannel TLS nativo    │  │ Video     │
│ 2. rt_stream_debug.log   │  │ - Priorización [★ MP4]   │  │ Nativo    │
│ 3. Cola en vivo para GUI │  │ - Filtro Tegra 3/4 DXVA2 │  │ DirectShow│
└──────────────────────────┘  │ - Series (SxxExx) / Pelis│  │ Embebido  │
                              └──────────────────────────┘  │ (HWND)    │
                                                            └─────┬─────┘
                                                                  │
                                      DirectShow IVideoWindow     │
                                      adjunto al Panel HWND       │
                                                                  ▼
┌───────────────────────────────────────────────────────────┐  ┌─────────┐
│                     src/engine.rs                         │  │ PANEL   │
│  Motor de streaming BitTorrent en hilo tokio dedicado:    │◄─┤ HWND    │
│  - Perfil de baja memoria (<150 MB RAM para Tegra 3)      │  │ (Direct-│
│  - Inyección de 15 trackers HTTP + UDP en Magnet y Options│  │  Show)  │
│  - Pre-fetching inteligente: Cabecera 1MB + Cola moov 512K│  └─────────┘
│  - Medición y registro de Time to First Byte (TTFB)       │
│  - HttpApi embebido en http://127.0.0.1:3030              │
└───────────────────────────────────────────────────────────┘
```

---

## 2. Descripción de Archivos y Módulos

### `src/main.rs` — Punto de Entrada
- **Responsabilidad:** Coordina el arranque secuencial de todos los subsistemas.
- **Flujo de Ejecución:**
  1. Inicializa el logger global (`logger::init()`), creando el archivo `%TEMP%\rt_stream_debug.log`.
  2. Valida la carpeta de cache (`engine::get_cache_dir()`).
  3. Lanza el motor de streaming `librqbit` en un hilo de fondo (`engine::start_engine()`).
  4. Inicia el bucle de mensajes Win32 principal en el hilo principal (`gui::run_app()`).
  5. Al cerrar la ventana (`WM_CLOSE` -> `WM_DESTROY`), ejecuta `std::process::exit(0)` garantizando que el proceso y todos los hilos de fondo (`tokio`, sockets) mueran de inmediato sin quedar colgados en el Administrador de Tareas.

### `src/logger.rs` — Telemetría, Diagnósticos y Sink para GUI
- **Responsabilidad:** Centraliza el registro de eventos de la aplicación con soporte para hilos concurrentes.
- **Categorías:** `[INFO]`, `[NET]`, `[TORRENT]`, `[PLAYER]`, `[ERR]`.
- **Canales de Salida Simultáneos:**
  1. **Consola estándar:** `println!` (útil si se ejecuta desde cmd/PowerShell en desarrollo).
  2. **Archivo persistente:** `%TEMP%\rt_stream_debug.log`.
  3. **Interfaz gráfica en vivo:** Inserta las líneas en una cola protegida por mutex, liberando el lock antes de emitir `PostMessageW(hwnd, WM_APP_NEW_LOG, 0, 0)` con una bandera atómica `LOG_NOTIFY_PENDING` para evitar tormentas de mensajes y deadlocks.

### `src/addons.rs` — Cliente de Stremio y Priorización de MP4
- **Responsabilidad:** Búsqueda en Cinemeta y extracción/filtrado de streams en Torrentio para Películas y Series.
- **Características:**
  - **Schannel TLS Nativo:** Emplea `ureq` configurado con `native_tls::TlsConnector` (Windows Schannel), garantizando compatibilidad con los certificados TLS de Windows 8.1.
  - **Filtro Estricto para Nvidia Tegra 3/4:**
    - **Rechaza tajantemente:** `2160p`, `4K`, `UHD`, `1080p`, `FHD`, `HEVC`, `x265`, `265`, `10bit`, `HDR`, `DV` (Dolby Vision), `AV1`.
    - **Acepta:** `720p`, `480p`, `BDRip`, `DVDRip` codificados en **H.264 / AVC**.
  - **Priorización de MP4 Nativo:**
    - Los streams `.mp4` se marcan con `[★ NATIVO MP4]` y se sitúan obligatoriamente en la cima de la lista ordenados por seeders.
    - Los streams `.mkv` se marcan con `[MKV EXTERNO]` para reproducir opcionalmente con VLC.
  - **Soporte de Series:**
    - Al seleccionar una serie, lee la Temporada y Episodio seleccionados en la GUI y consulta `/stream/series/{id}:{season}:{episode}.json`.

### `src/engine.rs` — Motor `librqbit 8.0` y Pre-fetching de Átomo `moov`
- **Responsabilidad:** Gestiona descargas BitTorrent y expone el archivo de video como un servidor HTTP local (`127.0.0.1:3030`).
- **Características:**
  - **Perfil de Baja Memoria:** Limita conexiones simultáneas (`concurrent_init_limit: Some(1)`) y restringe la cola de disco (`defer_writes_up_to: Some(2MB)`) para que el consumo de RAM no supere los 150 MB en la Surface RT.
  - **15 Trackers Públicos HTTP y UDP:** Inyecta trackers redundantes percent-encoded en el Magnet URI y directamente en `AddTorrentOptions.trackers`.
  - **Pre-fetching Concurrente Cabecera + Cola (moov atom):**
    - Petición 1: `Range: bytes=0-1048575` (primer megabyte del archivo con las cabeceras del contenedor).
    - Petición 2: `Range: bytes={file_len - 524288}-{file_len - 1}` (últimos 512 KB donde muchos MP4 guardan el índice `moov`).
    - Mide el **Time to First Byte (TTFB)** y lo registra en la consola de diagnósticos (`[TORRENT] TTFB: X ms | Cabecera y cola MP4 listas`).
  - **Telemetría en Vivo:** Emite cada 2 segundos los peers activos conectados, velocidad de bajada y progreso en megabytes.

### `src/player.rs` — Reproductor Embebido Nativo Media Foundation y Fallback VLC
- **Responsabilidad:** Gestionar la reproducción de video directamente dentro del panel child `HWND` de la interfaz gráfica sin ventanas externas ni navegadores web, utilizando la aceleración de hardware Nvidia Tegra 3 (DXVA2).
- **Implementación COM Nativa de Windows RT (Media Foundation):**
  - En Windows RT 8.1, DirectShow (`CLSID_FilterGraph`) no está registrado en el sistema (`0x80040154`). El pipeline nativo oficial es **Media Foundation** (`mfplay.dll` / `mfplat.dll`):
    - `MFStartup(0x00020070, 0)`: Inicializa la plataforma Media Foundation.
    - `MFPCreateMediaPlayer(url, fStartPlayback = 1, 0, NULL, video_hwnd, &p_player)`: Conecta directamente el stream HTTP/Range de librqbit al renderizador acelerado por hardware de Tegra 3 embebido en el HWND del contenedor.
    - `IMFPMediaPlayer::Play()`, `Pause()`, `Stop()`, `Shutdown()`, `Release()`.
    - `IMFPMediaPlayer::GetPosition()`, `GetDuration()` y `SetPosition()` mediante `PROPVARIANT` en unidades de 100ns para saltos temporales (`-15s` / `+30s`) y timecode dinámico en pantalla.
    - `IMFPMediaPlayer::UpdateVideo()`: Llamado en `WM_SIZE` para redibujar y ajustar automáticamente el video al cambiar el tamaño de la ventana.
- **Fallback Automático y Manual a VLC:**
  - Si Media Foundation falla al abrir un formato no compatible (por ejemplo MKV con audio DTS/AC3), el sistema lanza automáticamente VLC sin bloquear la interfaz. También cuenta con el botón secundario `"Abrir en VLC (MKV)"` y selector con `GetOpenFileNameW`.

### `src/gui.rs` — Interfaz Táctil Win32 con Conmutador de Vistas
- **Responsabilidad:** Control de la ventana principal y gestión del cambio entre la Vista de Catálogo y la Vista de Reproductor Embebido.
- **Conmutador de Vistas (View Switcher):**
  1. **Vista de Catálogo:**
     - Barra de búsqueda con campos de Temporada (`T:`) y Episodio (`E:`).
     - Lista de resultados (35%) y lista de streams (65%) con scroll horizontal (`LB_SETHORIZONTALEXTENT`).
     - Botón principal: `"▶ Reproducir (Nativo MP4)"`.
     - Botón secundario: `"Abrir en VLC (MKV)"`.
  2. **Vista de Reproductor Embebido:**
     - Panel de video (`STATIC` child `HWND`) ocupando el 70-75% del alto de la ventana.
     - Barra de transporte con botones táctiles: `"⏮ -15s"`, `"⏯ Play/Pausa"`, `"⏭ +30s"`, `"⏹ Volver al Catálogo"`, `"⛶ Pantalla Completa"` e indicador de tiempo dinámico.
     - Redimensionamiento suave (`WM_SIZE`) que recalcula las coordenadas del viewport y notifica a Media Foundation para mantener la escala de video correcta.
- **Consola Inferior Persistente:** Mantiene visible la telemetría y diagnósticos en tiempo real en ambas vistas.

---

## 3. Patches y Adaptación a ARM32 (`windows-sys`)

El proyecto incluye dos copias parcheadas en `vendor/`:
- `vendor/windows-sys-0.60/`: Versión 0.60.2 inicial.
- `vendor/windows-sys-0.61/`: Versión 0.61.2 enlazada en `Cargo.toml`.

Ambas corrigen la omisión de `target_arch = "arm"` en upstream para las siguientes estructuras clave:
1. **Red / Sockets:** `WSADATA`, `SERVENT`
2. **Sistema:** `DELAYLOAD_INFO`, `MEMORY_BASIC_INFORMATION`
3. **Shell:** `APPBARDATA`, `SHFILEOPSTRUCTA`, `SHFILEOPSTRUCTW`, `SHQUERYRBINFO`, `NOTIFYICONIDENTIFIER`, `ASSOCIATIONELEMENT`, `SHSTOCKICONINFO`, `SHELLEXECUTEINFOA`, `SHELLEXECUTEINFOW`, `NOTIFYICONDATAA`, `NOTIFYICONDATAW`, `SHFILEINFOA`, `SHFILEINFOW`, `SHCREATEPROCESSINFOW`
4. **Controles Comunes:** `TBBUTTON`

El archivo `arm32-windows-sys-0.61.patch` en la raíz del repositorio contiene el diff unificado completo validado con `git apply --check`.
