# Square to Search — Milestone 3 Implementation Plan (BYOK AI Streaming, OS Keyring, Wayland Advanced & Release Packaging)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans (or `/swarm` parallel waves) to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete Milestone 3 of `square-to-search`:
1. **OS Keyring Secret Storage (`keyring 3.6.3`):** Store BYOK API keys (`gemini`, `openai`, `deepl`) exclusively in Windows Credential Manager / Linux Secret Service — never plaintext in `config.toml`, never leaked in `Debug`/`tracing`, and never echoed back into Slint UI state.
2. **BYOK Translation & Multimodal Streaming Ask AI:** Implement `GeminiProvider`, `OpenAiCompatProvider`, `DeepLTranslator`, and local `OllamaProvider` with base64 PNG image support, UTF-8-safe chunked SSE/NDJSON streaming, LRU translation caching, custom manga prompt injection, live hot-reload from Settings without restart, and the interactive Ask AI streaming card (presets `"Giải thích"`, `"Tóm tắt"`, `"Dịch tự nhiên"`, custom prompt, streaming output, Stop/Cancel, Test Connection) in `SnipOverlayWindow` + Settings Tab 3.
3. **Linux Wayland `xdg-desktop-portal` + Fullscreen `wlr-layer-shell` In-place & HiDPI Hardening (Decision 2A):** Add `ashpd 0.14.3` portal adapters on Linux (`GlobalShortcuts`, `ScreenCast` PipeWire, `Screenshot` multi-monitor fallback), update `resolve_effective_live_mode_for_target` so KDE/Hyprland/Sway (`wlr-layer-shell`) enable In-place when the reader target is fullscreen/maximized while GNOME Wayland and floating Wayland windows stay on Floating Panel with notice, and harden negative-origin multi-monitor + fractional HiDPI (`100%/125%/150%/175%/200%`) coordinate math.
4. **Release Packaging, Dependency Audit & Final Verification:** Configure `deny.toml` (`cargo-deny 0.22.0`), create portable packaging scripts `scripts/package-windows.ps1` (resolving the `DirectML.dll` symlink and asserting zero CUDA DLLs) and `scripts/package-linux.sh` (bundle + AppDir + `.desktop`, zero CUDA `.so`), write `README.md`, and pass the full workspace verification gate.

**Architecture:** Extends the existing 6-crate Cargo workspace (`sts-core`, `sts-platform`, `sts-ocr`, `sts-translate`, `sts-search`, `sts-app`) cleanly while keeping strict crate boundaries:
- `sts-core` owns non-secret BYOK configuration defaults (`TranslateConfig`) and `SecretSlot` identifiers, plus OS-agnostic HiDPI/multi-monitor coordinate math.
- `sts-translate` stays platform-agnostic: receives resolved `ProviderSecrets` via `factory.rs`, implements `GeminiProvider`, `OpenAiCompatProvider`, `DeepLTranslator`, and `OllamaProvider`, extends `VisionAiProvider` with object-safe `ask_stream(..., UnboundedSender<String>)`, and wraps translators with LRU caching.
- `sts-platform` owns OS keyring (`secrets.rs` via `keyring 3.6.3`), Wayland portal adapters & capability descriptors (`wayland.rs` via `ashpd 0.14.3` on Linux), and fullscreen-aware `resolve_effective_live_mode_for_target` + `is_rect_fullscreen_or_maximized` (`overlay.rs`).
- `sts-app/ui` adds the streaming Ask AI card (`snip-overlay.slint`) and complete Tab 3 BYOK + masked OS Keyring UI (`settings.slint`, `globals.slint`).
- `sts-app` wires `SecretStore` (via `tokio::task::spawn_blocking`), live hot-reload of active `Translator` and `VisionAiProvider` on `SaveSettings`/`SaveApiKey`/`ClearApiKey`, cancellable `AskAi` streaming with PNG crop encoding, Wayland fullscreen `can_inplace` propagation, and E2E integration tests.
- `scripts/` + `deny.toml` + `README.md` deliver reproducible release packaging and dependency hygiene.

**Tech Stack:** Rust `1.100.0` (edition 2024), Slint `1.19.0`, Tokio `1.54.1`, `reqwest 0.13.5` (`stream` feature already enabled), `keyring 3.6.3` (`default-features = false`; Windows `windows-native`, Linux `async-secret-service` + `tokio` + `crypto-rust`), `base64 0.23.1`, `ashpd 0.14.3` (Linux-only), `wiremock 0.6.5`, `cargo-nextest 0.9.152`, `cargo-deny 0.22.0`, `slint-viewer 1.19.0`.

**Spec:** `docs/superpowers/specs/2026-11-22-square-to-search-design.md` (Decisions 1A, 2A, 3A; Sections 3.1, 4, 5, 6, 7).

---

## Global Constraints

- Rust toolchain pinned at `1.100.0` (`rust-toolchain.toml`), Slint `1.19.0+`.
- Targets: Windows 10/11 (`x86_64-pc-windows-msvc`) and Linux (`x86_64-unknown-linux-gnu`, X11 & Wayland). No macOS in v1.
- Zero `unwrap()` / `expect()` in production code (enforced by `[workspace.lints.clippy]`).
- `thiserror` in library crates (`sts-core`, `sts-platform`, `sts-translate`); `anyhow` only in `sts-app`.
- Explicit `// SAFETY:` comments on any `unsafe` block; avoid introducing new `unsafe`.
- Decision 3A strictly enforced: ZERO CUDA features, dependencies, UI options, DLLs, or `.so` files anywhere.
- Secrets NEVER written to `config.toml`, NEVER logged in `tracing` or `Debug`, and NEVER read back as plaintext into Slint `AppState`.
- On Windows PowerShell commands, prepend `$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH`, double-quote workspace paths containing spaces, and invoke nextest as `.\.tools\bin\cargo-nextest.exe nextest run ...`.
- Do not run `git commit` unless git `user.name`/`user.email` are configured by the user; if using `/swarm`, use `workspace: "inherit"` with strictly disjoint file ownership (git `HEAD` is currently unborn).

---

## Review Focus

1. **Secret Leakage into `config.toml`, Slint State, or Logs:** Pinned by `sts-core` TOML serialization assertion (no `api_key` field on `AppConfig`), `sts-platform` `Debug` redaction test on `MemorySecretStore` + `sts-translate` `ProviderSecrets` `Debug` redaction test, and `sts-app` E2E test checking `config.toml` bytes on disk never contain saved API keys.
2. **Chunked SSE / NDJSON Frame Splitting & Stale Deltas After Cancel:** Pinned by `sts-translate` `wiremock` tests splitting SSE/NDJSON lines and multi-byte Vietnamese UTF-8 sequences across TCP chunks, plus `sts-app` E2E test verifying `StopAskAi` and `CancelOverlay` abort in-flight streams with zero trailing `AskAiStreamDelta` events.
3. **Keyring Runtime Safety & Error Semantics:** Pinned by `sts-platform::secrets` tests verifying `NoEntry -> Ok(None)`, idempotent `delete_secret`, blank-secret rejection, and `sts-app` calling `SecretStore` via `tokio::task::spawn_blocking` so Linux `zbus`/`async-secret-service` never panics on nested `block_on` inside a Tokio worker thread.
4. **Wayland Decision 2A Boundary Regression:** Pinned by exhaustive matrix tests in `sts-platform::overlay` and E2E tests in `sts-app`: `InPlace` activates ONLY on `Kde`/`Hyprland`/`Sway` when `target_is_fullscreen_or_maximized == true`; `Gnome` Wayland and floating Wayland windows ALWAYS fall back to `FloatingPanel` with Vietnamese notice.
5. **Broken `DirectML.dll` SymbolicLink or Accidental CUDA Bundling in Release Archive:** `target\release\DirectML.dll` is a symbolic link to `%LOCALAPPDATA%\ort.pyke.io\...`. Pinned by `scripts/package-windows.ps1` resolving symlink targets, asserting `DirectML.dll` staged size `> 0`, asserting zero `*cuda*`/`*cudnn*`/`*cublas*` DLLs, and generating verified `.zip` + `.sha256`.

---

## File Structure

```text
square-to-search/
├── Cargo.toml                              # Add keyring (default-features=false), base64, ashpd
├── deny.toml                               # cargo-deny advisories, licenses, bans (incl. CUDA crate ban), sources
├── README.md                               # Project overview, feature guide, Wayland/Windows matrix, build & packaging
├── scripts/
│   ├── package-windows.ps1                 # Portable Windows ZIP bundler resolving DirectML.dll symlink + zero-CUDA check
│   └── package-linux.sh                    # Portable Linux tarball + AppDir/.desktop bundler + zero-CUDA check
└── crates/
    ├── sts-core/src/
    │   ├── lib.rs                          # Re-export KEYRING_SERVICE_NAME, SecretSlot, physical_to_logical_rect
    │   ├── config.rs                       # Add SecretSlot, KEYRING_SERVICE_NAME, non-secret BYOK fields on TranslateConfig
    │   └── geometry.rs                     # Add physical_to_logical_rect + fractional HiDPI (1.0..2.0) & negative-origin tests
    ├── sts-translate/
    │   ├── Cargo.toml                      # Add base64.workspace = true
    │   └── src/
    │       ├── lib.rs                      # Add AuthFailed/ProviderNotConfigured, ask_stream on VisionAiProvider, LineStreamDecoder
    │       ├── service.rs                  # Add CachedTranslator for single-provider BYOK LRU caching
    │       ├── gemini.rs                   # GeminiProvider (Translator + streaming VisionAiProvider via inline_data + alt=sse)
    │       ├── openai.rs                   # OpenAiCompatProvider (Translator + SSE streaming VisionAiProvider)
    │       ├── deepl.rs                    # DeepLTranslator (Free & Pro endpoints + DeepL-Auth-Key)
    │       ├── ollama.rs                   # OllamaProvider (Translator + NDJSON streaming VisionAiProvider)
    │       └── factory.rs                  # Redacted ProviderSecrets, build_translator, build_vision_ai_provider
    ├── sts-platform/
    │   ├── Cargo.toml                      # Add target-scoped keyring & Linux-only ashpd
    │   └── src/
    │       ├── lib.rs                      # Add SecretStoreFailed/WaylandPortalFailed; export secrets & wayland modules
    │       ├── secrets.rs                  # SecretStore trait, MemorySecretStore, KeyringSecretStore (keyring 3.6.3)
    │       ├── wayland.rs                  # WaylandPortalCapabilities, WaylandShortcutSpec, ashpd portals on Linux
    │       ├── overlay.rs                  # resolve_effective_live_mode_for_target & is_rect_fullscreen_or_maximized
    │       ├── hotkeys.rs                  # HotkeyBinding::{portal_id, description} helpers for Wayland GlobalShortcuts
    │       └── capture.rs                  # Fractional HiDPI & negative-origin multi-monitor crop verification tests
    └── sts-app/
        ├── Cargo.toml                      # Add image.workspace = true; wiremock.workspace = true in dev-dependencies
        ├── ui/
        │   ├── globals.slint               # Ask AI streaming properties, Tab 3 BYOK & Keyring state, new AppActions callbacks
        │   ├── snip-overlay.slint          # Interactive Ask AI card (preset chips, prompt, streaming answer, Stop/Copy)
        │   └── settings.slint              # Full production Tab 3 BYOK + masked OS Keyring + Test Connection UI
        ├── src/
        │   ├── state.rs                    # ProviderBuilder, SecretStore integration, Ask AI streaming, provider hot-reload, Wayland fullscreen can_inplace
        │   ├── ui_bridge.rs                # Bind Ask AI, Keyring & Test callbacks/events + clear password inputs cleanly
        │   └── main.rs                     # Wire KeyringSecretStore, DefaultProviderBuilder, optional Wayland GlobalShortcuts
        └── tests/
            ├── square_to_search_flow.rs    # Pass secrets + translation_cache in AppServices
            ├── live_translate_flow.rs      # Pass secrets + translation_cache in AppServices
            └── byok_ai_and_wayland_flow.rs # E2E Ask AI streaming/cancel, Keyring config.toml isolation, hot-reload, Wayland Decision 2A
```

### Parallel Wave Execution Graph

- **Wave 1 (Parallel, disjoint files):**
  - **Task 1 (`sts-core` + root `Cargo.toml`)**
  - **Task 4 (`crates/sts-app/ui/*.slint`)**
- **Wave 2 (Parallel, after Task 1):**
  - **Task 2 (`crates/sts-translate/**`)**
  - **Task 3 (`crates/sts-platform/**`)**
- **Wave 3 (Parallel, after Tasks 2, 3, 4):**
  - **Task 5 (`crates/sts-app/Cargo.toml`, `src/**`, `tests/**`)**
  - **Task 6 (`deny.toml`, `scripts/package-windows.ps1`, `scripts/package-linux.sh`, `README.md`)**
- **Wave 4 (Final Verification Gate):**
  - Full workspace formatting, Clippy, nextest suite, Slint syntax check, `cargo-deny`, release build, and Windows packaging smoke verification.

---

### Task 1: Workspace Dependencies, Non-Secret BYOK Config & HiDPI Multi-Monitor Geometry (`sts-core`)

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/sts-core/src/config.rs`
- Modify: `crates/sts-core/src/geometry.rs`
- Modify: `crates/sts-core/src/lib.rs`

**Interfaces:**
- Consumes: existing `AppConfig`, `TranslateConfig`, `LogicalRect`, `PhysicalRect`, `MonitorGeometry`.
- Produces in `sts_core::config`:
  - `pub const KEYRING_SERVICE_NAME: &str = "square-to-search";`
  - `pub enum SecretSlot { GeminiApiKey, OpenAiApiKey, DeepLApiKey }` with methods:
    - `pub const fn all() -> [SecretSlot; 3]`
    - `pub const fn account_key(self) -> &'static str` (`"gemini_api_key"`, `"openai_api_key"`, `"deepl_api_key"`)
    - `pub const fn slot_id(self) -> &'static str` (`"gemini"`, `"openai"`, `"deepl"`)
    - `pub fn from_slot_id(id: &str) -> Option<Self>`
    - `pub const fn display_name(self) -> &'static str`
  - Extended non-secret `TranslateConfig` fields (all `#[serde(default)]` with defaults):
    - `pub gemini_model: String` (default `"gemini-2.5-flash"`)
    - `pub openai_base_url: String` (default `"https://api.openai.com/v1"`)
    - `pub openai_model: String` (default `"gpt-4o-mini"`)
    - `pub ollama_model: String` (default `"qwen2.5vl:7b"`)
    - `pub deepl_free_tier: bool` (default `true`)
- Produces in `sts_core::geometry`:
  - `pub fn physical_to_logical_rect(rect: PhysicalRect, monitor: &MonitorGeometry) -> LogicalRect`
- Re-exported in `sts_core::lib`: `KEYRING_SERVICE_NAME`, `SecretSlot`, `physical_to_logical_rect`.

- [ ] **Step 1: Write failing tests in `crates/sts-core/src/config.rs` and `crates/sts-core/src/geometry.rs`**

Add tests in `crates/sts-core/src/config.rs`:
- `secret_slot_ids_roundtrip_and_cover_all_byok_slots`: validates `SecretSlot::all()` maps `slot_id <-> from_slot_id`, distinct `account_key`s, and `KEYRING_SERVICE_NAME == "square-to-search"`.
- `translate_config_serializes_no_secret_fields_and_backfills_m3_defaults`: serializes `AppConfig::default()` to TOML, asserts no `api_key` substring appears in TOML, deserializes partial `[translate]\nprovider = "gemini"\n`, and verifies defaults (`gemini_model == "gemini-2.5-flash"`, `openai_base_url == "https://api.openai.com/v1"`, `openai_model == "gpt-4o-mini"`, `ollama_model == "qwen2.5vl:7b"`, `deepl_free_tier == true`).

Add tests in `crates/sts-core/src/geometry.rs`:
- `fractional_hidpi_and_negative_origin_physical_roundtrip`: validates `physical_to_logical_rect` and `logical_to_physical_crop` across `scale_factor` values `[1.0, 1.25, 1.5, 1.75, 2.0]` on negative-origin monitors (`x = -2560, y = -1440`), ensuring bounds invariance (`crop.x + crop.width <= monitor.width` and `crop.y + crop.height <= monitor.height`).

- [ ] **Step 2: Run tests to verify RED**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
.\.tools\bin\cargo-nextest.exe nextest run -p sts-core
```
Expected: FAIL (`SecretSlot`, new `TranslateConfig` fields, and `physical_to_logical_rect` not found).

- [ ] **Step 3: Implement `Cargo.toml` entries and `sts-core` changes**

1. Add workspace dependency entries to `Cargo.toml` `[workspace.dependencies]`:
   ```toml
   keyring = { version = "3.6.3", default-features = false }
   base64 = "0.23.1"
   ashpd = "0.14.3"
   ```
2. Implement `KEYRING_SERVICE_NAME`, `SecretSlot`, and non-secret `TranslateConfig` fields in `crates/sts-core/src/config.rs` with individual `#[serde(default = "...")]` functions so partial legacy `config.toml` files from Milestone 1/2 backfill cleanly without triggering corrupt recovery.
3. Implement `physical_to_logical_rect` in `crates/sts-core/src/geometry.rs` (handling non-finite or `<= 0.0` `scale_factor` safely by clamping `scale_factor.max(0.1)`).
4. Re-export `KEYRING_SERVICE_NAME`, `SecretSlot`, and `physical_to_logical_rect` in `crates/sts-core/src/lib.rs`.

- [ ] **Step 4: Run `sts-core` verification**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo clippy -p sts-core --all-targets -- -D warnings
.\.tools\bin\cargo-nextest.exe nextest run -p sts-core
```
Expected: PASS (`0` warnings, all `sts-core` tests passing).

---

### Task 2: BYOK Translators, Multimodal Streaming Ask AI & Provider Factory (`sts-translate`)

**Files:**
- Modify: `crates/sts-translate/Cargo.toml`
- Modify: `crates/sts-translate/src/lib.rs`
- Modify: `crates/sts-translate/src/service.rs`
- Create: `crates/sts-translate/src/gemini.rs`
- Create: `crates/sts-translate/src/openai.rs`
- Create: `crates/sts-translate/src/deepl.rs`
- Create: `crates/sts-translate/src/ollama.rs`
- Create: `crates/sts-translate/src/factory.rs`

**Interfaces:**
- In `crates/sts-translate/src/lib.rs`:
  - Add to `TranslateError`:
    - `#[error("authentication failed for translation/AI provider: {0}")] AuthFailed(String)`
    - `#[error("translation/AI provider is not configured: {0}")] ProviderNotConfigured(String)`
  - Add to `VisionAiProvider`:
    ```rust
    /// Streams incremental answer chunks over `delta_tx` and resolves with the complete response.
    fn ask_stream<'a>(
        &'a self,
        request: &'a VisionAiRequest,
        delta_tx: tokio::sync::mpsc::UnboundedSender<String>,
    ) -> BoxFuture<'a, Result<VisionAiResponse, TranslateError>> {
        Box::pin(async move {
            let response = self.ask(request).await?;
            if !response.answer.is_empty() {
                let _ = delta_tx.send(response.answer.clone());
            }
            Ok(response)
        })
    }
    ```
  - Add UTF-8-safe `pub(crate) struct LineStreamDecoder`: maintains `pending: Vec<u8>`, exposes `push_bytes(&mut self, bytes: &[u8]) -> Vec<String>` and `finish(&mut self) -> Option<String>`, decoding UTF-8 only on complete `\n`-delimited lines (never slicing mid-codepoint across HTTP chunks!).
  - Add helper `pub(crate) fn map_http_status_error(provider: &str, status: reqwest::StatusCode, body: &str) -> TranslateError` mapping `401 | 403 -> AuthFailed`, `429 | 456 -> RateLimited`, others -> `InvalidResponse` (with summarized body, never reflecting auth headers).
- In `crates/sts-translate/src/service.rs`:
  - Add `pub struct CachedTranslator` wrapping a single `Arc<dyn Translator>` + `Arc<Mutex<TranslationCache>>` with `impl Translator for CachedTranslator`, skipping cache on empty inputs, batching only cache misses, and calling `call_checked` (never falling back to free web endpoints).
- In `crates/sts-translate/src/gemini.rs`:
  - `GeminiProvider::new(api_key: impl Into<String>, model: impl Into<String>, custom_prompt: Option<String>)`
  - `GeminiProvider::with_base_url(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>, custom_prompt: Option<String>)` (for `wiremock`)
  - Sends `x-goog-api-key` HTTP header (NEVER embeds API key in URL query string!).
  - `Translator`: `POST {base}/models/{model}:generateContent` using `pack_batch`/`unpack_batch` and system prompt / `custom_prompt`.
  - `VisionAiProvider`: `ask` (`:generateContent`) and `ask_stream` (`:streamGenerateContent?alt=sse`), attaching `request.image_png` as `inline_data: { mime_type: "image/png", data: <base64> }`, accumulating deltas into `VisionAiResponse.answer`.
- In `crates/sts-translate/src/openai.rs`:
  - `OpenAiCompatProvider::new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>, custom_prompt: Option<String>)`
  - Normalizes `base_url` by trimming trailing `/`; sends `Authorization: Bearer {key}`.
  - `Translator`: `POST {base}/chat/completions` (`stream: false`, `pack_batch`/`unpack_batch` + `custom_prompt`).
  - `VisionAiProvider`: `ask` (`stream: false`) and `ask_stream` (`stream: true`, multimodal message content array with `image_url: { url: "data:image/png;base64,..." }` when `image_png.is_some()`), parsing `data: ...` SSE lines and stopping on `data: [DONE]`.
- In `crates/sts-translate/src/deepl.rs`:
  - `DeepLTranslator::new(api_key: impl Into<String>, free_tier: bool)` (`https://api-free.deepl.com` vs `https://api.deepl.com`)
  - `DeepLTranslator::with_base_url(base_url: impl Into<String>, api_key: impl Into<String>)`
  - `POST {base}/v2/translate` with `Authorization: DeepL-Auth-Key {key}`, mapping `"zh-CN"` / `"zh"` -> `"ZH"`, `"en"` -> `"EN"`, etc., and `target_lang` uppercased (`"VI"`, `"EN-US"`, etc.).
- In `crates/sts-translate/src/ollama.rs`:
  - `OllamaProvider::new(base_url: impl Into<String>, model: impl Into<String>, custom_prompt: Option<String>)`
  - `Translator`: `POST {base}/api/chat` (`stream: false`).
  - `VisionAiProvider`: `ask` (`stream: false`) and `ask_stream` (`stream: true`), sending `images: [base64_png]` on user message when `image_png.is_some()`, parsing NDJSON `message.content` chunks until `done: true`.
- In `crates/sts-translate/src/factory.rs`:
  - `pub struct ProviderSecrets { pub gemini_api_key: Option<String>, pub openai_api_key: Option<String>, pub deepl_api_key: Option<String> }` with custom `fmt::Debug` printing `"[REDACTED]"` for `Some(_)` and `"None"` for `None`!
  - `pub fn build_translator(config: &TranslateConfig, secrets: &ProviderSecrets, cache: Arc<Mutex<TranslationCache>>) -> Result<Arc<dyn Translator>, TranslateError>`
  - `pub fn build_vision_ai_provider(config: &TranslateConfig, secrets: &ProviderSecrets) -> Result<Option<Arc<dyn VisionAiProvider>>, TranslateError>`
  - `pub fn vision_provider_display_label(config: &TranslateConfig) -> Option<String>`

- [ ] **Step 1: Write failing tests (`wiremock` + unit) across `sts-translate`**

Write failing unit & `wiremock` tests covering:
1. `LineStreamDecoder` splitting a multi-byte Vietnamese word (`"Tiếng Việt — Truyện tranh"`) across arbitrary byte boundaries (`chunks` cutting mid UTF-8 char) without replacement characters or dropped lines.
2. `GeminiProvider`:
   - Verifies `x-goog-api-key` header present and API key absent from URL query string.
   - Batch `translate()` unpacks numbered comic lines cleanly with `custom_prompt`.
   - `ask_stream()` decodes multi-chunk SSE `:streamGenerateContent?alt=sse` response with `inline_data` PNG attachment and emits partial chunks before resolving full answer.
   - HTTP `401/403` -> `TranslateError::AuthFailed`, `429` -> `TranslateError::RateLimited`.
3. `OpenAiCompatProvider`:
   - `translate()` hits `{base}/chat/completions` with `Authorization: Bearer ...` and trailing-slash normalization (`.../v1/`).
   - `ask_stream()` sends `data:image/png;base64,...`, parses chunked SSE including empty keepalive lines, malformed skip guard or error on corrupt JSON payload, and terminates cleanly on `data: [DONE]`.
4. `DeepLTranslator`:
   - Free vs Pro default URLs; `DeepL-Auth-Key` header; native array batch ordering; `403 -> AuthFailed`, `456 -> RateLimited`.
5. `OllamaProvider`:
   - `POST /api/chat` NDJSON streaming (`images: [...]`), handles `done: true`, maps network error cleanly.
6. `factory.rs`:
   - `ProviderSecrets` `Debug` output NEVER contains literal secret strings (Review Focus #1).
   - Missing key returns `TranslateError::ProviderNotConfigured` for `gemini`/`openai`/`deepl`; `"free_web"` builds `CachedFallbackTranslator`; `"ollama"` succeeds without API key; BYOK translators hit `CachedTranslator` LRU cache on repeat queries.

- [ ] **Step 2: Run tests to verify RED**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
.\.tools\bin\cargo-nextest.exe nextest run -p sts-translate
```
Expected: FAIL (`gemini`, `openai`, `deepl`, `ollama`, `factory` modules missing).

- [ ] **Step 3: Implement `sts-translate` modules cleanly**

1. Update `crates/sts-translate/Cargo.toml` (`base64.workspace = true`).
2. Update `crates/sts-translate/src/lib.rs` and `crates/sts-translate/src/service.rs`.
3. Implement `gemini.rs`, `openai.rs`, `deepl.rs`, `ollama.rs`, and `factory.rs`, sharing `LineStreamDecoder`, `pack_batch`/`unpack_batch`, `build_prompt_messages`, and `map_http_status_error` cleanly (zero duplicated SSE parsing or error mapping!).

- [ ] **Step 4: Run `sts-translate` verification**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo clippy -p sts-translate --all-targets -- -D warnings
.\.tools\bin\cargo-nextest.exe nextest run -p sts-translate
```
Expected: PASS (`0` warnings, all `sts-translate` tests passing).

---

### Task 3: OS Keyring Secrets, Wayland Portal Adapters, Decision 2A Fullscreen Policy & HiDPI Tests (`sts-platform`)

**Files:**
- Modify: `crates/sts-platform/Cargo.toml`
- Modify: `crates/sts-platform/src/lib.rs`
- Create: `crates/sts-platform/src/secrets.rs`
- Create: `crates/sts-platform/src/wayland.rs`
- Modify: `crates/sts-platform/src/overlay.rs`
- Modify: `crates/sts-platform/src/hotkeys.rs`
- Modify: `crates/sts-platform/src/capture.rs`

**Interfaces:**
- In `crates/sts-platform/src/lib.rs`:
  - Add `PlatformError::SecretStoreFailed(String)` and `PlatformError::WaylandPortalFailed(String)`.
  - Export `secrets` and `wayland` modules and re-export `KeyringSecretStore`, `MemorySecretStore`, `SecretStore`, `WaylandPortalCapabilities`, `WaylandLayerSurfaceConfig`, `WaylandShortcutSpec`, `default_wayland_shortcut_specs`, `is_rect_fullscreen_or_maximized`, `resolve_effective_live_mode_for_target`.
- In `crates/sts-platform/src/secrets.rs`:
  - ```rust
    pub trait SecretStore: Send + Sync {
        fn get_secret(&self, slot: SecretSlot) -> Result<Option<String>, PlatformError>;
        fn set_secret(&self, slot: SecretSlot, value: &str) -> Result<(), PlatformError>;
        fn delete_secret(&self, slot: SecretSlot) -> Result<(), PlatformError>;
        fn has_secret(&self, slot: SecretSlot) -> Result<bool, PlatformError> {
            Ok(self.get_secret(slot)?.is_some())
        }
    }
    ```
  - `MemorySecretStore`: thread-safe `Mutex<HashMap<SecretSlot, String>>` with custom `fmt::Debug` redacting values (`Review Focus #1`). Rejects empty/whitespace-only secrets via `Err(PlatformError::SecretStoreFailed(...))`; trims outer whitespace or rejects empty string consistently.
  - `KeyringSecretStore`: backed by `keyring::Entry::new(KEYRING_SERVICE_NAME, slot.account_key())` (in `keyring 3.6.3`: `get_password()`, `set_password()`, `delete_credential()`, mapping `keyring::Error::NoEntry` to `Ok(None)` on `get_secret` and `Ok(())` on `delete_secret`, mapping `NoStorageAccess`/`PlatformFailure`/etc. to `PlatformError::SecretStoreFailed`).
- In `crates/sts-platform/src/overlay.rs`:
  - ```rust
    #[must_use]
    pub fn is_rect_fullscreen_or_maximized(target: PhysicalRect, monitor: &MonitorGeometry) -> bool
    ```
    Returns `true` when `monitor.width > 0 && monitor.height > 0` and `(target.width * 100 >= monitor.width * 90) && (target.height * 100 >= monitor.height * 85)`.
  - ```rust
    #[must_use]
    pub fn resolve_effective_live_mode_for_target(
        requested: LiveDisplayMode,
        display: DisplayServer,
        target_is_fullscreen_or_maximized: bool,
    ) -> EffectiveLiveMode
    ```
    Policy (Decision 2A):
    - `FloatingPanel` on any display -> `FloatingPanel`, `notice: None`.
    - `InPlace` on `Windows | LinuxX11` -> `InPlace`, `notice: None`.
    - `InPlace` on `LinuxWayland { compositor }` where `display.supports_wayland_layer_shell()` (`Kde | Hyprland | Sway`) AND `target_is_fullscreen_or_maximized` -> `InPlace`, `notice: None`.
    - `InPlace` on `LinuxWayland { compositor: Gnome }` -> `FloatingPanel` with Vietnamese GNOME notice.
    - `InPlace` on `LinuxWayland { .. }` when `!target_is_fullscreen_or_maximized` (floating window on KDE/Hyprland/Sway or unsupported Wayland compositor) -> `FloatingPanel` with Vietnamese Wayland notice.
    - `InPlace` on `Other` -> `FloatingPanel` with notice.
  - Preserve `resolve_effective_live_mode(requested, display)` calling `resolve_effective_live_mode_for_target(requested, display, false)` so all existing callers and Milestone 2 unit tests stay 100% green.
- In `crates/sts-platform/src/wayland.rs`:
  - Cross-platform capability & protocol descriptors:
    - `WaylandPortalCapabilities { global_shortcuts: bool, screencast: bool, screenshot_fallback: bool, layer_shell_fullscreen: bool }` with `pub const fn for_display(display: DisplayServer) -> Self`
    - `WaylandLayerSurfaceConfig { anchor_all_edges: bool, exclusive_zone: i32, keyboard_interactivity: bool, input_region_empty: bool }` with `for_live_inplace() -> Self`
    - `WaylandShortcutSpec { id: &'static str, action: HotkeyAction, description: &'static str, preferred_trigger: String }` + `default_wayland_shortcut_specs(config: &HotkeyConfig) -> Vec<WaylandShortcutSpec>`
  - Linux `#[cfg(target_os = "linux")]` `ashpd 0.14.3` portal integration (`WaylandGlobalShortcutsPortal`, `WaylandScreenshotPortal`, `WaylandScreenCastPortal` requesting `SourceType::Window | SourceType::Monitor`). Inspect `ashpd` 0.14.3 exact API signatures in cargo cache before writing `#[cfg(target_os = "linux")]` code; additionally cross-check compilation using `cargo check -p sts-platform --target x86_64-unknown-linux-gnu` (if toolchain target or sysroot supports it, or `cargo clippy` check on Linux portal module logic).
- In `crates/sts-platform/src/hotkeys.rs` & `capture.rs`:
  - Add `HotkeyBinding::portal_id(self.action)` / `description(self.action)` helpers on `hotkeys.rs`.
  - Add `crop_frame` tests on negative-origin and fractional HiDPI coordinates in `capture.rs`.

- [ ] **Step 1: Write failing tests in `secrets.rs`, `overlay.rs`, `wayland.rs`, and `capture.rs`**

Tests must verify:
1. `MemorySecretStore` CRUD (`get -> None`, `set`, `get -> Some`, `has_secret`, idempotent `delete`), blank secret rejection, and `Debug` output NEVER contains secret value (`Review Focus #1 & #3`).
2. `resolve_effective_live_mode_for_target` exhaustive matrix across `(Windows, LinuxX11, Wayland::Kde, Wayland::Hyprland, Wayland::Sway, Wayland::Gnome, Wayland::Other, Other)` × `fullscreen = true/false` (`Review Focus #4`).
3. `is_rect_fullscreen_or_maximized` boundary thresholds (`90% × 85%` vs floating `70% × 70%`).
4. `WaylandPortalCapabilities::for_display` and `default_wayland_shortcut_specs` mapping from `HotkeyConfig`.
5. Negative-origin multi-monitor `crop_frame` clamping tests.

- [ ] **Step 2: Run tests to verify RED**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
.\.tools\bin\cargo-nextest.exe nextest run -p sts-platform
```
Expected: FAIL (`secrets`, `wayland`, `resolve_effective_live_mode_for_target` not yet defined).

- [ ] **Step 3: Implement `sts-platform` changes**

1. Update `crates/sts-platform/Cargo.toml`:
   ```toml
   [target.'cfg(target_os = "windows")'.dependencies]
   tray-icon = { workspace = true }
   windows-sys = { workspace = true }
   keyring = { workspace = true, features = ["windows-native"] }

   [target.'cfg(target_os = "linux")'.dependencies]
   tray-icon = { workspace = true, features = ["ksni"] }
   x11rb = { workspace = true }
   keyring = { workspace = true, features = ["async-secret-service", "tokio", "crypto-rust"] }
   ashpd = { workspace = true }
   ```
2. Implement `secrets.rs`, `wayland.rs`, `overlay.rs`, `hotkeys.rs`, `capture.rs`, and export from `lib.rs`.

- [ ] **Step 4: Run `sts-platform` verification**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo clippy -p sts-platform --all-targets -- -D warnings
.\.tools\bin\cargo-nextest.exe nextest run -p sts-platform
```
Also run `cargo check --target x86_64-unknown-linux-gnu -p sts-platform` to verify Linux `ashpd` + `keyring` `cfg(target_os = "linux")` code compiles cleanly from Windows if no C/D-Bus linker is needed for `cargo check` (metadata/rlib check)! Note any Linux sysroot requirement if `check` reaches C linkers, though pure `cargo check` skips linking.

---

### Task 4: Slint UI — Streaming Ask AI Card & Complete BYOK / OS Keyring Tab 3 (`sts-app/ui`)

**Files:**
- Modify: `crates/sts-app/ui/globals.slint`
- Modify: `crates/sts-app/ui/snip-overlay.slint`
- Modify: `crates/sts-app/ui/settings.slint`

**Interfaces (Slint `AppState` & `AppActions`):**
- `AppState` additions in `globals.slint`:
  - Ask AI streaming properties:
    - `in-out property <string> ai-prompt: "";`
    - `in-out property <string> ai-answer: "";`
    - `in-out property <bool> ai-streaming: false;`
    - `in-out property <string> ai-provider-label: "";`
  - Tab 3 BYOK & Keyring properties:
    - `in-out property <string> gemini-model: "gemini-2.5-flash";`
    - `in-out property <string> openai-base-url: "https://api.openai.com/v1";`
    - `in-out property <string> openai-model: "gpt-4o-mini";`
    - `in-out property <string> ollama-model: "qwen2.5vl:7b";`
    - `in-out property <bool> deepl-free-tier: true;`
    - `in-out property <bool> has-gemini-key: false;`
    - `in-out property <bool> has-openai-key: false;`
    - `in-out property <bool> has-deepl-key: false;`
    - `in-out property <string> gemini-key-input: "";`
    - `in-out property <string> openai-key-input: "";`
    - `in-out property <string> deepl-key-input: "";`
    - `in-out property <string> ai-test-status: "";`
    - `in-out property <bool> ai-testing: false;`
- `AppActions` callbacks additions in `globals.slint`:
  - `callback submit-ask-ai(string);`
  - `callback stop-ask-ai();`
  - `callback save-api-key(string, string);`
  - `callback clear-api-key(string);`
  - `callback test-ai-provider(string);`

- [ ] **Step 1: Add Slint properties and callbacks to `crates/sts-app/ui/globals.slint`**

Update `card-mode` comment to include `"ai"`:
`"none" | "ocr" | "translate" | "ai" | "ai-unconfigured" | "loading" | "error"`.
Add all Ask AI and Tab 3 BYOK/Keyring properties & callbacks listed above.

- [ ] **Step 2: Add interactive Ask AI card (`card-mode == "ai"`) to `crates/sts-app/ui/snip-overlay.slint`**

Design inside the inline result card (`width: min(520px, root.width - 16px)` or existing `min(500px, root.width - 16px)`):
- Include `"ai"` in `show-card` condition and header title (`AppState.card-mode == "ai" ? "Hỏi AI · " + AppState.ai-provider-label : ...`), plus mark the `"Hỏi AI"` `HudActionButton` `active: AppState.card-mode == "ai" || AppState.card-mode == "ai-unconfigured"`.
- Hide `OcrPackChip` row when `AppState.card-mode == "ai" || AppState.card-mode == "ai-unconfigured"`.
- Render `if AppState.card-mode == "ai" : VerticalLayout`:
  - Preset quick chips row (`OcrPackChip` or `PresetChip` buttons): `"Giải thích"`, `"Tóm tắt"`, `"Dịch tự nhiên"` — clicking fills `AppState.ai-prompt` with a crisp Vietnamese prompt about the selected image/text and immediately invokes `AppActions.submit-ask-ai(AppState.ai-prompt)`.
  - Prompt input row: `LineEdit { text <=> AppState.ai-prompt; placeholder-text: "Nhập câu hỏi về vùng ảnh này..."; accepted => { AppActions.submit-ask-ai(self.text); } }` + `"Gửi"` (`!AppState.ai-streaming`) / `"Dừng"` (`AppState.ai-streaming` -> `AppActions.stop-ask-ai()`).
  - Indeterminate `ProgressIndicator` visible while `AppState.ai-streaming`.
  - Streaming answer `TextEdit { height: 110px; text <=> AppState.ai-answer; read-only: true; wrap: word-wrap; }`.
  - Footer row: status badge + `"Copy câu trả lời"` (`AppActions.copy-result-text(AppState.ai-answer)`).

- [ ] **Step 3: Upgrade Settings Tab 3 (`Dịch & AI`) in `crates/sts-app/ui/settings.slint`**

Update Tab 3 to remove `"Giai đoạn 3"` preview tags and provide complete BYOK & Keyring controls:
1. Live Translate provider ComboBox (`"Miễn phí (Google / Bing Web)"`, `"Gemini (BYOK)"`, `"OpenAI-compatible (BYOK)"`, `"DeepL (BYOK)"`, `"Ollama Local"`).
2. Ask AI provider ComboBox (`"Chưa cấu hình"`, `"Gemini (BYOK)"`, `"OpenAI-compatible (BYOK)"`, `"Ollama Local"`).
3. Non-secret endpoint/model configuration card:
   - Gemini model (`LineEdit` bound to `AppState.gemini-model`)
   - OpenAI Base URL (`LineEdit` bound to `AppState.openai-base-url`) + OpenAI model (`AppState.openai-model`)
   - Ollama Base URL (`AppState.ollama-base-url`) + Ollama model (`AppState.ollama-model`)
   - DeepL API Free endpoint switch (`Switch` bound to `AppState.deepl-free-tier`)
   - Custom manga prompt (`TextEdit` bound to `AppState.ai-custom-prompt`)
4. Dedicated OS Keyring Secrets Card (`"API Keys (Lưu an toàn trong OS Keyring)"`):
   - One clean reusable Slint `component ApiKeyRow` (avoiding duplicated Slint markup!) used for `"gemini"`, `"openai"`, and `"deepl"`: displays provider label, badge (`"Đã lưu trong Keyring"` vs `"Chưa lưu"`), masked `LineEdit { input-type: InputType.password; placeholder-text: has-key ? "•••••••••••• (nhập mới để thay thế)" : "Dán API key..."; }` (clear text, NEVER displays real key), `"Lưu Key"` button (`AppActions.save-api-key(...)`), and `"Xóa"` button (`AppActions.clear-api-key(...)`, enabled when `has-key`).
5. Provider diagnostics row: `"Kiểm tra Dịch Live"` (`AppActions.test-ai-provider("live")`) and `"Kiểm tra Hỏi AI"` (`AppActions.test-ai-provider("ask-ai")`) buttons + status readout `AppState.ai-test-status`.

- [ ] **Step 4: Verify Slint syntax & inspect layout screenshots**

Run:
```powershell
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
```
Expected: Exit `0` with zero Slint compiler errors or warnings. Also render screenshots of `SettingsWindow` / `SnipOverlayWindow` if helpful via `slint-viewer.exe --save-data` or `--screenshot`? Check `--help` before passing flags, or inspect `ui` visually once `sts-app` compiles in Task 5.

---

### Task 5: Wire Keyring, Dynamic BYOK Providers, Ask AI Streaming, Hot-Reload & Wayland Fullscreen (`sts-app`)

**Files:**
- Modify: `crates/sts-app/Cargo.toml`
- Modify: `crates/sts-app/src/state.rs`
- Modify: `crates/sts-app/src/ui_bridge.rs`
- Modify: `crates/sts-app/src/main.rs`
- Modify: `crates/sts-app/tests/square_to_search_flow.rs`
- Modify: `crates/sts-app/tests/live_translate_flow.rs`
- Create: `crates/sts-app/tests/byok_ai_and_wayland_flow.rs`

**Interfaces:**
- In `crates/sts-app/Cargo.toml`:
  - Add `image = { workspace = true }` under `[dependencies]` (for PNG encoding `CaptureFrame` in Ask AI).
  - Add `wiremock = { workspace = true }` under `[dev-dependencies]`.
- In `crates/sts-app/src/state.rs`:
  - Add `ProviderTestTarget { LiveTranslator, AskAi }`.
  - Add `pub trait ProviderBuilder: Send + Sync` (method `build_translator(&self, config: &TranslateConfig, secrets: &ProviderSecrets, cache: Arc<Mutex<TranslationCache>>) -> Result<Arc<dyn Translator>, TranslateError>` and `build_vision_ai(&self, config: &TranslateConfig, secrets: &ProviderSecrets) -> Result<Option<Arc<dyn VisionAiProvider>>, TranslateError>`), plus `pub struct DefaultProviderBuilder;` delegating to `sts_translate::factory`.
  - Extend `AppServices` with:
    - `pub secrets: Arc<dyn SecretStore>`
    - `pub translation_cache: Arc<tokio::sync::Mutex<TranslationCache>>`
    - `pub provider_builder: Arc<dyn ProviderBuilder>`
  - Add async helper `load_provider_secrets(secrets: &Arc<dyn SecretStore>) -> Result<ProviderSecrets, PlatformError>` running `secrets.get_secret(...)` inside `tokio::task::spawn_blocking` (`Review Focus #3` — safe for Linux Secret Service `zbus`!).
  - Add `AppCommand` variants:
    - `SubmitAskAiPrompt(String)`
    - `StopAskAi`
    - `SaveApiKey { slot: SecretSlot, value: String }`
    - `ClearApiKey(SecretSlot)`
    - `TestAiProvider(ProviderTestTarget)`
  - Add `UiEvent` variants:
    - `ShowAskAiReady { provider_label: String }`
    - `AskAiStreamStarted { provider_label: String }`
    - `AskAiStreamDelta { chunk: String }`
    - `AskAiStreamFinished { full_answer: String, provider_label: String }`
    - `AskAiStreamError { message: String }`
    - `KeyringStatusUpdated { has_gemini: bool, has_openai: bool, has_deepl: bool, ai_provider_configured: bool, status_text: String }`
    - `ProviderTestFinished { ok: bool, message: String }`
    - Extend `UiEvent::ShowSettings` with `has_gemini_key: bool, has_openai_key: bool, has_deepl_key: bool` (so Settings opens immediately showing keyring badges cleanly).
  - Update `AppController`:
    - Maintain `translator: Arc<dyn Translator>`, `vision_ai: Option<Arc<dyn VisionAiProvider>>`, and `secrets_snapshot: ProviderSecrets` in controller state (`AppController::reload_providers(&mut self)` reads secrets via `spawn_blocking`, calls `provider_builder`; if selected BYOK `live_translator` lacks its API key, gracefully retains/builds Free Web fallback and warns via toast; updates `LiveSessionHandle` if needed or uses active `Arc<dyn Translator>`!).
    - Handle `AppCommand::RunAskAi(rect)`: crops selection check, reloads/checks `vision_ai`; if `None` (unconfigured or missing key), emits `ShowAskAiUnconfigured`; else emits `ShowAskAiReady { provider_label }`.
    - Handle `AppCommand::SubmitAskAiPrompt(prompt)`: aborts prior `in_flight_task`, validates non-empty prompt and `self.vision_ai`, crops active selection and encodes PNG bytes (`image::codecs::png::PngEncoder`), runs optional local OCR text (best-effort if starter pack installed, without blocking visual Ask AI when user just wants AI on an image!), spawns task listening to `delta_rx` and running `provider.ask_stream(...)` inside `tokio::select!` with `cancel_rx`; `StopAskAi` and `CancelOverlay` abort task and guarantee zero trailing deltas (`Review Focus #2`).
    - Handle `AppCommand::SaveApiKey { slot, value }`: calls `spawn_blocking` -> `secrets.set_secret(slot, &value)`, reloads providers, emits `KeyringStatusUpdated`.
    - Handle `AppCommand::ClearApiKey(slot)`: calls `spawn_blocking` -> `secrets.delete_secret(slot)`, reloads providers, emits `KeyringStatusUpdated`.
    - Handle `AppCommand::TestAiProvider(target)`: tests active Live Translator (translating `"Hello"` -> `target_lang`) or active Vision AI provider (short text prompt `"Reply OK"`) and emits `ProviderTestFinished { ok, message }`.
    - Update `start_live_session_for_target`: compute `let is_fullscreen = is_rect_fullscreen_or_maximized(overlay_physical_rect, &overlay_monitor);` and pass `is_fullscreen` into `resolve_effective_live_mode_for_target` so KDE/Hyprland/Sway fullscreen sessions enable `InPlace` (`can_inplace = display_server.supports_windowed_inplace() || (display_server.supports_wayland_layer_shell() && is_fullscreen)`).
- In `crates/sts-app/src/ui_bridge.rs`:
  - Wire `submit-ask-ai`, `stop-ask-ai`, `save-api-key`, `clear-api-key`, `test-ai-provider` callbacks + map `provider` ComboBox indices (`0..=4`) and `ai-provider` indices (`0..=3`) cleanly both ways in `apply_config_to_app_state` and `read_config_from_app_state`.
  - Handle `ShowAskAiReady`, `AskAiStreamStarted`, `AskAiStreamDelta` (append `chunk` onto `AppState.ai-answer`), `AskAiStreamFinished`, `AskAiStreamError`, `KeyringStatusUpdated` (clear `gemini-key-input`/`openai-key-input`/`deepl-key-input` when saved so plaintext never lingers in Slint!), `ProviderTestFinished`, and updated `ShowSettings`.
- In `crates/sts-app/src/main.rs`:
  - Instantiate `KeyringSecretStore::new()`, `DefaultProviderBuilder`, shared `TranslationCache`, and attempt `WaylandGlobalShortcutsPortal` registration on `#[cfg(target_os = "linux")]` when `display_server.is_wayland()`.

- [ ] **Step 1: Update existing test `AppServices` constructors and write failing E2E tests in `crates/sts-app/tests/byok_ai_and_wayland_flow.rs`**

Update `square_to_search_flow.rs` and `live_translate_flow.rs` helpers to inject `MemorySecretStore`, `translation_cache`, and `DefaultProviderBuilder`.
Add E2E tests in `crates/sts-app/tests/byok_ai_and_wayland_flow.rs`:
1. `ask_ai_streams_with_cropped_png_and_preset_prompt_then_stops_cleanly`: starts local `wiremock` SSE OpenAI-compatible Vision endpoint, stores `openai` key via `SaveApiKey`, triggers `RunAskAi` -> `SubmitAskAiPrompt`, asserts `ShowAskAiReady`, multiple `AskAiStreamDelta` events, PNG data URL present in HTTP request body, and `AskAiStreamFinished`. Also tests `StopAskAi` mid-stream ensuring zero trailing deltas (`Review Focus #2`).
2. `saving_api_keys_stores_only_in_secret_store_never_in_config_toml`: saves Gemini, OpenAI, and DeepL keys + `SaveSettings`, reads raw `config.toml` bytes from `TempDir`, asserts zero literal API key substrings appear anywhere in `config.toml`, then clears a key and asserts `ShowAskAiUnconfigured` when attempting `RunAskAi` (`Review Focus #1`).
3. `hot_reloading_settings_switches_live_translator_to_byok_openai`: starts with Free Web default, saves OpenAI API key + updates `TranslateConfig.provider = "openai"`, tests `TestAiProvider(LiveTranslator)` and runs `RunQuickTranslate` -> verifies translation came from `wiremock` OpenAI endpoint without restarting `AppController`.
4. `wayland_fullscreen_enables_inplace_on_kde_while_gnome_and_windowed_use_floating_panel`: tests `AppController` configured with `DisplayServer::LinuxWayland { compositor: Kde }` starting fullscreen live region -> `can_inplace == true`, `LiveDisplayMode::InPlace`; floating live region on KDE -> `FloatingPanel`; fullscreen live region on `WaylandCompositor::Gnome` -> `FloatingPanel` with Vietnamese notice (`Review Focus #4`).

- [ ] **Step 2: Run `sts-app` tests to verify RED**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
.\.tools\bin\cargo-nextest.exe nextest run -p sts-app
```
Expected: FAIL (missing commands, events, and fields).

- [ ] **Step 3: Implement `sts-app` wiring across `state.rs`, `ui_bridge.rs`, and `main.rs`**

- [ ] **Step 4: Run `sts-app` verification**

Run:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo clippy -p sts-app --all-targets -- -D warnings
.\.tools\bin\cargo-nextest.exe nextest run -p sts-app
```
Expected: PASS (`0` warnings, all `sts-app` tests green).

---

### Task 6: Dependency Audit (`deny.toml`), Portable Release Packaging Scripts & Final Verification

**Files:**
- Create: `deny.toml`
- Create: `scripts/package-windows.ps1`
- Create: `scripts/package-linux.sh`
- Create: `README.md`

- [ ] **Step 1: Create `deny.toml` for `cargo-deny 0.22.0`**

Configure `[advisories]`, `[licenses]` (matching permissive open-source licenses used across workspace crates — inspect `cargo-deny check` output against existing `Cargo.lock` so zero false-positive license rejections fail the check without genuine justification!), `[bans]` explicitly banning CUDA/cuDNN bindings if ever pulled transitively (e.g., `cudarc`, `cuda-runtime-sys`, `cudnn-sys` — verify exact crate names match `cargo-deny` 0.22 syntax), and `[sources]` allowing `crates-io`.
Run `.\.tools\bin\cargo-deny.exe check` iteratively until `deny.toml` reflects the exact audited dependency graph cleanly.

- [ ] **Step 2: Implement `scripts/package-windows.ps1` (`Review Focus #5`)**

Script behavior:
- Supports `param([switch]$SkipBuild)` and `[string]$OutDir = "dist"`.
- Resolves workspace root, adds `"$env:USERPROFILE\.cargo\bin"` to PATH if needed.
- Unless `-SkipBuild`, builds `cargo build --release -p sts-app`.
- Creates clean staging directory `dist\staging\square-to-search-windows-x64\`:
  - Copies `target\release\sts-app.exe`.
  - Inspects `target\release\DirectML.dll`: if it is a `SymbolicLink` or reparse point, resolves `(Get-Item $dllPath).Target` (or copies bytes via `[System.IO.File]::Copy` / resolving target path) so the staged `DirectML.dll` is a real non-zero regular file (`Length -gt 0`, fail loudly if `0` bytes!).
  - Scans `target\release\*.dll` and staged `*.dll`: **aborts with non-zero exit code** if any filename matches `cuda|cudnn|cublas|cufft|curand|cusolver|cusparse|nccl|nvrtc` (Decision 3A guard).
  - Copies `README.md` into staging directory.
- Produces `dist\square-to-search-windows-x64.zip` via `Compress-Archive -Force` and writes `dist\square-to-search-windows-x64.zip.sha256` using `Get-FileHash -Algorithm SHA256`.

- [ ] **Step 3: Implement `scripts/package-linux.sh`**

POSIX `bash` script (`set -euo pipefail`):
- Builds `cargo build --release -p sts-app` (unless `--skip-build`).
- Stages both portable tarball (`dist/square-to-search-linux-x64.tar.gz`) and AppDir structure (`dist/AppDir/usr/bin/sts-app`, `dist/AppDir/usr/lib/`, `dist/AppDir/square-to-search.desktop`, `AppRun` wrapper setting `LD_LIBRARY_PATH`), resolves any symlinked `libonnxruntime*.so*` via `cp -L` into `usr/lib/` (never `libonnxruntime_providers_cuda.so` — aborts if any `*cuda*`/`*cudnn*` `.so` is present), writes `.sha256` checksum.

- [ ] **Step 4: Write `README.md`**

Concise Vietnamese/English user & developer README covering:
- Overview (Square to Search + Continuous Live Translate)
- Default Hotkeys (`Ctrl+Shift+S`, `Ctrl+Shift+T`, `Ctrl+Shift+H`)
- Platform Support Matrix (Windows 10/11 Auto/CPU/DirectML; Linux X11 full; Linux Wayland Floating Panel + KDE/Hyprland/Sway fullscreen `wlr-layer-shell`; Zero CUDA in v1)
- OCR Starter Download & SHA-256 verification
- Free Web Translator vs BYOK (Gemini / OpenAI-compatible / DeepL + OS Keyring) & Ollama Local
- Development & Release Packaging commands.

- [ ] **Step 5: Run Final Full-Workspace Milestone 3 Verification Gate**

Run in PowerShell:
```powershell
$env:PATH = "$env:USERPROFILE\.cargo\bin;" + $env:PATH
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
.\.tools\bin\cargo-nextest.exe nextest run --workspace
.\.tools\bin\slint-viewer.exe --check "crates\sts-app\ui\app.slint"
.\.tools\bin\cargo-deny.exe check
cargo build --release -p sts-app
powershell -ExecutionPolicy Bypass -File "scripts\package-windows.ps1" -SkipBuild
```
Also verify the generated ZIP contents directly:
- Assert `dist\square-to-search-windows-x64.zip` contains `sts-app.exe`, non-empty `DirectML.dll` (`Length > 0`), `README.md`, and zero `*cuda*` entries; assert `dist\square-to-search-windows-x64.zip.sha256` exists.

Expected: Every command exits `0` cleanly, zero compiler/clippy/deny warnings, 100% nextest pass rate with 0 skipped tests, and verified portable Windows release archive.
