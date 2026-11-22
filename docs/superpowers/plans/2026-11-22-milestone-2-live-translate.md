# Milestone 2: Continuous Live Translate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the complete **Continuous Live Translate** workflow on top of Milestone 1's 6-crate Rust + Slint workspace: lightweight frame-diff + debounce sampling loop, comic speech-bubble clustering (horizontal + vertical Japanese RTL/LTR reading order), adaptive bubble backdrop & auto-fit Vietnamese typography, batched LRU-cached page translation, Windows `WDA_EXCLUDEFROMCAPTURE` + click-through In-place overlay, Linux X11 target-window capture + XShape click-through, movable translucent Floating Panel, Live mini-controls pill, and `Ctrl+Shift+T` / `Ctrl+Shift+H` live hotkeys.

**Architecture:** Pure domain algorithms (`sts-core::live` frame-diff/debounce and `sts-core::bubbles` geometric bubble clustering, background sampling, WCAG text contrast, font-size fitting) remain 100% OS-agnostic and unit-tested deterministically. `sts-platform` adds window capture (`xcap::Window`) and native overlay window styling (`windows-sys 0.61.2` on Windows, `x11rb 0.13.2` XShape on Linux X11, Wayland Decision 2A fallback). `sts-app` adds 3 Slint 1.19 windows (`LiveInPlaceWindow`, `LivePanelWindow`, `LiveControlsWindow`), reuses `SnipOverlayWindow` for Live region selection, and drives continuous capture/OCR/batch-translate asynchronously via `crates/sts-app/src/live.rs` without ever blocking the Slint UI thread.

**Tech Stack:** Existing Milestone 1 workspace stack (Rust `1.100.0`, Slint `1.19.0` with `features = ["raw-window-handle-06", "unstable-winit-030"]`, Tokio `1.54.1`, `xcap 0.9.10`, `oar-ocr 0.11.0`, `sts-translate` Google/Bing + LRU cache) + already-locked platform crates: `raw-window-handle 0.6.2`, `windows-sys 0.61.2` (`Win32_Foundation`, `Win32_UI_WindowsAndMessaging` on Windows), `x11rb 0.13.2` (`shape` feature on Linux).

**Spec:** `docs/superpowers/specs/2026-11-22-square-to-search-design.md`

## Global Constraints

- Rust toolchain pinned at `1.100.0` (`rust-toolchain.toml`), Slint `1.19.0+`.
- Targets: Windows 10/11 (`x86_64-pc-windows-msvc`) and Linux (`x86_64-unknown-linux-gnu`, X11 & Wayland). No macOS in v1.
- Anti-Goal #1 (Zero self-capture loop & zero overlay flicker): Windows overlays MUST apply `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` (`LiveInPlaceWindow`, `LivePanelWindow`, and `LiveControlsWindow` alike); Linux X11 Live capture uses target window capture (`capture_window`); Linux Wayland resolves In-place to Floating Panel cleanly until Milestone 3 adds fullscreen `wlr-layer-shell` (Decision 2A).
- Anti-Goal #3 (No scrolling spam): Frame-diff + debounce (default `350ms`, configurable via `AppConfig.live.debounce_ms`) MUST suppress OCR/translation while the user is actively scrolling and abort stale in-flight tasks on subsequent page flips.
- No CUDA anywhere (Decision 3A); no `unwrap()`/`expect()` outside tests; every `unsafe` Win32/X11 FFI block MUST carry a `// SAFETY:` comment; `thiserror` in libraries, `anyhow` only in `sts-app`.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `.tools/bin/cargo-nextest.exe nextest run --workspace`, and `.tools/bin/slint-viewer.exe --check` must all exit `0`.

## Review Focus

1. **Continuous scrolling without settling must NOT trigger OCR/translation:** Feeding changing frames every 100ms (faster than the `350ms` debounce) must emit `0` pipeline runs, and trigger **exactly once** after frames remain visually stable for `>= 350ms` *(pinned in Task 1 & Task 5)*.
2. **Rapid page flip cancels stale in-flight translation:** If page 1's OCR/translation is slow and the user flips to page 2 before page 1 returns, page 1's stale blocks MUST be dropped/aborted and never overwrite page 2 *(pinned in Task 5)*.
3. **Vertical Japanese manga multi-column bubble vs separate adjacent bubbles:** Adjacent vertical columns inside one manga speech bubble must merge in **right-to-left** column order without stray ASCII spaces, while two separate speech bubbles separated by whitespace/panel margins must stay distinct `TextBlock`s sorted in RTL reading order *(pinned in Task 2)*.
4. **Adaptive bubble backdrop & narrow-bubble Vietnamese font fitting:** Light/white speech bubbles select dark text, dark/black speech bubbles select light text (WCAG contrast), and long Vietnamese translations inside narrow vertical Japanese bubbles shrink smoothly down to `min_font_px` without producing `NaN`, zero, or negative font sizes *(pinned in Task 2)*.
5. **Live Translate without installed OCR pack & Wayland In-place fallback:** Triggering Live Translate when `config.ocr.live_pack` is missing must open `FirstRunOcrModal` (and resume Live selection once downloaded); requesting `LiveDisplayMode::InPlace` on Linux Wayland must automatically fall back to `FloatingPanel` with a Vietnamese explanatory status notice *(pinned in Task 3 & Task 5)*.

---

## File Structure

```text
square-to-search/
├── Cargo.toml                              # Add raw-window-handle, windows-sys, x11rb; add slint raw-window-handle-06 + unstable-winit-030
└── crates/
    ├── sts-core/src/
    │   ├── lib.rs                          # Re-export `live` and `bubbles` modules
    │   ├── live.rs                         # Downscaled pixel-grid frame diff + LiveDebounceTracker state machine
    │   └── bubbles.rs                      # Speech-bubble clustering (H/V CJK + RTL/LTR), border bg sampler, WCAG contrast, font fitter
    ├── sts-platform/
    │   ├── Cargo.toml                      # Add raw-window-handle + target-gated windows-sys / x11rb
    │   └── src/
    │       ├── lib.rs                      # Re-export overlay module & WindowGeometry
    │       ├── capture.rs                  # Add WindowGeometry, list_windows, capture_window on ScreenCapturer + XcapCapturer
    │       └── overlay.rs                  # OverlayStyle, OverlayWindowConfigurator, NativeOverlayConfigurator (Win32/X11), resolve_effective_live_mode
    └── sts-app/
        ├── Cargo.toml                      # Add raw-window-handle for passing window handles to NativeOverlayConfigurator
        ├── ui/
        │   ├── globals.slint               # Add LiveTranslatedBlockRow + Live AppState properties & AppActions callbacks
        │   ├── snip-overlay.slint          # Support `live-selecting-region` mode (confirm region directly for Live Translate)
        │   ├── live-inplace.slint          # Transparent click-through overlay rendering translated bubbles in-place
        │   ├── live-panel.slint            # Movable translucent floating side panel with original toggle, copy, rate-limit banner
        │   ├── live-controls.slint         # Compact always-clickable mini-control pill (Pause, Hide/Show, Mode switch, Reselect, Stop)
        │   └── app.slint                   # Re-export LiveTranslatedBlockRow, LiveInPlaceWindow, LivePanelWindow, LiveControlsWindow
        ├── src/
        │   ├── lib.rs                      # Export `live` module & new Slint generated types
        │   ├── live.rs                     # LiveTarget, LiveSessionHandle, async continuous capture -> diff/debounce -> OCR -> cluster -> batch translate loop
        │   ├── state.rs                    # Wire ToggleLiveTranslate, ConfirmLiveRegion, ToggleLivePause, ToggleLiveVisibility, SwitchLiveDisplayMode, ReselectLiveRegion, StopLiveTranslate
        │   ├── ui_bridge.rs                # Manage LiveInPlaceWindow, LivePanelWindow, LiveControlsWindow + apply NativeOverlayConfigurator & WinitWindowAccessor
        │   └── main.rs                     # Pass NativeOverlayConfigurator into UI bridge
        └── tests/
            └── live_translate_flow.rs      # End-to-end Live Translate integration tests with deterministic fakes + tokio::time::pause()
```

---

### Task 1: `sts-core::live` (Downscaled Frame-Diff & Debounce State Machine)

**Files:**
- Modify: `crates/sts-core/src/lib.rs`
- Create: `crates/sts-core/src/live.rs`
- Test: unit tests in `crates/sts-core/src/live.rs`

**Interfaces:**
- Consumes: `sts_core::types::CaptureFrame`
- Produces:
  - `sts_core::live::DEFAULT_MOTION_THRESHOLD: f32` (`0.015`, i.e., 1.5% normalized mean absolute luma difference)
  - `sts_core::live::DEFAULT_SAMPLE_INTERVAL_MS: u64` (`200`, ~5 Hz)
  - `sts_core::live::FrameSignature` (compact fixed-size 32×32 downscaled luma thumbnail `[u8; 1024]` + `(width, height)`)
  - `sts_core::live::FrameSignature::from_frame(frame: &CaptureFrame) -> Option<Self>`
  - `sts_core::live::FrameSignature::diff_ratio(&self, other: &Self) -> f32` (`0.0..=1.0`, `1.0` if dimensions differ)
  - `sts_core::live::DebounceDecision` (`NoChange`, `Scrolling`, `TriggerPipeline`)
  - `sts_core::live::LiveDebounceTracker::{new(debounce: Duration, motion_threshold: f32), set_paused(&mut self, bool), is_paused(&self) -> bool, reset(&mut self), observe(&mut self, sig: FrameSignature, now: Duration) -> DebounceDecision, mark_translated(&mut self, sig: FrameSignature)}`

- [ ] **Step 1: Write failing tests in `crates/sts-core/src/live.rs`**

```rust
#[test]
fn frame_signature_diff_is_zero_for_identical_and_high_for_changed_frames() {
    // 64x64 solid white vs solid white -> diff == 0.0
    // 64x64 solid white vs solid black -> diff == 1.0
    // 64x64 vs 32x32 different dimensions -> diff == 1.0
}

#[test]
fn continuous_scrolling_never_triggers_until_screen_settles_for_debounce_ms() {
    // Review Focus #1:
    // Feed 10 distinct frames at t = 0, 100, 200, ..., 900 ms with debounce = 350ms
    // Assert every call returns NoChange or Scrolling (0 TriggerPipeline decisions)
    // Feed identical frame at t = 1100ms (200ms stable < 350ms) -> Scrolling
    // Feed identical frame at t = 1250ms (350ms stable >= 350ms) -> TriggerPipeline
    // Mark translated; feed identical frame at t = 1600ms -> NoChange
}

#[test]
fn paused_tracker_suppresses_triggers_and_resumes_cleanly() {
    // Set paused = true -> distinct settled frames return NoChange
    // Set paused = false -> changed settled frame returns TriggerPipeline
}
```

- [ ] **Step 2: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-core`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 3: Implement `crates/sts-core/src/live.rs` and export from `crates/sts-core/src/lib.rs`**

- `FrameSignature::from_frame`: Reject zero dimensions or mismatched `rgba.len() != frame.expected_byte_len()`; sample a deterministic 32×32 grid across `(frame.width, frame.height)` converting RGB to integer luma `((77 * r + 150 * g + 29 * b) >> 8) as u8`.
- `diff_ratio`: If `(width, height)` differ return `1.0`; else sum `|a[i] - b[i]|` divided by `(1024.0 * 255.0)`.
- `LiveDebounceTracker`: Track `last_translated: Option<FrameSignature>`, `last_observed: Option<FrameSignature>`, `stable_since: Option<Duration>`, `paused: bool`. On `observe(sig, now)`:
  - If `paused`: update `last_observed = Some(sig)`, return `NoChange`.
  - If `last_observed` exists and `prev.diff_ratio(&sig) >= motion_threshold`: motion detected → set `last_observed = Some(sig)`, `stable_since = Some(now)`, return `Scrolling`.
  - Otherwise screen is still relative to `last_observed`: ensure `stable_since` is initialized to `now` if `None`; if `last_translated` exists and `translated.diff_ratio(&sig) < motion_threshold`: return `NoChange`; if `now.saturating_sub(stable_since) >= debounce`: set `last_observed = Some(sig)`, return `TriggerPipeline`, else return `Scrolling`.

- [ ] **Step 4: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-core; cargo clippy -p sts-core --all-targets -- -D warnings; cargo fmt -p sts-core -- --check`
Expected: All `sts-core` tests PASS, zero clippy/fmt issues.

- [ ] **Step 5: Commit** *(skip if git `user.name`/`user.email` unset)*

---

### Task 2: `sts-core::bubbles` (Speech-Bubble Clustering, Adaptive Background & Vietnamese Font Fitting)

**Files:**
- Modify: `crates/sts-core/src/lib.rs`
- Create: `crates/sts-core/src/bubbles.rs`
- Test: unit tests in `crates/sts-core/src/bubbles.rs`

**Interfaces:**
- Consumes: `sts_core::geometry::PhysicalRect`, `sts_core::types::{CaptureFrame, OcrLine, ReadingDirection, TextBlock}`
- Produces:
  - `sts_core::bubbles::ClusterConfig` (`min_confidence: f32`, `horizontal_gap_factor: f32`, `vertical_gap_factor: f32`, `band_tolerance_factor: f32`; `Default` tuned for manga/manhwa)
  - `sts_core::bubbles::cluster_ocr_lines(lines: &[OcrLine], direction: ReadingDirection) -> Vec<TextBlock>`
  - `sts_core::bubbles::cluster_ocr_lines_with_config(lines: &[OcrLine], direction: ReadingDirection, cfg: &ClusterConfig) -> Vec<TextBlock>`
  - `sts_core::bubbles::sample_bubble_background(frame: &CaptureFrame, bbox: PhysicalRect) -> [u8; 4]`
  - `sts_core::bubbles::contrasting_text_color(bg_rgba: [u8; 4]) -> [u8; 4]`
  - `sts_core::bubbles::compute_fitted_font_size(text: &str, logical_w: f32, logical_h: f32, base_font_px: f32, min_font_px: f32) -> f32`

- [ ] **Step 1: Write failing tests in `crates/sts-core/src/bubbles.rs`**

```rust
#[test]
fn clusters_vertical_japanese_bubble_right_to_left_without_merging_adjacent_bubble() {
    // Review Focus #3:
    // Bubble A (right side, x=400..470): 2 vertical columns at x=440 ("危ない") and x=405 ("逃げろ！")
    // Bubble B (left side, x=80..150): 2 vertical columns at x=120 ("大丈夫") and x=85 ("か？")
    // Assert 2 distinct TextBlocks returned in RTL order:
    //   blocks[0].text == "危ない逃げろ！" (Bubble A first, right-to-left columns joined with no stray ASCII space)
    //   blocks[1].text == "大丈夫か？" (Bubble B second)
}

#[test]
fn clusters_horizontal_comic_bubble_top_to_bottom_in_ltr_order() {
    // Left bubble (x=50, y=40 "Watch" + y=66 "out!") and Right bubble (x=320, y=45 "Run!")
    // Assert 2 blocks in LTR order: blocks[0].text == "Watch out!", blocks[1].text == "Run!"
}

#[test]
fn filters_empty_and_low_confidence_lines_and_auto_infers_reading_direction() {
    // Low confidence (0.1) & whitespace-only lines dropped; majority vertical -> RTL, majority horizontal -> LTR
}

#[test]
fn samples_adaptive_bubble_bg_selects_wcag_contrast_text_and_fits_long_vietnamese() {
    // Review Focus #4:
    // White frame border around bbox -> bg near [255,255,255,255] -> dark text [17,24,39,255]
    // Dark frame border around bbox -> bg near [20,20,24,255] -> light text [248,250,252,255]
    // Long 12-word Vietnamese sentence in narrow 70x140 logical box with base=16.0, min=9.0:
    //   fitted_size is finite, >= 9.0, < 16.0
    // Zero/negative dimensions -> safely returns min_font_px (no NaN/Inf)
}
```

- [ ] **Step 2: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-core`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 3: Implement `crates/sts-core/src/bubbles.rs` and export from `crates/sts-core/src/lib.rs`**

- Use union-find (disjoint set) over filtered `OcrLine` indices:
  - Merge two `is_vertical == true` lines if vertical projections overlap (or gap `<= 0.4 * avg_col_w`) AND horizontal gap `<= horizontal_gap_factor * max(w_a, w_b)` (`horizontal_gap_factor = 1.25`). Sort members by `x` descending (right-to-left), then `y` ascending; join without spaces for CJK characters, or space if both boundaries are ASCII alphanumeric.
  - Merge two `is_vertical == false` lines if horizontal projections overlap (or center-x within `0.6 * max(w_a, w_b)`) AND vertical gap `<= vertical_gap_factor * max(h_a, h_b)` (`vertical_gap_factor = 0.9`). Sort members by `y` ascending, then `x` ascending; join with `" "` for Latin/Korean/alphanumeric boundaries and `""` for CJK-to-CJK boundaries.
  - Compute union bounding box for each component.
  - Resolve `ReadingDirection::Auto`: `RightToLeft` if vertical blocks `> horizontal blocks`, else `LeftToRight`. Sort blocks by row bands (top-to-bottom where band tolerance is `band_tolerance_factor * median_block_height`), ordering within each band right-to-left (`RightToLeft`) or left-to-right (`LeftToRight`).
- `sample_bubble_background`: Sample perimeter ring 1–2px outside/along the edges of `bbox` clamped to frame bounds, take per-channel median R, G, B (robust against black text ink touching edges!), return `[r, g, b, 255]` (fallback `[255, 255, 255, 255]` if frame invalid).
- `contrasting_text_color`: Relative luminance `L = 0.2126*R + 0.7152*G + 0.0722*B` (`0..255`); return `[17, 24, 39, 255]` if `L >= 140.0`, else `[248, 250, 252, 255]`.
- `compute_fitted_font_size`: Guard non-finite/non-positive dimensions (`return min_font_px.max(1.0)`). Binary search or step down from `base_font_px` to `min_font_px` estimating wrapped line count (`chars_per_line = (inner_w / (font * 0.56)).max(1.0)`, `line_height = font * 1.28`) until `estimated_lines * line_height <= inner_h`.

- [ ] **Step 4: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-core; cargo clippy -p sts-core --all-targets -- -D warnings; cargo fmt -p sts-core -- --check`
Expected: All `sts-core` tests PASS, zero clippy/fmt warnings.

- [ ] **Step 5: Commit** *(skip if git identity unset)*

---

### Task 3: `sts-platform` (Target-Window Capture & Native Click-Through / Anti-Self-Capture Overlays)

**Files:**
- Modify: `Cargo.toml` (workspace root: add `raw-window-handle = "0.6.2"`, `windows-sys = { version = "0.61.2", features = ["Win32_Foundation", "Win32_UI_WindowsAndMessaging"] }`, `x11rb = { version = "0.13.2", features = ["shape"] }`, and update `slint = { version = "1.19.0", features = ["raw-window-handle-06", "unstable-winit-030"] }`)
- Modify: `crates/sts-platform/Cargo.toml`, `crates/sts-platform/src/lib.rs`, `crates/sts-platform/src/capture.rs`
- Create: `crates/sts-platform/src/overlay.rs`
- Test: unit tests in `crates/sts-platform/src/{capture,overlay}.rs`

**Interfaces:**
- Consumes: `sts_core::types::{CaptureFrame, LiveDisplayMode}`, `sts_platform::display_env::DisplayServer`, `raw_window_handle::RawWindowHandle`
- Produces:
  - `sts_platform::capture::WindowGeometry { pub id: u32, pub title: String, pub app_name: String, pub x: i32, pub y: i32, pub width: u32, pub height: u32, pub scale_factor: f32, pub is_minimized: bool }`
  - `ScreenCapturer::list_windows(&self) -> Result<Vec<WindowGeometry>, PlatformError>` (with default impl returning empty `Ok(vec![])` so existing Milestone 1 fake capturers keep compiling cleanly!)
  - `ScreenCapturer::capture_window(&self, window_id: u32) -> Result<(WindowGeometry, CaptureFrame), PlatformError>` (with default impl returning `Err(PlatformError::WindowNotFound(window_id))`, overridden by `XcapCapturer`)
  - `PlatformError::WindowNotFound(u32)`, `PlatformError::OverlayStyleFailed(String)`
  - `sts_platform::overlay::{OverlayStyle, OverlayWindowConfigurator, NativeOverlayConfigurator, EffectiveLiveMode, resolve_effective_live_mode}`

- [ ] **Step 1: Update workspace `Cargo.toml` and `crates/sts-platform/Cargo.toml`**

Add `raw-window-handle`, target-gated `windows-sys` (Windows) and `x11rb` (`features = ["shape"]`, Linux) to workspace `Cargo.toml` and `crates/sts-platform/Cargo.toml`, and add `features = ["raw-window-handle-06", "unstable-winit-030"]` to workspace `slint`.

- [ ] **Step 2: Write failing tests in `crates/sts-platform/src/overlay.rs` and `capture.rs`**

```rust
// overlay.rs tests
#[test]
fn resolve_effective_live_mode_honors_inplace_on_windows_and_x11() {
    assert_eq!(resolve_effective_live_mode(LiveDisplayMode::InPlace, DisplayServer::Windows).mode, LiveDisplayMode::InPlace);
    assert!(resolve_effective_live_mode(LiveDisplayMode::InPlace, DisplayServer::Windows).notice.is_none());
    assert_eq!(resolve_effective_live_mode(LiveDisplayMode::InPlace, DisplayServer::LinuxX11).mode, LiveDisplayMode::InPlace);
}

#[test]
fn resolve_effective_live_mode_falls_back_to_floating_panel_on_wayland() {
    // Review Focus #5:
    let gnome = DisplayServer::LinuxWayland { compositor: WaylandCompositor::Gnome };
    let eff = resolve_effective_live_mode(LiveDisplayMode::InPlace, gnome);
    assert_eq!(eff.mode, LiveDisplayMode::FloatingPanel);
    assert!(eff.notice.as_deref().is_some_and(|n| n.contains("Wayland")));
}

#[test]
fn overlay_style_presets_exclude_from_capture_for_all_live_windows() {
    let inplace = OverlayStyle::live_inplace();
    assert!(inplace.click_through && inplace.exclude_from_capture && inplace.always_on_top);
    let panel = OverlayStyle::live_panel();
    assert!(!panel.click_through && panel.exclude_from_capture && panel.always_on_top);
}
```

- [ ] **Step 3: Run tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-platform`
Expected: FAIL (`not yet implemented`)

- [ ] **Step 4: Implement `capture.rs` window methods & `overlay.rs` native styler**

- `capture.rs`: Add `WindowGeometry`, `list_windows`, and `capture_window` to `ScreenCapturer` (with sensible defaults) and implement them in `XcapCapturer` using `xcap::Window::all()` (filtering out minimized/zero-sized windows, syncing `WindowGeometry` width/height to captured physical framebuffer).
- `overlay.rs`:
  - Implement `OverlayStyle { click_through, exclude_from_capture, always_on_top }` with `live_inplace()` and `live_panel()` constructors.
  - Implement `EffectiveLiveMode { pub mode: LiveDisplayMode, pub notice: Option<String> }` and `resolve_effective_live_mode(requested, display)`.
  - Implement `OverlayWindowConfigurator` trait and `NativeOverlayConfigurator`:
    - Windows (`#[cfg(target_os = "windows")]`): Match `RawWindowHandle::Win32(h)`, validate non-zero `hwnd`, toggle `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE` via `GetWindowLongPtrW` / `SetWindowLongPtrW`, call `SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)` (`0x0000_0011`) when `exclude_from_capture` else `WDA_NONE` (`0`), and call `SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE)` when `always_on_top`. Document every `unsafe` block with `// SAFETY:`.
    - Linux (`#[cfg(target_os = "linux")]`): Match `RawWindowHandle::Xlib(h)` / `RawWindowHandle::Xcb(h)` and apply `x11rb::protocol::shape` `SK::INPUT` empty rectangle list when `click_through = true` (or clear shape mask when `false`); for `RawWindowHandle::Wayland`, return `Ok(())` cleanly (logged at debug).

- [ ] **Step 5: Run tests and clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-platform; cargo clippy --workspace --all-targets -- -D warnings; cargo fmt --all -- --check`
Expected: All `sts-platform` + workspace tests PASS, zero warnings.

- [ ] **Step 6: Commit** *(skip if git identity unset)*

---

### Task 4: Slint UI Windows for Live Translate (`live-inplace.slint`, `live-panel.slint`, `live-controls.slint`)

**Files:**
- Modify: `crates/sts-app/ui/globals.slint`, `crates/sts-app/ui/snip-overlay.slint`, `crates/sts-app/ui/app.slint`
- Create: `crates/sts-app/ui/live-inplace.slint`, `crates/sts-app/ui/live-panel.slint`, `crates/sts-app/ui/live-controls.slint`

**Interfaces:**
- Consumes: `Theme`, `AppState`, `AppActions` in `crates/sts-app/ui/globals.slint`
- Produces:
  - Struct `LiveTranslatedBlockRow { index: int, x: length, y: length, width: length, height: length, original-text: string, translated-text: string, bg-color: color, text-color: color, font-size: length }`
  - New `AppState` properties: `live-selecting-region: bool`, `live-active: bool`, `live-paused: bool`, `live-overlay-visible: bool`, `live-display-mode: string` (`"in-place" | "floating-panel"`), `live-can-inplace: bool`, `live-show-original-in-panel: bool`, `live-status-text: string`, `live-error-text: string`, `live-rate-limited: bool`, `live-blocks: [LiveTranslatedBlockRow]`
  - New `AppActions` callbacks: `live-confirm-region(length, length, length, length)`, `live-toggle-pause()`, `live-toggle-visibility()`, `live-switch-display-mode()`, `live-reselect-region()`, `live-stop()`, `live-copy-block(string)`
  - Exported windows in `app.slint`: `LiveInPlaceWindow`, `LivePanelWindow`, `LiveControlsWindow`

- [ ] **Step 1: Extend `globals.slint`, `snip-overlay.slint`, and `app.slint`**

- Add `LiveTranslatedBlockRow`, the new `AppState` properties (with clean realistic defaults so `slint-viewer --screenshot` previews look rich!), and the 7 new `AppActions` callbacks to `globals.slint`.
- Update `snip-overlay.slint`: When `AppState.live-selecting-region` is true, display a subtle top hint banner (`"Kéo chọn vùng đọc truyện cho Live Translate · Nhấn Esc để hủy"`), hide the 4-button Square to Search toolbar/card, and show a compact 2-button confirm bar (`"Bắt đầu Live Translate"` → `AppActions.live-confirm-region(...)`, `"Hủy"` → `AppActions.cancel-overlay()`) right below the dragged box.
- Re-export `LiveTranslatedBlockRow`, `LiveInPlaceWindow`, `LivePanelWindow`, `LiveControlsWindow` in `app.slint`.

- [ ] **Step 2: Create `live-inplace.slint`, `live-panel.slint`, and `live-controls.slint`**

- `live-inplace.slint`: `no-frame: true; background: #00000000; always-on-top: true;`. When `AppState.live-overlay-visible`, iterate `for block in AppState.live-blocks` placing each rounded `Rectangle` at `(block.x, block.y, block.width, block.height)` with `background: block.bg-color.with-alpha(AppState.live-overlay-opacity-pct / 100.0)` and centered wrapped `Text { text: block.translated-text; color: block.text-color; font-size: block.font-size; wrap: word-wrap; }`.
- `live-panel.slint`: Translucent always-on-top movable panel (`title: "Square to Search — Live Translate"`, `always-on-top: true`) showing header status pill (Live / Tạm dừng), toggle switch for `Hiện chữ gốc`, rate-limit / error warning banner with 1-click button `Mở Settings → Dịch & AI` (`AppActions.open-settings-tab(3)`), `ListView` (per `slint` skill for smooth scrolling) of numbered dialogue cards (`#1`, `#2`, ...) with translated Vietnamese text, optional muted original text, and 1-click `Copy` (`AppActions.live-copy-block(block.translated-text)`), plus clean empty state.
- `live-controls.slint`: Compact frameless always-on-top dark pill (`no-frame: true; background: #00000000; always-on-top: true`) with 5 crisp controls: `Tạm dừng`/`Tiếp tục`, `Ẩn`/`Hiện`, `Phủ đè`/`Bảng nổi` (with clear disabled visual when `!AppState.live-can-inplace`), `Chọn lại vùng`, and `Dừng` (danger accent).

- [ ] **Step 3: Validate with `slint-viewer --check` and inspect `--screenshot` renders**

Run:
```powershell
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
.\.tools\bin\slint-viewer.exe --screenshot "target\m2-live-inplace.png" --component LiveInPlaceWindow --size 720x540 "crates\sts-app\ui\app.slint"
.\.tools\bin\slint-viewer.exe --screenshot "target\m2-live-panel.png" --component LivePanelWindow --size 360x560 "crates\sts-app\ui\app.slint"
.\.tools\bin\slint-viewer.exe --screenshot "target\m2-live-controls.png" --component LiveControlsWindow --size 480x56 "crates\sts-app\ui\app.slint"
```
Inspect each PNG visually via `read_file`, fix any clipping/overlap/contrast defects, and re-verify `--check` exits `0`.

- [ ] **Step 4: Commit** *(skip if git identity unset)*

---

### Task 5: `sts-app` Live Translate Pipeline (`live.rs`), Controller/Bridge Wiring & E2E Integration Tests

**Files:**
- Modify: `crates/sts-app/Cargo.toml`, `crates/sts-app/src/lib.rs`, `crates/sts-app/src/state.rs`, `crates/sts-app/src/ui_bridge.rs`, `crates/sts-app/src/main.rs`
- Create: `crates/sts-app/src/live.rs`
- Create: `crates/sts-app/tests/live_translate_flow.rs`

**Interfaces:**
- Consumes: `sts_core::{live, bubbles}`, `sts_platform::{capture, overlay}`, `sts_ocr::engine::OcrEngine`, `sts_translate::Translator`, Task 4 Slint windows
- Produces:
  - `sts_app::live::{LiveTarget, LiveRenderedBlock, LiveLoopConfig, LiveSessionHandle, spawn_live_session}`
  - Extended `AppCommand` (`ConfirmLiveRegion(LogicalRect)`, `SelectLiveWindow(u32)`, `ToggleLivePause`, `SwitchLiveDisplayMode`, `ReselectLiveRegion`, `StopLiveTranslate`, `CopyLiveBlock(String)`) and wired `ToggleLiveTranslate` + `ToggleLiveVisibility`
  - Extended `UiEvent` (`ShowLiveRegionPicker { monitor, frame }`, `LiveSessionStarted { display_mode, can_inplace, overlay_monitor, overlay_physical_rect, notice }`, `LiveBlocksUpdated { blocks: Vec<LiveRenderedBlock>, status_text }`, `LivePauseChanged(bool)`, `LiveVisibilityChanged(bool)`, `LiveDisplayModeChanged(LiveDisplayMode)`, `LiveRateLimited { message }`, `LiveSessionStopped`)

- [ ] **Step 1: Write failing end-to-end integration tests in `crates/sts-app/tests/live_translate_flow.rs`**

Using deterministic fakes + `tokio::time::pause()`:

```rust
#[tokio::test]
async fn live_translate_prompts_first_run_download_when_live_pack_missing_then_resumes() {
    // Review Focus #5:
    // ToggleLiveTranslate when CjkEn pack missing -> UiEvent::ShowFirstRunOcrModal
    // StartStarterDownload -> downloads pack -> automatically emits UiEvent::ShowLiveRegionPicker
}

#[tokio::test]
async fn live_loop_ignores_rapid_scrolling_triggers_once_after_debounce_and_batches_page_bubbles() {
    // Review Focus #1 + batch translation:
    // ConfirmLiveRegion with FakeCapturer cycling changing frames every 100ms for 500ms, then settling
    // Assert 0 OCR/translate calls during scroll; after advancing >= 350ms stable, exactly 1 OCR call & 1 batch translate call
    // Assert UiEvent::LiveBlocksUpdated contains clustered & translated Vietnamese blocks in RTL manga order
}

#[tokio::test]
async fn rapid_page_flip_discards_stale_slow_translation_and_displays_newest_page() {
    // Review Focus #2:
    // Page 1 translator blocks on a Notify; user flips to Page 2 and settles -> Page 2 completes fast
    // Release Page 1 slow future -> assert stale Page 1 blocks are NEVER emitted after Page 2
}

#[tokio::test]
async fn live_controls_toggle_pause_visibility_display_mode_and_stop_cleanly() {
    // ToggleLivePause -> LivePauseChanged(true) suppresses capture pipeline
    // ToggleLiveVisibility -> LiveVisibilityChanged(false) -> LiveVisibilityChanged(true)
    // SwitchLiveDisplayMode -> LiveDisplayModeChanged(FloatingPanel)
    // StopLiveTranslate -> LiveSessionStopped (and background loop task terminates)
}

#[tokio::test]
async fn live_rate_limit_emits_actionable_banner_event_without_crashing() {
    // FakeTranslator returns TranslateError::RateLimited -> UiEvent::LiveRateLimited emitted; loop stays alive
}
```

- [ ] **Step 2: Run integration tests to verify RED**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run -p sts-app --test live_translate_flow`
Expected: FAIL (`not yet implemented` / missing module)

- [ ] **Step 3: Implement `crates/sts-app/src/live.rs`, wire `state.rs`, `ui_bridge.rs`, and `main.rs`**

- `live.rs`:
  - `LiveTarget::ScreenRegion { monitor: MonitorGeometry, crop_physical: PhysicalRect, crop_logical: LogicalRect }` and `LiveTarget::Window { window_id: u32 }`.
  - `LiveRenderedBlock { index: usize, logical_rect: LogicalRect, original_text: String, translated_text: String, bg_rgba: [u8; 4], text_rgba: [u8; 4], font_size: f32 }`.
  - `spawn_live_session(...) -> LiveSessionHandle`: Runs a `tokio::time::interval(sample_interval)` loop (uses `tokio::time::Instant` so `tokio::time::pause()` works deterministically!). Tracks monotonic `generation: u64`; whenever `LiveDebounceTracker::observe` returns `TriggerPipeline`, increments `generation`, aborts any previous in-flight pipeline `JoinHandle`, and spawns the OCR → `cluster_ocr_lines` → single batched `translator.translate(...)` → `sample_bubble_background` / `contrasting_text_color` / `compute_fitted_font_size` pipeline. Before emitting `UiEvent::LiveBlocksUpdated`, verifies `generation` is still current (`AtomicU64`) so stale results can NEVER overwrite a newer page!
- `state.rs`:
  - Add `display_server: DisplayServer` (injectable via `AppServices` for Wayland fallback testing, defaulting to `detect_display_server()` in `main.rs`).
  - Handle `ToggleLiveTranslate`: if live active → stop session; if `live_pack` not installed → remember `pending_live_start = true` and emit `ShowFirstRunOcrModal` (auto-resuming on `StartStarterDownload`); otherwise capture active monitor and emit `ShowLiveRegionPicker`.
  - Handle `ConfirmLiveRegion(rect)`: resolve effective mode via `resolve_effective_live_mode(self.config.live.display_mode, self.services.display_server)`, compute `PhysicalRect` via `logical_to_physical_crop`, spawn `LiveSessionHandle`, emit `LiveSessionStarted`.
  - Handle `ToggleLivePause`, `ToggleLiveVisibility` (also triggered by `Ctrl+Shift+H` hotkey!), `SwitchLiveDisplayMode` (respecting `can_inplace`), `ReselectLiveRegion`, `StopLiveTranslate`, `CopyLiveBlock`, and stop live session cleanly on `Quit`.
- `ui_bridge.rs` & `main.rs`:
  - Add `live_inplace: LiveInPlaceWindow`, `live_panel: LivePanelWindow`, `live_controls: LiveControlsWindow` to `UiWindows` and bind all 7 new `AppActions` callbacks.
  - On `LiveSessionStarted`: show `LiveControlsWindow` (applying `OverlayStyle::live_panel()` via `NativeOverlayConfigurator` so controls are excluded from capture on Windows!), and show either `LiveInPlaceWindow` (positioned at `(overlay_monitor.x + crop_physical.x, overlay_monitor.y + crop_physical.y)` with `(crop_physical.width, crop_physical.height)`, applying `OverlayStyle::live_inplace()` + `winit_window.set_cursor_hittest(false)` via `WinitWindowAccessor`) OR `LivePanelWindow` (applying `OverlayStyle::live_panel()`).
  - On `LiveBlocksUpdated`: convert `Vec<LiveRenderedBlock>` into `ModelRc<LiveTranslatedBlockRow>` and update `AppState.live_blocks` on all live windows.
  - On `LiveVisibilityChanged` / `LiveDisplayModeChanged` / `LiveSessionStopped`: toggle visibility / switch windows / hide all 3 live windows cleanly.
  - Keep existing Milestone 1 integration tests in `tests/square_to_search_flow.rs` 100% green!

- [ ] **Step 4: Run all tests & clippy to verify GREEN**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run --workspace; cargo clippy --workspace --all-targets -- -D warnings`
Expected: All Milestone 1 + Milestone 2 unit & integration tests PASS, zero warnings.

- [ ] **Step 5: Commit** *(skip if git identity unset)*

---

### Task 6: Milestone 2 Full Verification, Lint, Formatting & Release Build Check

**Files:**
- Verify only (fix any issues surfaced by strict checks)

**Interfaces:**
- Consumes: entire workspace from Tasks 1–5
- Produces: verified green Milestone 2 build ready for Milestone 3 planning

- [ ] **Step 1: Run formatting check**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; cargo fmt --all -- --check`
Expected: Exit code `0`.

- [ ] **Step 2: Run strict Clippy across entire workspace**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; cargo clippy --workspace --all-targets -- -D warnings`
Expected: Exit code `0`, zero warnings.

- [ ] **Step 3: Run full workspace test suite via `cargo-nextest`**

Run: `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH; .\.tools\bin\cargo-nextest.exe nextest run --workspace`
Expected: All tests PASS with 0 skipped/failed.

- [ ] **Step 4: Verify all `.slint` files & Windows release binary build**

Run:
```powershell
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo build --release -p sts-app
Get-ChildItem "target\release\sts-app.exe"
```
Expected: `slint-viewer --check` exits `0`, `target\release\sts-app.exe` builds cleanly.

- [ ] **Step 5: Clean temporary preview PNGs in `target/` & check `git status --short`**

Remove any `target\m2-*.png` preview screenshots and confirm clean workspace status.
