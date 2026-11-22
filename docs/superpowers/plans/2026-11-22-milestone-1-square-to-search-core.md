# Milestone 1: Foundation & Square to Search Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a working, tested, cross-platform Rust + Slint desktop application implementing the 6-crate workspace foundation and the full **Square to Search** workflow (System Tray, Global Hotkeys, freeze-frame snipping overlay + floating action bar, local PP-OCRv5 ONNX OCR with SHA-256 manifest & first-run starter download, free Google/Bing web translator + LRU cache, inline Copy/Translate quick card, Google Lens/Yandex/Bing browser launch with clipboard fallback, and 5-tab Settings UI).

**Architecture:** Cargo workspace split into 6 single-responsibility crates (`sts-core`, `sts-platform`, `sts-ocr`, `sts-translate`, `sts-search`, `sts-app`) communicating over trait boundaries and Tokio async channels, with Slint `1.19` declarative UI driven via `export global` state/callback bindings. Milestone 2 (Continuous Live Translate) and Milestone 3 (BYOK AI streaming, advanced Wayland & release packaging) will each receive their own dedicated plans after Milestone 1 ships and passes verification.

**Tech Stack:** Rust `1.100.0` (pinned in `rust-toolchain.toml`), Slint `1.19.0` + `slint-build 1.19.0`, Tokio `1.54.1`, `oar-ocr 0.11.0` (CPU + `directml` on Windows only), `xcap 0.9.10`, `global-hotkey 0.10.0`, `tray-icon 0.26.1` (`ksni` feature on Linux), `arboard 3.7.0`, `open 5.4.5`, `reqwest 0.13.5`, `lru 0.19.1`, `sha2 0.11.1`, `directories 6.0.0`, `thiserror 2.0.21`, `anyhow 1.0.104`, `wiremock 0.6.5`, `tempfile 3.27.0`.

**Spec:** `docs/superpowers/specs/2026-11-22-square-to-search-design.md`

## Global Constraints

- Rust toolchain pinned at `1.100.0` (`rust-toolchain.toml`), Slint `1.19.0+`.
- Targets: Windows 10/11 (`x86_64-pc-windows-msvc`) and Linux (`x86_64-unknown-linux-gnu`, X11 & Wayland). No macOS in v1.
- Execution devices in v1: `Auto`, `Cpu`, `DirectMl` (Windows only). **No CUDA** features, DLLs, or Settings options in v1 (Decision 3A).
- No Python, Node.js, or CUDA Toolkit runtime dependencies for end users.
- No silent uploads to third-party image hosts; Visual Search uploads directly to Google Lens / Yandex / Bing with clipboard fallback.
- Free default translator uses Google Translate web endpoint (`translate_a/single`) with Bing web fallback, batched per request + LRU cache, transparently disclosed in Settings (Decision 1A).
- Linux Wayland: Floating Panel & Square to Search on all DEs; GNOME Wayland multi-monitor Square to Search captures the focused/active monitor (Decision 2A).
- Rust discipline: `thiserror` in library crates, `anyhow` only in `sts-app`, no `unwrap()`/`expect()` in production code, `cargo clippy --workspace --all-targets -- -D warnings` must pass, `cargo fmt --all -- --check` must pass.
- Slint discipline: every `.slint` file must pass `.tools/bin/slint-viewer --check` and render cleanly via `--screenshot`.

## Review Focus

1. **HiDPI & negative-origin multi-monitor drag selection:** Dragging right-to-left/bottom-to-top or across edges on a 125%/150%/200% DPI display or negative-coordinate secondary monitor must normalize to a valid positive `PhysicalRect` clamped inside the frame without out-of-bounds panic *(pinned in Task 1 & Task 2)*.
2. **Corrupted or interrupted OCR model download:** Mid-transfer failure or SHA-256 mismatch when downloading a PP-OCRv5 ONNX pack must delete the temporary `.part` file, return `OcrError::ChecksumMismatch`, and never overwrite or corrupt an existing valid pack *(pinned in Task 3)*.
3. **Batch separator collision & HTTP 429 in free web translator:** Source text containing separator tokens or upstream returning HTTP `429`/HTML CAPTCHA must never scramble translated bubble order, must fall back to Bing web translator, and must return `TranslateError::RateLimited` if both fail *(pinned in Task 4)*.
4. **Google Lens direct upload failure / missing redirect:** Network outage, timeout, or upstream non-redirect on `POST /v3/upload` must copy the cropped PNG/RGBA to the OS clipboard, open `https://lens.google.com`, and return `VisualSearchOutcome::FallbackClipboard` *(pinned in Task 5)*.
5. **Corrupt or outdated `config.toml` on disk:** Missing newly added fields must backfill from `Default`, and syntactically broken TOML must back up the corrupt file to `config.toml.bak` and recover cleanly with defaults instead of failing to boot *(pinned in Task 1)*.

---

## File Structure

```text
square-to-search/
├── Cargo.toml
└── crates/
    ├── sts-core/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs          # Public re-exports & CoreError
    │       ├── geometry.rs     # PhysicalRect, LogicalRect, MonitorGeometry, drag & HiDPI crop math
    │       ├── types.rs        # CaptureFrame, OcrLine, TextBlock, TranslatedBlock, config enums
    │       ├── config.rs       # AppConfig (TOML) + ConfigStore (load/backfill/backup/atomic save)
    │       └── cache.rs        # TranslationCache (LRU keyed by text hash + lang pair)
    ├── sts-platform/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs          # Public re-exports & PlatformError
    │       ├── display_env.rs  # DisplayServer detection (Windows, LinuxX11, LinuxWayland)
    │       ├── capture.rs      # ScreenCapturer trait + XcapCapturer + crop_frame
    │       ├── clipboard.rs    # ClipboardService trait + ArboardClipboard
    │       ├── hotkeys.rs      # HotkeyAction, HotkeyBinding parser, HotkeyManager + GlobalHotkeyService
    │       └── tray.rs         # TrayCommand + TrayService (tray-icon; ksni on Linux)
    ├── sts-ocr/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs          # Public re-exports & OcrError
    │       ├── manifest.rs     # ModelManifest (CJK+EN, Korean, Vi/Latin × Mobile/Server) + ModelStore
    │       ├── downloader.rs   # Streaming reqwest + SHA-256 .part atomic downloader
    │       └── engine.rs       # OcrEngine trait, OarOcrEngine (CPU/DirectML), IdleManagedOcrEngine
    ├── sts-translate/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs          # Translator & VisionAiProvider traits, TranslateError
    │       ├── batch.rs        # Collision-resistant batch pack/unpack
    │       ├── google_web.rs   # GoogleWebTranslator (translate_a/single)
    │       ├── bing_web.rs     # BingWebTranslator fallback
    │       └── service.rs      # CachedFallbackTranslator (LRU + Google -> Bing fallback)
    ├── sts-search/
    │   ├── Cargo.toml
    │   └── src/
    │       ├── lib.rs          # BrowserOpener, VisualSearchOutcome, VisualSearchService (Lens/Yandex/Bing + fallback)
    └── sts-app/
        ├── Cargo.toml
        ├── build.rs            # slint_build::compile("ui/app.slint")
        ├── ui/
        │   ├── app.slint       # Root imports & shared export globals (AppState, AppActions)
        │   ├── snip-overlay.slint  # Freeze-frame selection, 4-action floating bar, inline result card
        │   ├── first-run-ocr.slint # 1-click starter OCR model (~25MB mobile CJK+EN) download modal
        │   └── settings.slint      # 5-tab Settings window
        ├── src/
        │   ├── lib.rs          # Re-exports AppController for integration tests
        │   ├── state.rs        # AppController async state machine & AppCommand/UiEvent channels
        │   ├── ui_bridge.rs    # Slint globals <-> AppController wiring
        │   └── main.rs         # Tracing, winit backend, Tokio runtime, Tray + Hotkeys + Slint loop
        └── tests/
            └── square_to_search_flow.rs  # End-to-end AppController integration tests with fakes
```

---

### Task 1: Workspace Skeleton & `sts-core` (Geometry, Types, Config, LRU Cache)

**Files:**
- Create: `Cargo.toml`
- Create: `crates/sts-core/Cargo.toml`, `crates/sts-core/src/lib.rs`, `crates/sts-core/src/geometry.rs`, `crates/sts-core/src/types.rs`, `crates/sts-core/src/config.rs`, `crates/sts-core/src/cache.rs`
- Create minimal stubs so workspace compiles: `crates/{sts-platform,sts-ocr,sts-translate,sts-search}/Cargo.toml` + `src/lib.rs`, `crates/sts-app/Cargo.toml` + `src/main.rs`
- Test: unit tests in `crates/sts-core/src/{geometry,config,cache}.rs`

**Interfaces:**
- Consumes: none
- Produces:
  - `sts_core::CoreError`
  - `sts_core::geometry::{PhysicalRect, LogicalRect, MonitorGeometry, normalize_drag_rect, logical_to_physical_crop}`
  - `sts_core::types::{CaptureFrame, OcrLine, TextBlock, TranslatedBlock, OcrLanguagePack, OcrModelTier, ExecutionDevice, LiveDisplayMode, ReadingDirection, VisualSearchEngine, UiLanguage}`
  - `sts_core::config::{AppConfig, HotkeyConfig, LiveTranslateConfig, OcrConfig, TranslateConfig, VisualSearchConfig, ConfigStore, ConfigLoadOutcome}`
  - `sts_core::cache::TranslationCache`

- [ ] **Step 1: Create workspace `Cargo.toml` and minimal crate stubs**

Create workspace `Cargo.toml` with `resolver = "2"`, `members = ["crates/*"]`, `[workspace.package]` (`edition = "2024"`, `rust-version = "1.100"`, `license = "MIT OR Apache-2.0"`), `[workspace.lints.clippy]` (`all = { level = "warn", priority = -1 }`, `perf = "warn"`, `unwrap_used = "warn"`, `expect_used = "warn"`), and `[workspace.dependencies]` pinning the verified crate versions (`slint = "1.19.0"`, `slint-build = "1.19.0"`, `oar-ocr = "0.11.0"`, `xcap = "0.9.10"`, `global-hotkey = "0.10.0"`, `tray-icon = { version = "0.26.1", default-features = false }`, `arboard = "3.7.0"`, `open = "5.4.5"`, `directories = "6.0.0"`, `lru = "0.19.1"`, `reqwest = { version = "0.13.5", features = ["json", "multipart", "stream", "query"] }`, `sha2 = "0.11.1"`, `hex = "0.4.3"`, `tokio = { version = "1.54.1", features = ["full"] }`, `image = "0.25.10"`, `thiserror = "2.0.21"`, `anyhow = "1.0.104"`, `serde = { version = "1.0.229", features = ["derive"] }`, `serde_json = "1.0.154"`, `toml = "1.2.2"`, `url = "2.5.9"`, `tracing = "0.1.44"`, `tracing-subscriber = "0.3.23"`, `wiremock = "0.6.5"`, `tempfile = "3.27.0"`). Create minimal `Cargo.toml` + `src/lib.rs` (or `src/main.rs` for `sts-app`) stubs for the 5 other crates.

- [ ] **Step 2: Write failing tests for `sts-core` geometry, config persistence, and LRU cache**

In `crates/sts-core/src/geometry.rs`, `config.rs`, and `cache.rs`, declare the public types and `todo!()` stub functions plus these unit tests:

```rust
// geometry.rs tests
#[test]
fn normalize_drag_rect_handles_reverse_drag_and_clamps_to_viewport() {
    let rect = normalize_drag_rect(500.0, 400.0, -20.0, 100.0, 800.0, 600.0);
    assert_eq!(rect, LogicalRect { x: 0.0, y: 100.0, width: 500.0, height: 300.0 });
}

#[test]
fn logical_to_physical_crop_scales_hidpi_and_clamps_within_frame() {
    let monitor = MonitorGeometry { id: 1, name: "Left".into(), x: -1920, y: 0, width: 1920, height: 1080, scale_factor: 1.5, is_primary: false };
    let logical = LogicalRect { x: 100.0, y: 50.0, width: 2000.0, height: 800.0 };
    let crop = logical_to_physical_crop(logical, &monitor);
    assert_eq!(crop, PhysicalRect { x: 150, y: 75, width: 1770, height: 1005 });
}

// config.rs tests (using tempfile::TempDir)
#[test]
fn default_config_matches_spec_values() {
    let cfg = AppConfig::default();
    assert_eq!(cfg.hotkeys.square_to_search, "Ctrl+Shift+S");
    assert_eq!(cfg.hotkeys.toggle_live_translate, "Ctrl+Shift+T");
    assert_eq!(cfg.hotkeys.toggle_live_visibility, "Ctrl+Shift+H");
    assert_eq!(cfg.live.debounce_ms, 350);
    assert_eq!(cfg.ocr.idle_unload_secs, 300);
    assert_eq!(cfg.ocr.tier, OcrModelTier::Mobile);
    assert_eq!(cfg.ocr.device, ExecutionDevice::Auto);
    assert_eq!(cfg.search.engine, VisualSearchEngine::GoogleLens);
}

#[test]
fn config_store_backfills_missing_fields_from_partial_toml() {
    // Write minimal TOML with only `[live]\ndebounce_ms = 500`
    // Assert load_or_default returns debounce_ms == 500 and all other fields at Default
}

#[test]
fn config_store_backs_up_corrupt_toml_and_recovers_default() {
    // Write `not = [valid toml` to config.toml
    // Assert load_or_default returns ConfigLoadOutcome::RecoveredFromCorrupt { backup_path, config }
    // Assert backup_path ends with `config.toml.bak` and exists on disk
    // Assert config == AppConfig::default()
}

// cache.rs tests
#[test]
fn translation_cache_hits_same_text_and_langs_and_evicts_lru() {
    let mut cache = TranslationCache::new(2);
    cache.insert("ja", "vi", "おはよう", "Chào buổi sáng".into());
    cache.insert("ja", "vi", "ありがとう", "Cảm ơn".into());
    assert_eq!(cache.get("ja", "vi", "おはよう").map(str::to_owned), Some("Chào buổi sáng".into()));
    assert_eq!(cache.get("en", "vi", "おはよう"), None);
    cache.insert("ja", "vi", "さようなら", "Tạm biệt".into());
    assert_eq!(cache.get("ja", "vi", "ありがとう"), None); // evicted
}
```

- [ ] **Step 3: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-core`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 4: Implement `sts-core` modules**

- `geometry.rs`: Implement `PhysicalRect { x: u32, y: u32, width: u32, height: u32 }`, `LogicalRect { x: f32, y: f32, width: f32, height: f32 }`, `MonitorGeometry { id: u32, name: String, x: i32, y: i32, width: u32, height: u32, scale_factor: f32, is_primary: bool }`, `normalize_drag_rect(start_x: f32, start_y: f32, end_x: f32, end_y: f32, viewport_w: f32, viewport_h: f32) -> LogicalRect`, and `logical_to_physical_crop(rect: LogicalRect, monitor: &MonitorGeometry) -> PhysicalRect` (clamping `scale_factor.max(0.1)` and keeping `x + width <= monitor.width`, `y + height <= monitor.height`).
- `types.rs`: Implement `CaptureFrame { width: u32, height: u32, scale_factor: f32, rgba: Vec<u8> }`, `OcrLine { bbox: PhysicalRect, text: String, confidence: f32, is_vertical: bool }`, `TextBlock { bbox: PhysicalRect, text: String, is_vertical: bool }`, `TranslatedBlock { bbox: PhysicalRect, original_text: String, translated_text: String, bg_rgba: [u8; 4] }`, and serde-enabled enums `OcrLanguagePack`, `OcrModelTier`, `ExecutionDevice` (`Auto`, `Cpu`, `DirectMl` — NO `Cuda`), `LiveDisplayMode`, `ReadingDirection`, `VisualSearchEngine`, `UiLanguage`.
- `config.rs`: Implement `AppConfig` with `#[serde(default)]` on every struct so partial TOML backfills cleanly, plus `ConfigStore::new(path: PathBuf)`, `ConfigStore::default_os_path() -> Result<PathBuf, CoreError>` (via `directories::ProjectDirs::from("dev", "square-to-search", "square-to-search")`), `ConfigStore::load_or_default(&self) -> Result<ConfigLoadOutcome, CoreError>`, and atomic `ConfigStore::save(&self, config: &AppConfig) -> Result<(), CoreError>`.
- `cache.rs`: Implement `TranslationCache` backed by `lru::LruCache<u64, String>` hashing `(source_lang, target_lang, text)` via `sha2::Sha256` truncated to `u64` (deterministic across runs) with `NonZeroUsize` capacity clamped to `>= 1`.

- [ ] **Step 5: Run tests and lint to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-core; cargo clippy --workspace --all-targets -- -D warnings`
Expected: All `sts-core` tests PASS, zero clippy warnings.

- [ ] **Step 6: Commit**

Run: `git add Cargo.toml Cargo.lock crates/; git commit -m "feat(core): add workspace skeleton, geometry, config, and LRU cache"` (skip commit with clear note if git `user.name`/`user.email` is not set).

---

### Task 2: `sts-platform` (Display Env, Screen Capture, Clipboard, Hotkeys, System Tray)

**Files:**
- Modify: `crates/sts-platform/Cargo.toml`, `crates/sts-platform/src/lib.rs`
- Create: `crates/sts-platform/src/display_env.rs`, `crates/sts-platform/src/capture.rs`, `crates/sts-platform/src/clipboard.rs`, `crates/sts-platform/src/hotkeys.rs`, `crates/sts-platform/src/tray.rs`
- Test: unit tests in `crates/sts-platform/src/{display_env,capture,hotkeys}.rs`

**Interfaces:**
- Consumes: `sts_core::geometry::{MonitorGeometry, PhysicalRect}`, `sts_core::types::CaptureFrame`
- Produces:
  - `sts_platform::PlatformError`
  - `sts_platform::display_env::{DisplayServer, WaylandCompositor, detect_display_server, detect_display_server_from_env}`
  - `sts_platform::capture::{ScreenCapturer, XcapCapturer, crop_frame}`
  - `sts_platform::clipboard::{ClipboardService, ArboardClipboard}`
  - `sts_platform::hotkeys::{HotkeyAction, HotkeyBinding, parse_hotkey_binding, HotkeyManager, GlobalHotkeyService}`
  - `sts_platform::tray::{TrayCommand, TrayService}`

- [ ] **Step 1: Add dependencies to `crates/sts-platform/Cargo.toml`**

Depend on `sts-core`, `xcap`, `arboard`, `global-hotkey`, `thiserror`, `tracing`, `image`, plus target-scoped tray dependencies:
- `[target.'cfg(target_os = "windows")'.dependencies]`: `tray-icon = { workspace = true }`
- `[target.'cfg(target_os = "linux")'.dependencies]`: `tray-icon = { workspace = true, features = ["ksni"] }`

- [ ] **Step 2: Write failing tests for display detection, frame cropping, and hotkey parsing**

```rust
// display_env.rs tests
#[test]
fn detects_gnome_wayland_and_disables_windowed_inplace_flag() {
    let ds = detect_display_server_from_env("linux", Some("wayland"), Some("wayland-0"), Some("ubuntu:GNOME"));
    assert_eq!(ds, DisplayServer::LinuxWayland { compositor: WaylandCompositor::Gnome });
    assert!(!ds.supports_wayland_layer_shell());
}

#[test]
fn detects_hyprland_wayland_supports_layer_shell() {
    let ds = detect_display_server_from_env("linux", Some("wayland"), Some("wayland-1"), Some("Hyprland"));
    assert_eq!(ds, DisplayServer::LinuxWayland { compositor: WaylandCompositor::Hyprland });
    assert!(ds.supports_wayland_layer_shell());
}

// capture.rs tests
#[test]
fn crop_frame_extracts_exact_subregion_rgba_and_rejects_zero_size() {
    // Build synthetic 4x4 RGBA frame where pixel (x, y) = [x as u8, y as u8, 0, 255]
    // Crop PhysicalRect { x: 1, y: 2, width: 2, height: 2 } -> assert 2x2 pixels match exact (1,2)..(2,3)
    // Crop zero width -> assert Err(PlatformError::InvalidCropRect)
}

// hotkeys.rs tests
#[test]
fn parses_spec_default_hotkeys_and_rejects_invalid_strings() {
    assert!(parse_hotkey_binding("Ctrl+Shift+S").is_ok());
    assert!(parse_hotkey_binding("Ctrl+Shift+T").is_ok());
    assert!(parse_hotkey_binding("Ctrl+Shift+H").is_ok());
    assert!(parse_hotkey_binding("NotAMod+???").is_err());
}
```

- [ ] **Step 3: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-platform`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 4: Implement `sts-platform` modules**

- `display_env.rs`: Implement `DisplayServer`, `WaylandCompositor` (`Gnome`, `Kde`, `Hyprland`, `Sway`, `Other`), `supports_wayland_layer_shell(&self) -> bool`, `supports_windowed_inplace(&self) -> bool`, and `detect_display_server()`.
- `capture.rs`: Implement `ScreenCapturer` trait (`list_monitors`, `capture_monitor(&self, monitor_id: u32)`, `capture_active_monitor(&self)`), `XcapCapturer` using `xcap::Monitor::all()` (picking monitor containing cursor / primary fallback for `capture_active_monitor`, satisfying GNOME Wayland active-monitor focus rule), and pure function `crop_frame(frame: &CaptureFrame, rect: PhysicalRect) -> Result<CaptureFrame, PlatformError>` clamping to frame bounds and returning `InvalidCropRect` if resulting width or height is 0.
- `clipboard.rs`: Implement `ClipboardService` trait (`set_text(&self, text: &str)`, `set_image_rgba(&self, width: usize, height: usize, rgba: &[u8])`) + `ArboardClipboard` wrapping `arboard::Clipboard` inside a `Mutex` (lazy-init on first call so headless CI tests don't panic).
- `hotkeys.rs`: Implement `parse_hotkey_binding(spec: &str) -> Result<global_hotkey::hotkey::HotKey, PlatformError>` supporting `Ctrl`/`Alt`/`Shift`/`Super` + `A..Z`, `0..9`, `F1..F12`, plus `GlobalHotkeyService` wrapping `global_hotkey::GlobalHotKeyManager` and forwarding `GlobalHotKeyEvent` to a `tokio::sync::mpsc::UnboundedSender<HotkeyAction>`.
- `tray.rs`: Implement `TrayService` creating the system tray icon with 4 menu items (*Square to Search*, *Live Translate*, *Settings*, *Quit*) forwarding `MenuEvent` to a `tokio::sync::mpsc::UnboundedSender<TrayCommand>`.

- [ ] **Step 5: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-platform; cargo clippy --workspace --all-targets -- -D warnings`
Expected: All `sts-platform` tests PASS, zero clippy warnings.

- [ ] **Step 6: Commit**

Run: `git add crates/sts-platform Cargo.lock; git commit -m "feat(platform): add display env detection, capture, clipboard, hotkeys, and tray"`

---

### Task 3: `sts-ocr` (Manifest, SHA-256 Downloader, Offline Import, PP-OCRv5 ONNX & Idle Unload)

**Files:**
- Modify: `crates/sts-ocr/Cargo.toml`, `crates/sts-ocr/src/lib.rs`
- Create: `crates/sts-ocr/src/manifest.rs`, `crates/sts-ocr/src/downloader.rs`, `crates/sts-ocr/src/engine.rs`
- Test: unit/async tests in `crates/sts-ocr/src/{manifest,downloader,engine}.rs`

**Interfaces:**
- Consumes: `sts_core::types::{CaptureFrame, OcrLine, OcrLanguagePack, OcrModelTier, ExecutionDevice}`, `sts_core::geometry::PhysicalRect`
- Produces:
  - `sts_ocr::OcrError` (`ModelNotInstalled`, `ChecksumMismatch`, `DownloadFailed`, `InvalidOfflineDir`, `InferenceFailed`, `Io`)
  - `sts_ocr::manifest::{ModelArtifact, ModelPackSpec, PackInstallStatus, ModelManifest, ModelStore}`
  - `sts_ocr::downloader::{DownloadProgress, ModelDownloader}`
  - `sts_ocr::engine::{OcrEngine, OarOcrEngine, IdleManagedOcrEngine}`

- [ ] **Step 1: Add dependencies to `crates/sts-ocr/Cargo.toml`**

Depend on `sts-core`, `oar-ocr` (default features on Linux; `features = ["directml"]` under `[target.'cfg(target_os = "windows")'.dependencies]` — **never** `cuda`), `reqwest`, `sha2`, `hex`, `tokio`, `image`, `directories`, `thiserror`, `tracing`, plus dev-dependencies `wiremock`, `tempfile`.

- [ ] **Step 2: Write failing tests for manifest, SHA-256 downloader, offline import, and idle unload**

```rust
// downloader.rs tests (wiremock + tempfile)
#[tokio::test]
async fn downloader_verifies_sha256_reports_progress_and_atomically_installs() {
    // Serve 3 small mock artifacts (det, rec, dict) via wiremock with matching SHA-256 in custom ModelPackSpec
    // Assert DownloadProgress callback fires with final bytes_downloaded == total_bytes
    // Assert all 3 files exist in ModelStore::pack_dir and no `.part` files remain
}

#[tokio::test]
async fn downloader_rejects_corrupt_checksum_removes_part_and_preserves_existing_file() {
    // Pre-populate an existing valid file at destination, then serve wrong bytes from wiremock
    // Assert download_pack returns Err(OcrError::ChecksumMismatch { .. })
    // Assert no `.part` file remains and pre-existing valid file bytes are unchanged
}

// manifest.rs tests
#[test]
fn offline_import_validates_required_files_and_copies_into_store() {
    // Missing dict file -> Err(OcrError::InvalidOfflineDir(_))
    // Complete det/rec/dict dir -> Ok(()) and store.is_installed(pack, tier) == true
}

// engine.rs tests (using FakeOcrSession + tokio::time::pause())
#[tokio::test]
async fn idle_managed_engine_unloads_session_after_idle_timeout() {
    tokio::time::pause();
    // Run recognize once -> assert loaded_sessions_count() == 1
    // Advance time by 301s (past 300s default idle timeout) & invoke sweep_idle()
    // Assert loaded_sessions_count() == 0
}
```

- [ ] **Step 3: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-ocr`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 4: Implement `sts-ocr` modules**

- `manifest.rs`: Define `ModelManifest::builtin()` listing the 6 packs (`CjkEn`, `Korean`, `VietnameseLatin` × `Mobile`, `Server`) with pinned PP-OCRv5 ONNX artifact filenames (`det.onnx`, `rec.onnx`, `dict.txt`), upstream Hugging Face / `oar-ocr` release URLs, approximate sizes, and SHA-256 checksums; implement `ModelStore` (`new(root: PathBuf)`, `default_os_dir()`, `pack_dir(pack, tier)`, `is_installed(pack, tier)`, `list_statuses(&self, manifest: &ModelManifest)`, `delete_pack(pack, tier)`, `import_offline_dir(pack, tier, src_dir: &Path)`).
- `downloader.rs`: Implement `ModelDownloader::download_pack(&self, store: &ModelStore, spec: &ModelPackSpec, on_progress: impl Fn(DownloadProgress) + Send + Sync)` streaming via `reqwest` bytes stream into `<dest>.part`, hashing incrementally with `sha2::Sha256`, deleting `<dest>.part` on error or checksum mismatch, and renaming atomically only after SHA-256 matches.
- `engine.rs`:
  - Implement `OcrEngine` async trait (`recognize(&self, frame: &CaptureFrame, pack: OcrLanguagePack, tier: OcrModelTier) -> Result<Vec<OcrLine>, OcrError>`).
  - Implement `OarOcrEngine` constructing `oar_ocr::prelude::OAROCRBuilder::new(det_path, rec_path, dict_path)` from `ModelStore` (returning `OcrError::ModelNotInstalled(pack, tier)` if absent), selecting DirectML + CPU fallback on Windows when `ExecutionDevice::Auto | DirectMl`, CPU on Linux, running inference inside `tokio::task::spawn_blocking` so Tokio workers never block, converting RGBA `CaptureFrame` to `image::RgbImage`, and mapping detected polygons to `OcrLine { bbox, text, confidence, is_vertical }` (`is_vertical = bbox.height > bbox.width * 1.4`).
  - Implement `IdleManagedOcrEngine<B>` tracking `last_used: Instant` per loaded `(OcrLanguagePack, OcrModelTier)` session and unloading idle sessions after `idle_timeout: Duration` (default 300s) via background timer or explicit `sweep_idle()`.

- [ ] **Step 5: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-ocr; cargo clippy --workspace --all-targets -- -D warnings`
Expected: All `sts-ocr` tests PASS, zero clippy warnings.

- [ ] **Step 6: Commit**

Run: `git add crates/sts-ocr Cargo.lock; git commit -m "feat(ocr): add PP-OCRv5 manifest, SHA-256 downloader, OAR-OCR engine, and idle unload"`

---

### Task 4: `sts-translate` (Batch Packing, Google Web + Bing Web Fallback, Cached Service)

**Files:**
- Modify: `crates/sts-translate/Cargo.toml`, `crates/sts-translate/src/lib.rs`
- Create: `crates/sts-translate/src/batch.rs`, `crates/sts-translate/src/google_web.rs`, `crates/sts-translate/src/bing_web.rs`, `crates/sts-translate/src/service.rs`
- Test: unit/async tests in `crates/sts-translate/src/{batch,google_web,service}.rs`

**Interfaces:**
- Consumes: `sts_core::cache::TranslationCache`
- Produces:
  - `sts_translate::TranslateError` (`RateLimited`, `Network`, `InvalidResponse`, `UnsupportedLanguage`)
  - `sts_translate::{TranslateRequest, TranslateResponse, Translator, VisionAiProvider}`
  - `sts_translate::batch::{pack_batch, unpack_batch}`
  - `sts_translate::google_web::GoogleWebTranslator`
  - `sts_translate::bing_web::BingWebTranslator`
  - `sts_translate::service::CachedFallbackTranslator`

- [ ] **Step 1: Add dependencies to `crates/sts-translate/Cargo.toml`**

Depend on `sts-core`, `reqwest`, `serde`, `serde_json`, `tokio`, `thiserror`, `tracing`, `url`; dev-dependency `wiremock`.

- [ ] **Step 2: Write failing tests for batch packing, Google JSON parsing, cache hits, and 429 → Bing fallback**

```rust
// batch.rs tests
#[test]
fn pack_and_unpack_avoids_collision_when_input_contains_separator() {
    let inputs = vec!["Line 1 |||STS_SEP||| inside", "Line 2"];
    let packed = pack_batch(&inputs);
    let unpacked = unpack_batch(&packed.payload, &packed.delimiter, inputs.len()).unwrap();
    assert_eq!(unpacked, vec!["Line 1 |||STS_SEP||| inside", "Line 2"]);
}

// service.rs tests (wiremock)
#[tokio::test]
async fn cached_fallback_uses_cache_for_repeats_and_falls_back_to_bing_on_google_429() {
    // Mock Google server returning 200 JSON for first batch ["Hello"], assert translated to "Xin chào"
    // Request ["Hello"] again -> assert Google received only 1 request total (cache hit)
    // Switch Google mock to return HTTP 429, configure Bing mock to return 200 JSON for ["World"] -> "Thế giới"
    // Assert translate_batch(["World"]) returns ["Thế giới"] via Bing fallback
}

#[tokio::test]
async fn returns_rate_limited_when_both_google_and_bing_return_429() {
    // Both Google and Bing mocks return 429 -> assert matches!(res, Err(TranslateError::RateLimited(_)))
}
```

- [ ] **Step 3: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-translate`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 4: Implement `sts-translate` modules**

- `lib.rs`: Define `TranslateRequest { source_lang: String, target_lang: String, texts: Vec<String> }`, `TranslateResponse { translations: Vec<String>, provider_name: String }`, `Translator` trait (using `Pin<Box<dyn Future<...>>>` or `async_trait`-free RPITIT/boxed future for `dyn Translator`), and `VisionAiProvider` trait stub for Milestone 3.
- `batch.rs`: Implement `pack_batch(texts: &[&str]) -> PackedBatch` picking a separator token not present in any input item (incrementing suffix until collision-free) and `unpack_batch(translated: &str, delimiter: &str, expected_len: usize) -> Result<Vec<String>, TranslateError>` trimming surrounding whitespace per item and falling back gracefully if upstream alters spacing around the delimiter.
- `google_web.rs`: Implement `GoogleWebTranslator::with_base_url(base_url: String)` (default `https://translate.googleapis.com`) querying `/translate_a/single?client=gtx&sl=...&tl=...&dt=t&q=...`, parsing `response[0]` segment pairs into joined text, detecting HTTP `429` or non-JSON HTML CAPTCHA as `TranslateError::RateLimited`.
- `bing_web.rs`: Implement `BingWebTranslator::with_base_url(base_url: String)` (supporting configurable endpoint for tests and Bing web translator token + `/ttranslatev3` call, mapping `429` to `TranslateError::RateLimited`).
- `service.rs`: Implement `CachedFallbackTranslator` checking `TranslationCache` under a `tokio::sync::Mutex` (never held across `.await`!), batching only cache-miss texts into one provider request, falling back from primary to secondary on `RateLimited`/`Network`/`InvalidResponse`, populating cache with results, and reassembling original indices.

- [ ] **Step 5: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-translate; cargo clippy --workspace --all-targets -- -D warnings`
Expected: All `sts-translate` tests PASS, zero clippy warnings.

- [ ] **Step 6: Commit**

Run: `git add crates/sts-translate Cargo.lock; git commit -m "feat(translate): add free Google/Bing web translators, collision-safe batching, and LRU cache"`

---

### Task 5: `sts-search` (Google Lens / Yandex / Bing Direct Upload + Clipboard Fallback)

**Files:**
- Modify: `crates/sts-search/Cargo.toml`, `crates/sts-search/src/lib.rs`
- Test: async tests in `crates/sts-search/src/lib.rs`

**Interfaces:**
- Consumes: `sts_core::types::{CaptureFrame, VisualSearchEngine}`, `sts_platform::clipboard::ClipboardService`
- Produces:
  - `sts_search::SearchError`
  - `sts_search::BrowserOpener`, `sts_search::SystemBrowserOpener`
  - `sts_search::VisualSearchOutcome` (`OpenedResultUrl { engine, url }`, `FallbackClipboard { engine, fallback_url, reason }`)
  - `sts_search::VisualSearchService`

- [ ] **Step 1: Add dependencies to `crates/sts-search/Cargo.toml`**

Depend on `sts-core`, `sts-platform`, `reqwest`, `image`, `open`, `tokio`, `thiserror`, `tracing`; dev-dependency `wiremock`.

- [ ] **Step 2: Write failing tests for Lens upload success, upload failure → clipboard fallback, and disabled fallback**

```rust
#[tokio::test]
async fn google_lens_direct_upload_opens_redirect_url_without_touching_clipboard() {
    // Wiremock POST /v3/upload responds 302 Location: https://lens.google.com/search?ep=ccm&p=abc123
    // Assert outcome == VisualSearchOutcome::OpenedResultUrl { engine: GoogleLens, url: "https://lens.google.com/search?ep=ccm&p=abc123" }
    // Assert mock browser opened that exact URL and mock clipboard received 0 image writes
}

#[tokio::test]
async fn google_lens_failure_copies_crop_to_clipboard_and_opens_fallback_page() {
    // Wiremock POST /v3/upload responds 503 Service Unavailable
    // Assert outcome is VisualSearchOutcome::FallbackClipboard { engine: GoogleLens, fallback_url: "https://lens.google.com", .. }
    // Assert mock clipboard received RGBA crop bytes and mock browser opened "https://lens.google.com"
}

#[tokio::test]
async fn returns_error_when_upload_fails_and_clipboard_fallback_disabled() {
    // Same 503 with enable_clipboard_fallback = false -> assert Err(SearchError::UploadFailed(_))
}
```

- [ ] **Step 3: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-search`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 4: Implement `crates/sts-search/src/lib.rs`**

- Encode `CaptureFrame` RGBA buffer to PNG in memory using `image::codecs::png::PngEncoder`.
- Configure `reqwest::Client` with `reqwest::redirect::Policy::none()` and a 15-second timeout so `302`/`303` `Location` headers (or Yandex JSON image URL / Bing visual search URL) are captured directly without downloading full result HTML.
- Support configurable endpoint base URLs for deterministic `wiremock` tests while defaulting to `https://lens.google.com`, `https://yandex.com`, and `https://www.bing.com`.
- On any upload/parse failure: if `enable_clipboard_fallback` is true, call `clipboard.set_image_rgba(...)` + `browser.open_url(landing_url)` and return `Ok(VisualSearchOutcome::FallbackClipboard { .. })`; otherwise return `Err(SearchError::UploadFailed(..))`.

- [ ] **Step 5: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-search; cargo clippy --workspace --all-targets -- -D warnings`
Expected: All `sts-search` tests PASS, zero clippy warnings.

- [ ] **Step 6: Commit**

Run: `git add crates/sts-search Cargo.lock; git commit -m "feat(search): add direct visual search uploader with clipboard fallback"`

---

### Task 6: Slint UI Components (`snip-overlay.slint`, `first-run-ocr.slint`, `settings.slint`, `app.slint`)

**Files:**
- Modify: `crates/sts-app/Cargo.toml`
- Create: `crates/sts-app/build.rs`
- Create: `crates/sts-app/ui/app.slint`, `crates/sts-app/ui/snip-overlay.slint`, `crates/sts-app/ui/first-run-ocr.slint`, `crates/sts-app/ui/settings.slint`

**Interfaces:**
- Consumes: Slint `1.19` standard widgets & `export global` host interop pattern
- Produces:
  - Slint globals: `AppState` (screenshot `image`, selection rect, active card mode `none|ocr|translate|ai-unconfigured|loading|error`, OCR text, translated text, toast text, OCR pack selector, starter download progress, settings fields & model pack statuses `[ModelPackRow]`, Wayland notice banner) and `AppActions` (callbacks: `cancel-overlay`, `selection-changed`, `action-visual-search`, `action-copy-ocr`, `action-quick-translate`, `action-ask-ai`, `copy-result-text`, `search-text-google`, `switch-snip-ocr-pack`, `start-starter-download`, `dismiss-first-run`, `open-settings-tab`, `save-settings`, `download-model-pack`, `delete-model-pack`, `import-offline-model`)
  - Slint windows: `SnipOverlayWindow`, `FirstRunOcrWindow`, `SettingsWindow`

- [ ] **Step 1: Add Slint dependencies & `build.rs` to `crates/sts-app`**

Add `slint = { workspace = true }` and `[build-dependencies] slint-build = { workspace = true }` to `crates/sts-app/Cargo.toml`, and create `crates/sts-app/build.rs` calling `slint_build::compile("ui/app.slint")`.

- [ ] **Step 2: Implement `.slint` UI files following the project's `slint` skill**

- `ui/app.slint`: Export shared structs (`ModelPackRow`), `export global AppState`, `export global AppActions`, and re-export `SnipOverlayWindow`, `FirstRunOcrWindow`, `SettingsWindow`.
- `ui/snip-overlay.slint`: `no-frame: true`, `FocusScope` with `capture-key-pressed` handling `Key.Escape` → `AppActions.cancel-overlay()`, backdrop freeze-frame `Image`, dimmed scrim outside selection rect, `TouchArea` drag-to-draw + corner/edge drag resize, floating action bar clamped within window bounds (`clamp(8px, root.width - self.width - 8px)`) with 4 buttons (**Tìm ảnh**, **Copy chữ**, **Dịch nhanh**, **Hỏi AI**), and inline quick result card showing selectable `TextEdit`, OCR language quick-switch chips (`CJK+EN`, `Vi/Latin`, `Korean`), *Copy tất cả*, *Tìm trên Google*, and 1-click *Mở Settings → Dịch & AI* when Ask AI is clicked without an AI provider.
- `ui/first-run-ocr.slint`: Clean modal dialog explaining the one-time ~25 MB `PP-OCRv5 mobile (CJK + EN)` starter pack download, with 1-click *Tải ngay*, progress bar (`0..100%`), status message, and *Để sau* button.
- `ui/settings.slint`: 5-tab Settings window matching Spec §5.2.D:
  1. **Chung**: UI language (`Tiếng Việt`/`English`), autostart toggle, 3 global hotkey inputs.
  2. **Live Translate**: Display mode (`Phủ đè trực tiếp` / `Bảng dịch nổi`), source language, reading direction, target language, debounce ms, opacity/font size, plus conditional Wayland/GNOME explanatory notice banner.
  3. **OCR & Models**: Pack list (`CJK + EN`, `Korean`, `Vietnamese / Latin`) with `mobile`/`server` tier toggle, download/delete buttons, progress bar, offline folder import button, Square-to-Search default OCR language selector, Execution Device (`Auto`, `CPU`, `DirectML (Windows)` — **NO CUDA**), idle unload minutes (`5`).
  4. **Dịch & AI**: Transparent disclosure card for default free Google/Bing web translator + provider selector placeholders for Milestone 3 BYOK/Ollama.
  5. **Tìm ảnh**: Default engine (`Google Lens`, `Yandex`, `Bing`) + clipboard fallback toggle.

- [ ] **Step 3: Validate every `.slint` file with `slint-viewer --check` and `--screenshot`**

Run:
```powershell
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
.\.tools\bin\slint-viewer.exe --screenshot "target\slint-preview-settings.png" --component SettingsWindow --size 760x560 "crates\sts-app\ui\app.slint"
.\.tools\bin\slint-viewer.exe --screenshot "target\slint-preview-first-run.png" --component FirstRunOcrWindow --size 460x280 "crates\sts-app\ui\app.slint"
.\.tools\bin\slint-viewer.exe --screenshot "target\slint-preview-snip.png" --component SnipOverlayWindow --size 960x540 "crates\sts-app\ui\app.slint"
```
Inspect each rendered PNG via `read_file` to verify layout, text contrast, zero clipping/overlap, and clean Vietnamese typography. Fix any visual issues and re-render before continuing.

- [ ] **Step 4: Verify `sts-app` compiles generated Slint code**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; cargo check -p sts-app`
Expected: Compiles with zero errors.

- [ ] **Step 5: Commit**

Run: `git add crates/sts-app; git commit -m "feat(ui): add Milestone 1 Slint windows (snip overlay, first-run OCR, 5-tab settings)"`

---

### Task 7: `sts-app` Controller Orchestration, UI Bridge & End-to-End Integration Tests

**Files:**
- Modify: `crates/sts-app/Cargo.toml`, `crates/sts-app/src/main.rs`
- Create: `crates/sts-app/src/lib.rs`, `crates/sts-app/src/state.rs`, `crates/sts-app/src/ui_bridge.rs`
- Test: `crates/sts-app/tests/square_to_search_flow.rs`

**Interfaces:**
- Consumes: all public interfaces from `sts-core`, `sts-platform`, `sts-ocr`, `sts-translate`, `sts-search`, and Slint globals from Task 6
- Produces:
  - `sts_app::state::{AppCommand, UiEvent, AppServices, AppController}`
  - Runnable desktop binary `sts-app` (`square-to-search`)

- [ ] **Step 1: Write failing end-to-end integration tests in `crates/sts-app/tests/square_to_search_flow.rs`**

Construct `AppController` with deterministic in-memory fakes for `ScreenCapturer`, `ClipboardService`, `OcrEngine`, `Translator`, `VisualSearchService`, and a `tempfile::TempDir` `ConfigStore` + `ModelStore`, and test all 5 Milestone 1 user flows:

```rust
#[tokio::test]
async fn snip_visual_search_succeeds_even_when_no_ocr_model_installed() {
    // TriggerSquareToSearch -> Receive UiEvent::ShowSnipOverlay { frame }
    // RunVisualSearch(rect) -> Receive UiEvent::VisualSearchDone(OpenedResultUrl { .. }) & UiEvent::HideSnipOverlay
}

#[tokio::test]
async fn snip_copy_ocr_prompts_first_run_download_when_model_missing() {
    // TriggerSquareToSearch -> RunOcrCopy(rect) when ModelStore has no packs
    // Assert UiEvent::ShowFirstRunOcrModal is emitted (does not crash or silently fail)
}

#[tokio::test]
async fn snip_copy_ocr_extracts_text_and_shows_inline_card_when_model_present() {
    // With installed pack + FakeOcrEngine returning "Hello Rust"
    // Assert UiEvent::ShowInlineResult { ocr_text: "Hello Rust", translated_text: None, .. }
}

#[tokio::test]
async fn snip_quick_translate_runs_ocr_then_translator_and_shows_vietnamese() {
    // FakeOcrEngine -> "Hello", FakeTranslator -> "Xin chào"
    // Assert UiEvent::ShowInlineResult { ocr_text: "Hello", translated_text: Some("Xin chào"), .. }
}

#[tokio::test]
async fn snip_ask_ai_without_configured_provider_offers_settings_shortcut_and_save_persists_config() {
    // RunAskAi -> UiEvent::ShowAskAiUnconfigured
    // SaveSettings(updated_cfg) -> assert reloaded ConfigStore equals updated_cfg
}
```

- [ ] **Step 2: Run integration tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run -p sts-app --test square_to_search_flow`
Expected: FAIL (`not yet implemented` / missing module)

- [ ] **Step 3: Implement `state.rs`, `ui_bridge.rs`, and `main.rs`**

- `state.rs`: Implement `AppController` running on Tokio (`tokio::sync::mpsc::Receiver<AppCommand>` → emits `UiEvent`), storing the active freeze-frame `CaptureFrame` + `MonitorGeometry`, converting logical Slint selections with `logical_to_physical_crop` + `crop_frame`, guarding OCR/Translate commands with `ModelStore::is_installed` (emitting `ShowFirstRunOcrModal` + caching the pending action to auto-resume after starter download completes), opening `https://www.google.com/search?q=<urlencoded>` on `SearchTextGoogle`, handling `SaveSettings` via `ConfigStore::save`, and cancelling in-flight OCR/translation tasks cleanly via `CancellationToken` on `CancelOverlay`.
- `ui_bridge.rs`: Wire `AppActions` callbacks to `cmd_tx.send(...)` and drive `UiEvent` updates onto Slint windows via `slint::invoke_from_event_loop` with `Weak` window handles (converting `CaptureFrame` RGBA buffer to `slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(...))`).
- `main.rs`: Initialize `tracing_subscriber`, explicitly select `winit` backend (`slint::BackendSelector::new().backend_name("winit".into()).select()`), spawn Tokio multi-threaded runtime, instantiate real `XcapCapturer`, `ArboardClipboard`, `OarOcrEngine` wrapped in `IdleManagedOcrEngine` (default 300s idle timeout), `CachedFallbackTranslator`, `VisualSearchService`, `TrayService`, and `GlobalHotkeyService`, and start `slint::run_event_loop_until_quit()`.

- [ ] **Step 4: Run integration tests & full workspace tests to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run --workspace`
Expected: All unit and integration tests across all 6 crates PASS.

- [ ] **Step 5: Commit**

Run: `git add crates/sts-app Cargo.lock; git commit -m "feat(app): wire Square to Search controller, Slint UI bridge, tray, and hotkeys"`

---

### Task 8: Milestone 1 Full Verification, Lint, Formatting & Release Build Check

**Files:**
- Verify only (fix any issues found in earlier crates if clippy/fmt flags them)

**Interfaces:**
- Consumes: entire workspace from Tasks 1–7
- Produces: verified green Milestone 1 build ready for Milestone 2 planning

- [ ] **Step 1: Run formatting check**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; cargo fmt-check`
Expected: Exit code `0` with zero formatting diffs.

- [ ] **Step 2: Run strict Clippy across entire workspace**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; cargo lint`
Expected: Exit code `0`, zero warnings (`-D warnings`).

- [ ] **Step 3: Run full workspace test suite via `cargo-nextest`**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe run --workspace`
Expected: All tests PASS with 0 skipped/failed.

- [ ] **Step 4: Re-verify all `.slint` files & inspect release build**

Run:
```powershell
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo build --release -p sts-app
Get-ChildItem "target\release\sts-app.exe"
```
Expected: `slint-viewer --check` exits `0`, release binary `target\release\sts-app.exe` builds cleanly with zero warnings.

- [ ] **Step 5: Clean up any temporary preview screenshots in `target/` and verify `git status`**

Run: `git status --short`
Expected: Clean working tree (or only expected untracked setup files if git identity isn't configured yet).
