<p align="center">
  <img src="ui/icons/brand/strawberry32-horizontal-color.svg" width="560" alt="Strawberry32 Horizontal Logo"/>
</p>

<p align="center">
  <strong>Native, hardware-accelerated BitTorrent streaming media client built specifically for Windows RT 8.1 and ARM32 devices.</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Platform-Windows%20RT%208.1%20(ARM32)-ff4b82.svg" alt="Platform"/>
  <img src="https://img.shields.io/badge/Target-thumbv7a--pc--windows--msvc-10b981.svg" alt="Target"/>
  <img src="https://img.shields.io/badge/Language-Rust%202021-f59e0b.svg" alt="Rust"/>
  <img src="https://img.shields.io/badge/UI%20Toolkit-Slint%20v1.9-6366f1.svg" alt="Slint"/>
  <img src="https://img.shields.io/badge/License-GPL--3.0--or--later-blue.svg" alt="License"/>
</p>

---

## Overview

**Strawberry32** is a dedicated streaming entertainment media player engineered from scratch for the Microsoft Surface RT (1st Gen), Surface 2, and other 32-bit ARM (ARMv7 / `thumbv7a`) Windows RT devices. 

By combining **Rust**, **Slint CPU Software Rendering**, **librqbit**, and low-level **Windows Media Foundation** APIs, Strawberry32 bypasses modern heavy web runtimes, delivering smooth 720p/1080p video playback and fluid catalog navigation within a tight 2 GB RAM budget and low-power Tegra 3 SoC.

<!-- Main Application Showcase -->
<p align="center">
  <img src="ui/screenshots/home_view.png" width="850" alt="Strawberry32 Home Interface on Windows RT 8.1"/>
</p>

---

## Features

- **⚡ Hardware-Accelerated Playback**:
  Streams video through Windows Media Foundation (`IMFPMediaPlayer`), utilizing NVIDIA Tegra 3 hardware H.264 (8-bit) decoding for fluid 720p and 1080p playback with negligible CPU load.
- **🔄 Intelligent Torrent Streaming Engine**:
  Powered by `librqbit 8.0` with dynamic container probing (`moov` atom for MP4 and `EBML/Cues` for MKV). Pre-fetches container headers and initial chunks so video starts buffering rapidly with multi-stage progress tracking.
- **🌐 Modular Stremio Add-on Protocol**:
  Compatible with standard Stremio v3 protocol add-ons. Users can connect modular add-on manifests via HTTP/HTTPS URLs to browse catalogs and stream sources.
- **🎬 Native Jellyfin Integration**:
  Connect directly to your personal Jellyfin server over your local network (LAN) or HTTPS—no plugins or IMDb IDs required. Automatically chooses between Direct Play and server-side H.264/AAC transcoding tailored for Tegra 3, merges Jellyfin sources onto Cinemeta cards with deduplication, and includes dedicated **Jellyfin Only** filters and Jellyfin subtitle support.
- **🎨 Touch & Keyboard-First Slint UI**:
  Obsidian dark theme (`#0e0e12`) with strawberry accents (`#ff4b82`), paged catalog rows with on-demand image decoding, collapsible details drawer, and native fullscreen support.
- **💬 Layered GDI Subtitle Engine**:
  Per-pixel alpha layered subtitle window with real-time scaling (50%–200%), live sync delay adjustment (-250ms / +250ms), and OpenSubtitles v3 integration.
- **🔍 Advanced Search, Filtering & Sorting**:
  Search movies and series, filter by Genre (Action, Comedy, Sci-Fi, Horror, etc.), sort by Rating, Year, Popularity, or Title with ascending/descending toggles.
- **⏮️ Continue Watching & Library**:
  Resumes recently played content directly from the Home screen or Library tab, storing watch progress locally.
- **💾 Low-Footprint Storage & Cache**:
  All telemetry, cache, logs, and settings are consolidated under `%LOCALAPPDATA%\Strawberry32`, with configurable disk limits to safeguard eMMC flash storage.

---

## 📸 Screenshots & Previews

| Home Catalog & Hero Banner | Search with Interactive Filter Chips |
| :---: | :---: |
| <img src="ui/screenshots/home_view.png" width="450" alt="Home View"/><br><sub>Featured Cinemeta catalog rows and high-res Hero Banner on Tegra 3</sub> | <img src="ui/screenshots/search_view.png" width="450" alt="Search & Filters"/><br><sub>Dedicated Search tab with Genre and Sort chips and instant discovery</sub> |

| Detail View & Stream Selection | In-Player Hardware Playback & Subtitles |
| :---: | :---: |
| <img src="ui/screenshots/details_view.png" width="450" alt="Detail View"/><br><sub>Cinemeta metadata, IMDb ratings, and smart stream compatibility badges</sub> | <img src="ui/screenshots/media_player_view.png" width="450" alt="Player View"/><br><sub>Windows Media Foundation H.264 hardware decoding with true transparent GDI subtitles</sub> |

<p align="center">
  <img src="ui/screenshots/settings_view.png" width="700" alt="Settings & Add-on Manager"/><br/>
  <sub>Extensive Settings: Add-on manager with priority ordering, streaming limits, card sizing, and storage cache cleaner</sub>
</p>


## 🎬 Connecting to Jellyfin

<p align="center">
  <img src="ui/screenshots/jellyfin_connection.png" width="700" alt="Native Jellyfin Connection Settings"/><br/>
  <sub>Native Jellyfin Integration: Connect directly to your Jellyfin server via LAN IP or HTTPS with automatic Direct Play / server-side transcoding</sub>
</p>

Strawberry32 includes a built-in native Jellyfin client (no server-side plugins required):

1. **Find your Jellyfin server's LAN IP** on the machine hosting Jellyfin (for example, `http://192.168.0.120:8096`). If you are hosting on Windows, you can check your LAN IP using 'ipconfig'. You need to copy the IPv4 address.
2. Open **Settings → Jellyfin** in Strawberry32 (shown above):
   - Enter your **Server URL** (e.g. `http://192.168.0.120:8096`).
   - Enter your Jellyfin **Username** and **Password**, then click **Connect**.
3. Once connected, you can:
   - Browse and search your library (duplicate titles are automatically merged with Cinemeta cards, and you can toggle **Jellyfin Only** in Search).
   - Open any title and use the **Jellyfin** filter pill (highlighted in blue) to view your server's Direct Play or transcoded H.264/AAC streams, along with Jellyfin's embedded/external subtitles.

---

## ⌨️ Controls & Shortcuts

| Input | Action |
| :--- | :--- |
| **Space** | Play / Pause |
| **Left Arrow** / **Touch Double-Tap Left** | Seek Backward 10 seconds |
| **Right Arrow** / **Touch Double-Tap Right** | Seek Forward 10 seconds |
| **Up Arrow** | Increase Volume (+5%) |
| **Down Arrow** | Decrease Volume (-5%) |
| **F** / **Touch Double-Tap Center** | Toggle Fullscreen |
| **M** | Toggle Mute |
| **Escape** | Exit Fullscreen / Back |

---

## 🛠️ Architecture & Technical Stack

```
   ┌────────────────────────────────────────────────────────────┐
   │             Strawberry32 Slint User Interface               │
   │      (Pure Software Renderer - No D3D11 overhead)          │
   └───────────────┬────────────────────────────┬───────────────┘
                   │ User Events                │ Render Calls
                   ▼                            ▼
   ┌────────────────────────────────────────────────────────────┐
   │               Native Rust Application Core                 │
   │  ┌──────────────────────┐        ┌──────────────────────┐  │
   │  │   Addon Manager      │        │  Metadata Inspector  │  │
   │  │ (Stremio v3 Protocol)│        │ (moov & EBML Cues)   │  │
   │  └──────────┬───────────┘        └──────────┬───────────┘  │
   │             │ Stream Resource               │ Byte Ranges  │
   │             ▼                               ▼              │
   │  ┌──────────────────────────────────────────────────────┐  │
   │  │      librqbit BitTorrent Swarm Streaming Engine      │  │
   │  └──────────────────────────┬───────────────────────────┘  │
   │                             │ Local HTTP Server (3031)     │
   └─────────────────────────────┼──────────────────────────────┘
                                 │ HTTP Bytestream
                                 ▼
   ┌────────────────────────────────────────────────────────────┐
   │              Windows Media Foundation (MFP)                │
   │      NVIDIA Tegra 3 VPU Hardware H.264 Decoding            │
   └────────────────────────────────────────────────────────────┘
```

### Surface RT (Tegra 3) Engineering Constraints
1. **CPU/GPU Constraints**: NVIDIA Tegra 3 (Quad-core Cortex-A9 @ 1.3 GHz) lacks Direct3D 11 feature level support for modern GPU compositors. Strawberry32 utilizes the Slint Pure Software Renderer, eliminating GPU crashes and driver timeouts.
2. **Video Codec Compatibility**: Hardware decoding on Windows RT 8.1 supports 8-bit H.264 (AVC) up to 1080p. Incompatible streams (e.g. 10-bit HEVC/x265 or AV1) trigger a clear prompt to open in external VLC.
3. **Memory Limits**: The application maintains a lean working set (< 150 MB typical RAM usage) ensuring it never triggers aggressive memory paging on 2 GB RAM devices.

---

## Building & Installation

### Prerequisites

The supported build environment is a **Windows 10/11 x64 host** cross-compiling for **Windows RT 8.1 ARM32**. The compiler tools run on x64; the resulting application targets ARM32 (`thumbv7a-pc-windows-msvc`).

You do **not** need to install the complete Visual Studio IDE or download a full Windows SDK if you already have the compatible headers and libraries described below.

Required components:

- **Rust compiler (`rustc`)**: the custom ARM32-targeting Rust toolchain package described in [Toolchain](#toolchain), including its `thumbv7a-pc-windows-msvc` standard library.
- **Cargo**: installed separately. The custom `rustc` archive used here does not include Cargo. Ensure Cargo invokes the intended custom `rustc`.
- **ARM32 LLVM toolchain**: hamed7ir's `rt2.1` package, providing `clang-cl`, `lld-link`, `llvm-lib`, and related tools.
- **Windows SDK / MSVC-compatible development files**: the CRT, UCRT, and Windows SDK headers and libraries listed below.
- **Git**: to clone the repository and fetch its source dependencies.

> **Important:** `rustup target add thumbv7a-pc-windows-msvc` on an ordinary Rust installation is not a substitute for the custom Rust compiler package and target standard library used by this project.

### Toolchain

Strawberry32 was built on a Windows x64 host using [hamed7ir/llvm-rt1](https://github.com/hamed7ir/llvm-rt1), specifically the **rt2.1 / LLVM 23.1.1 ARM32 Windows toolchain**.

- Project: https://github.com/hamed7ir/llvm-rt1
- Toolchain directory used in the development environment:
  `E:\arm32-tools\rt2.1-23.1.1-rt2.1-arm32-win\`
- Tools used:
  - `clang-cl.exe` — C/C++ compiler used by native dependencies and build scripts.
  - `lld-link.exe` — linker for the final ARM32 executable.
  - `llvm-lib.exe` — librarian used where native dependencies require it.
- Rust: the matching custom `rustc` build for `thumbv7a-pc-windows-msvc` was used. Cargo was installed separately because the custom `rustc` archive did not include it.

The toolchain runs on **Windows x64** and emits **32-bit ARM Windows binaries**. The `win64` host designation in the Rust compiler package refers to the machine running the compiler, not the architecture of Strawberry32.

The LLVM toolchain does not itself provide the Windows SDK or MSVC CRT headers/libraries. This project used a manually assembled SDK/CRT subset under `E:\arm32-tools\winsdk\` rather than relying on a full Visual Studio installation.

### Windows SDK and CRT files

The build configuration expects this directory structure (the paths reproduce the development machine layout; adjust them for your installation):

```text
E:\arm32-tools\winsdk\
├── crt\
│   ├── include\
│   └── lib\
│       ├── aarch\
│       └── x86_64\
└── sdk\
    ├── include\
    │   ├── shared\
    │   ├── um\
    │   └── ucrt\
    └── lib\
        ├── ucrt\
        │   ├── aarch\
        │   └── x86_64\
        └── um\
            ├── aarch\
            └── x86_64\
```

The two library architectures serve different compilation stages:

- `aarch`: libraries used when linking the final ARM32 application.
- `x86_64`: libraries used by host-side Windows build scripts and native build tools that execute on the development PC.

Do not interchange these directories. The final application must link against ARM32 libraries, while host build scripts must link against x64 libraries.

### Cargo configuration

The following is the target configuration used during development. Save it as `.cargo/config.toml` in the repository root. Update the paths if your toolchain or SDK is installed elsewhere.

```toml
[build]
target = "thumbv7a-pc-windows-msvc"

[env]
LIB = "<ARM32_TOOLS>\\winsdk\\crt\\lib\\x86_64;<ARM32_TOOLS>\\winsdk\\sdk\\lib\\um\\x86_64;<ARM32_TOOLS>\\winsdk\\sdk\\lib\\ucrt\\x86_64"
INCLUDE = "<ARM32_TOOLS>\\winsdk\\crt\\include;<ARM32_TOOLS>\\winsdk\\sdk\\include\\ucrt;<ARM32_TOOLS>\\winsdk\\sdk\\include\\um;<ARM32_TOOLS>\\winsdk\\sdk\\include\\shared"
CC_thumbv7a_pc_windows_msvc = "<ARM32_TOOLS>\\rt2.1-23.1.1-rt2.1-arm32-win\\bin\\clang-cl.exe"
AR_thumbv7a_pc_windows_msvc = "<ARM32_TOOLS>\\rt2.1-23.1.1-rt2.1-arm32-win\\bin\\llvm-lib.exe"
CFLAGS_thumbv7a_pc_windows_msvc = "--target=thumbv7a-pc-windows-msvc -D_ARM_WINAPI_PARTITION_DESKTOP_SDK_AVAILABLE=1"

[target.thumbv7a-pc-windows-msvc]
linker = "<ARM32_TOOLS>\\rt2.1-23.1.1-rt2.1-arm32-win\\bin\\lld-link.exe"
rustflags = [
    "-C", "target-feature=+crt-static",
    "-C", "link-arg=/SUBSYSTEM:WINDOWS,6.03",
    "-C", "link-arg=/LIBPATH:<ARM32_TOOLS>\\winsdk\\crt\\lib\\aarch",
    "-C", "link-arg=/LIBPATH:<ARM32_TOOLS>\\winsdk\\sdk\\lib\\um\\aarch",
    "-C", "link-arg=/LIBPATH:<ARM32_TOOLS>\\winsdk\\sdk\\lib\\ucrt\\aarch"
]

[target.x86_64-pc-windows-msvc]
linker = "<ARM32_TOOLS>\\llvm-msvc-x86_64-armv7-cross\\bin\\lld-link.exe"
rustflags = [
    "-C", "link-arg=/LIBPATH:<ARM32_TOOLS>\\winsdk\\crt\\lib\\x86_64",
    "-C", "link-arg=/LIBPATH:<ARM32_TOOLS>\\winsdk\\sdk\\lib\\um\\x86_64",
    "-C", "link-arg=/LIBPATH:<ARM32_TOOLS>\\winsdk\\sdk\\lib\\ucrt\\x86_64"
]
```

The `x86_64-pc-windows-msvc` section is for host-side build scripts (`build.rs`) that Cargo compiles and runs on the development PC. The `thumbv7a-pc-windows-msvc` section is for the final application and target-side native dependencies.

> **Note:** replace every `<ARM32_TOOLS>` with the folder where you keep the toolchain and SDK/CRT subset (for example `C:\\arm32-tools`; backslashes must be doubled inside TOML double-quoted strings). Keep the ARM32 and x64 library paths separated as shown.

### Verify the environment

Run these commands in PowerShell after configuring `PATH` and the Rust compiler:

```powershell
rustc -vV
cargo --version
clang-cl --version
lld-link --version
llvm-lib --version
```

Confirm that `rustc -vV` reports the intended custom compiler and that its target standard library includes `thumbv7a-pc-windows-msvc`. Also verify that the LLVM commands resolve to the `rt2.1` installation, not to an unrelated LLVM installation.

### Compilation

Clone the repository and build the release binary:

```powershell
git clone https://github.com/your-username/Strawberry32.git
cd Strawberry32
cargo build --release --target thumbv7a-pc-windows-msvc
```

The resulting binary will be located at:

```text
target/thumbv7a-pc-windows-msvc/release/Strawberry32.exe
```

### Deploying to Windows RT 8.1

1. Ensure your Surface RT / Windows RT device is jailbroken to run unsigned desktop applications (e.g. via the standard RT 8.1 Jailbreak / Testsigning tool).
2. Copy `Strawberry32.exe` to a folder on your device (e.g. `C:\Strawberry32\`).
3. Run `Strawberry32.exe`. On first launch, the app initializes its local storage in `%LOCALAPPDATA%\Strawberry32` and loads its configured metadata and add-on sources.

> **Compatibility note:** this project targets ARM32 Windows, but the intended runtime is specifically Windows RT 8.1. A successful cross-compilation does not by itself guarantee runtime compatibility; validate each release on the actual Surface RT / Windows RT 8.1 device.


---

## Legal & Compliance Notice

Strawberry32 is an open-source media player and protocol client. 
- Strawberry32 does not host, index, archive, or distribute any media files, torrents, or streams.
- All metadata, catalog information, and subtitle data are provided via external third-party add-ons configured by the end user under standard Stremio v3 protocol specifications.
- Strawberry32 developers do not operate or endorse third-party add-on servers and are not responsible for content accessed by users.
- Users are solely responsible for ensuring that their use complies with all applicable local laws, regulations, and copyright requirements.

---

## Licensing

This project is licensed under the **GNU General Public License v3.0 (GPLv3)**.

### Technology Licensing Details
- **Slint UI**: Licensed under GPLv3 (for open-source desktop usage).
- **librqbit**: Licensed under the Apache License, Version 2.0.
- **stremio-native-stream-server modules**: Ported under the MIT License.

Because Slint is linked into the application under the terms of the GNU General Public License v3.0, the overall combined work is distributed under the **GPL-3.0-or-later**. See the [LICENSE](LICENSE) file for the full license text.

---

## Credits & Acknowledgments

- **Slint**: Beautiful, efficient native GUI toolkit developed by [Slint Software](https://slint.dev/).
- **librqbit**: Lightweight BitTorrent client library written in Rust by [ikatson](https://github.com/ikatson/rqbit).
- **Stremio**: Open-source streaming system and add-on architecture by [Stremio Team](https://www.stremio.com/).
- **stremio-native-stream-server**: Reference torrent streaming pipeline by [perpetus](https://github.com/perpetus/stremio-native-stream-server).
- **Windows RT Community**: Dedicated enthusiasts keeping ARM32 devices functional and alive.
