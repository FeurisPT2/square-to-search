//! Asynchronous state machine and command/event orchestration for `square-to-search`.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use sts_core::cache::TranslationCache;
use sts_core::config::{
    AppConfig, ConfigLoadOutcome, ConfigStore, SecretSlot, TranslateConfig, VisualSearchConfig,
};
use sts_core::geometry::{LogicalRect, MonitorGeometry, PhysicalRect, logical_to_physical_crop};
use sts_core::types::{CaptureFrame, LiveDisplayMode, OcrLanguagePack, OcrLine, OcrModelTier};
use sts_ocr::OcrError;
use sts_ocr::downloader::{DownloadProgress, ModelDownloader};
use sts_ocr::engine::OcrEngine;
use sts_ocr::manifest::{ModelManifest, ModelPackSpec, ModelStore, PackInstallStatus};
use sts_platform::PlatformError;
use sts_platform::capture::{ScreenCapturer, WindowGeometry, crop_frame};
use sts_platform::clipboard::ClipboardService;
use sts_platform::display_env::DisplayServer;
use sts_platform::overlay::{
    is_rect_fullscreen_or_maximized, resolve_effective_live_mode_for_target,
};
use sts_platform::secrets::SecretStore;
use sts_search::{BrowserOpener, SearchError, VisualSearchOutcome, VisualSearchService};
use sts_translate::{
    CachedFallbackTranslator, ProviderSecrets, TranslateError, TranslateRequest, TranslateResponse,
    Translator, VisionAiProvider, VisionAiRequest, VisionAiResponse, build_translator,
    build_vision_ai_provider, vision_provider_display_label,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use url::Url;

use crate::live::{
    LiveLoopConfig, LiveRenderedBlock, LiveSessionHandle, LiveTarget, spawn_live_session,
};

/// Heap-allocated `Send` future for object-safe async traits in `sts-app`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Thread-safe progress callback invoked during model pack downloads.
pub type ProgressCallback = Arc<dyn Fn(DownloadProgress) + Send + Sync>;

/// Object-safe wrapper around [`VisualSearchService`] allowing deterministic in-memory fakes in
/// integration tests without external network dependencies.
pub trait VisualSearcher: Send + Sync {
    /// Executes visual reverse image search for `frame` using `config`.
    fn search<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        config: &'a VisualSearchConfig,
    ) -> BoxFuture<'a, Result<VisualSearchOutcome, SearchError>>;
}

impl VisualSearcher for VisualSearchService {
    fn search<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        config: &'a VisualSearchConfig,
    ) -> BoxFuture<'a, Result<VisualSearchOutcome, SearchError>> {
        Box::pin(self.search_with_config(frame, config))
    }
}

/// Object-safe wrapper around [`ModelDownloader`] allowing deterministic in-memory pack downloads
/// in integration tests.
pub trait PackDownloader: Send + Sync {
    /// Downloads and verifies all artifacts in `spec`, installing them atomically into `store`.
    fn download_pack<'a>(
        &'a self,
        store: &'a ModelStore,
        spec: &'a ModelPackSpec,
        on_progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<PathBuf, OcrError>>;
}

impl PackDownloader for ModelDownloader {
    fn download_pack<'a>(
        &'a self,
        store: &'a ModelStore,
        spec: &'a ModelPackSpec,
        on_progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<PathBuf, OcrError>> {
        Box::pin(async move {
            ModelDownloader::download_pack(self, store, spec, move |p| on_progress(p)).await
        })
    }
}

/// Target provider tested by [`AppCommand::TestAiProvider`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTestTarget {
    /// Active Live / Quick Translate text translator.
    LiveTranslator,
    /// Active multimodal Ask AI vision provider.
    AskAi,
}

/// Factory abstraction for constructing [`Translator`] and [`VisionAiProvider`] instances from
/// configuration and OS keyring secrets (enabling hot-reload and deterministic test doubles).
pub trait ProviderBuilder: Send + Sync {
    /// Builds a [`Translator`] for `config` and `secrets` backed by `cache`.
    fn build_translator(
        &self,
        config: &TranslateConfig,
        secrets: &ProviderSecrets,
        cache: Arc<AsyncMutex<TranslationCache>>,
    ) -> Result<Arc<dyn Translator>, TranslateError>;

    /// Builds an optional multimodal [`VisionAiProvider`] for Ask AI from `config` and `secrets`.
    fn build_vision_ai(
        &self,
        config: &TranslateConfig,
        secrets: &ProviderSecrets,
    ) -> Result<Option<Arc<dyn VisionAiProvider>>, TranslateError>;
}

/// Production [`ProviderBuilder`] delegating to [`sts_translate::factory`].
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultProviderBuilder;

impl ProviderBuilder for DefaultProviderBuilder {
    fn build_translator(
        &self,
        config: &TranslateConfig,
        secrets: &ProviderSecrets,
        cache: Arc<AsyncMutex<TranslationCache>>,
    ) -> Result<Arc<dyn Translator>, TranslateError> {
        build_translator(config, secrets, cache)
    }

    fn build_vision_ai(
        &self,
        config: &TranslateConfig,
        secrets: &ProviderSecrets,
    ) -> Result<Option<Arc<dyn VisionAiProvider>>, TranslateError> {
        build_vision_ai_provider(config, secrets)
    }
}

/// Loads all BYOK provider secrets from `secrets` inside `tokio::task::spawn_blocking` so OS
/// keyring IPC (Linux Secret Service `zbus` or Windows Credential Manager) never blocks or nests
/// a Tokio runtime inside an async worker thread.
pub async fn load_provider_secrets(
    secrets: &Arc<dyn SecretStore>,
) -> Result<ProviderSecrets, PlatformError> {
    let store = Arc::clone(secrets);
    tokio::task::spawn_blocking(move || {
        Ok(ProviderSecrets {
            gemini_api_key: store.get_secret(SecretSlot::GeminiApiKey)?,
            openai_api_key: store.get_secret(SecretSlot::OpenAiApiKey)?,
            deepl_api_key: store.get_secret(SecretSlot::DeepLApiKey)?,
        })
    })
    .await
    .map_err(|err| PlatformError::SecretStoreFailed(format!("secret loading task failed: {err}")))?
}

async fn save_provider_secret(
    secrets: &Arc<dyn SecretStore>,
    slot: SecretSlot,
    value: String,
) -> Result<(), PlatformError> {
    let store = Arc::clone(secrets);
    tokio::task::spawn_blocking(move || store.set_secret(slot, &value))
        .await
        .map_err(|err| {
            PlatformError::SecretStoreFailed(format!("secret save task failed: {err}"))
        })?
}

async fn clear_provider_secret(
    secrets: &Arc<dyn SecretStore>,
    slot: SecretSlot,
) -> Result<(), PlatformError> {
    let store = Arc::clone(secrets);
    tokio::task::spawn_blocking(move || store.delete_secret(slot))
        .await
        .map_err(|err| {
            PlatformError::SecretStoreFailed(format!("secret delete task failed: {err}"))
        })?
}

/// Encodes an RGBA8 [`CaptureFrame`] into PNG bytes for multimodal Vision AI requests.
pub fn encode_frame_as_png(frame: &CaptureFrame) -> Result<Vec<u8>, String> {
    if frame.width == 0 || frame.height == 0 || frame.rgba.len() != frame.expected_byte_len() {
        return Err("Khung hình vùng chọn không hợp lệ để mã hóa PNG.".to_owned());
    }
    let mut png_bytes = Vec::new();
    let encoder = PngEncoder::new(&mut png_bytes);
    encoder
        .write_image(
            &frame.rgba,
            frame.width,
            frame.height,
            ExtendedColorType::Rgba8,
        )
        .map_err(|err| format!("Không thể mã hóa ảnh vùng chọn sang PNG: {err}"))?;
    Ok(png_bytes)
}

/// Converts a Slint UI pack identifier string (`"cjk-en"`, `"vi-latin"`, `"korean"`) into an
/// [`OcrLanguagePack`].
#[must_use]
pub fn pack_from_id(id: &str) -> Option<OcrLanguagePack> {
    match id.trim().to_ascii_lowercase().as_str() {
        "cjk-en" | "cjk_en" => Some(OcrLanguagePack::CjkEn),
        "vi-latin" | "vietnamese_latin" | "vi_latin" => Some(OcrLanguagePack::VietnameseLatin),
        "korean" => Some(OcrLanguagePack::Korean),
        _ => None,
    }
}

/// Returns the canonical Slint UI pack identifier string for `pack`.
#[must_use]
pub const fn pack_to_id(pack: OcrLanguagePack) -> &'static str {
    match pack {
        OcrLanguagePack::CjkEn => "cjk-en",
        OcrLanguagePack::VietnameseLatin => "vi-latin",
        OcrLanguagePack::Korean => "korean",
    }
}

/// Converts a Slint UI tier identifier string (`"mobile"`, `"server"`) into an [`OcrModelTier`].
#[must_use]
pub fn tier_from_id(id: &str) -> Option<OcrModelTier> {
    match id.trim().to_ascii_lowercase().as_str() {
        "mobile" => Some(OcrModelTier::Mobile),
        "server" => Some(OcrModelTier::Server),
        _ => None,
    }
}

/// Returns the canonical Slint UI tier identifier string for `tier`.
#[must_use]
pub const fn tier_to_id(tier: OcrModelTier) -> &'static str {
    match tier {
        OcrModelTier::Mobile => "mobile",
        OcrModelTier::Server => "server",
    }
}

/// Kind of snip OCR/translation action executed on a selection rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnipActionKind {
    /// Extract text via local OCR and copy to clipboard.
    OcrCopy,
    /// Extract text via local OCR and translate to the configured target language.
    QuickTranslate,
}

/// Cached snip action waiting for the first-run starter OCR model download to finish.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingSnipAction {
    /// Action kind (`OcrCopy` or `QuickTranslate`).
    pub kind: SnipActionKind,
    /// Logical selection rectangle inside the active freeze-frame overlay.
    pub rect: LogicalRect,
    /// Requested OCR language pack.
    pub pack: OcrLanguagePack,
}

/// Commands sent from Slint UI callbacks, system tray, and global hotkeys to [`AppController`].
#[allow(clippy::large_enum_variant)]
#[derive(Clone, PartialEq)]
pub enum AppCommand {
    /// Capture the active monitor freeze-frame and show the Square to Search overlay.
    TriggerSquareToSearch,
    /// Cancel and hide the Square to Search overlay, aborting any in-flight OCR/translate/AI task.
    CancelOverlay,
    /// Update the logical drag selection rectangle inside the overlay.
    UpdateSelection(LogicalRect),
    /// Crop the selected region and run visual reverse image search (works without OCR models).
    RunVisualSearch(LogicalRect),
    /// Crop the selected region, run local OCR, copy text to clipboard, and show the inline card.
    RunOcrCopy(LogicalRect),
    /// Crop the selected region, run local OCR + translation, and show the inline card.
    RunQuickTranslate(LogicalRect),
    /// Trigger Ask AI on the selected region (shows settings shortcut when no provider configured).
    RunAskAi(LogicalRect),
    /// Submit a prompt to the active multimodal Vision AI provider for the selected region.
    SubmitAskAiPrompt(String),
    /// Stop an in-flight Ask AI streaming response without closing the overlay card.
    StopAskAi,
    /// Copy arbitrary result text from the inline card to the OS clipboard.
    CopyResultText(String),
    /// Open a Google web search in the default browser for the given text query.
    SearchTextGoogle(String),
    /// Switch the active Square-to-Search OCR language pack (and re-run the active card action).
    SwitchSnipOcrPack(OcrLanguagePack),
    /// Start the 1-click starter OCR model download and auto-resume any pending snip/live action.
    StartStarterDownload,
    /// Dismiss the first-run starter OCR download modal without downloading.
    DismissFirstRun,
    /// Open the 5-tab Settings window focused on the given tab index (`0..=4`).
    OpenSettingsTab(i32),
    /// Persist updated application settings to `config.toml` and hot-reload providers.
    SaveSettings(AppConfig),
    /// Store or overwrite a BYOK API key exclusively in the OS keyring.
    SaveApiKey { slot: SecretSlot, value: String },
    /// Remove a BYOK API key from the OS keyring.
    ClearApiKey(SecretSlot),
    /// Send a lightweight probe request to verify the configured Live or Ask AI provider.
    TestAiProvider(ProviderTestTarget),
    /// Download a specific PP-OCRv5 model pack and tier from the manifest.
    DownloadModelPack {
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    },
    /// Delete an installed PP-OCRv5 model pack and tier from the local store.
    DeleteModelPack {
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    },
    /// Import an offline directory containing `det.onnx`, `rec.onnx`, and `dict.txt`.
    ImportOfflineModel {
        pack: OcrLanguagePack,
        tier: OcrModelTier,
        dir: PathBuf,
    },
    /// Toggle Continuous Live Translate (`Ctrl+Shift+T` or System Tray).
    ToggleLiveTranslate,
    /// Confirm the selected logical screen region to begin Continuous Live Translate.
    ConfirmLiveRegion(LogicalRect),
    /// Target an OS window directly by ID for Continuous Live Translate (Linux X11 anti-self-capture).
    SelectLiveWindow(u32),
    /// Pause or resume Continuous Live Translate capture and OCR/translation.
    ToggleLivePause,
    /// Toggle Live Translate overlay visibility (`Ctrl+Shift+H` or Hide/Show button).
    ToggleLiveVisibility,
    /// Switch active Live Translate display mode between In-place and Floating Panel.
    SwitchLiveDisplayMode,
    /// Stop the current Live Translate session and reopen region selection.
    ReselectLiveRegion,
    /// Stop the active Live Translate session and hide all Live overlay windows.
    StopLiveTranslate,
    /// Copy a translated Live dialogue block to the OS clipboard.
    CopyLiveBlock(String),
    /// Quit the application event loop cleanly.
    Quit,
}

impl fmt::Debug for AppCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TriggerSquareToSearch => f.write_str("TriggerSquareToSearch"),
            Self::CancelOverlay => f.write_str("CancelOverlay"),
            Self::UpdateSelection(rect) => f.debug_tuple("UpdateSelection").field(rect).finish(),
            Self::RunVisualSearch(rect) => f.debug_tuple("RunVisualSearch").field(rect).finish(),
            Self::RunOcrCopy(rect) => f.debug_tuple("RunOcrCopy").field(rect).finish(),
            Self::RunQuickTranslate(rect) => {
                f.debug_tuple("RunQuickTranslate").field(rect).finish()
            }
            Self::RunAskAi(rect) => f.debug_tuple("RunAskAi").field(rect).finish(),
            Self::SubmitAskAiPrompt(prompt) => {
                f.debug_tuple("SubmitAskAiPrompt").field(prompt).finish()
            }
            Self::StopAskAi => f.write_str("StopAskAi"),
            Self::CopyResultText(text) => f.debug_tuple("CopyResultText").field(text).finish(),
            Self::SearchTextGoogle(query) => {
                f.debug_tuple("SearchTextGoogle").field(query).finish()
            }
            Self::SwitchSnipOcrPack(pack) => {
                f.debug_tuple("SwitchSnipOcrPack").field(pack).finish()
            }
            Self::StartStarterDownload => f.write_str("StartStarterDownload"),
            Self::DismissFirstRun => f.write_str("DismissFirstRun"),
            Self::OpenSettingsTab(tab) => f.debug_tuple("OpenSettingsTab").field(tab).finish(),
            Self::SaveSettings(cfg) => f.debug_tuple("SaveSettings").field(cfg).finish(),
            Self::SaveApiKey { slot, .. } => f
                .debug_struct("SaveApiKey")
                .field("slot", slot)
                .field("value", &"[REDACTED]")
                .finish(),
            Self::ClearApiKey(slot) => f.debug_tuple("ClearApiKey").field(slot).finish(),
            Self::TestAiProvider(target) => f.debug_tuple("TestAiProvider").field(target).finish(),
            Self::DownloadModelPack { pack, tier } => f
                .debug_struct("DownloadModelPack")
                .field("pack", pack)
                .field("tier", tier)
                .finish(),
            Self::DeleteModelPack { pack, tier } => f
                .debug_struct("DeleteModelPack")
                .field("pack", pack)
                .field("tier", tier)
                .finish(),
            Self::ImportOfflineModel { pack, tier, dir } => f
                .debug_struct("ImportOfflineModel")
                .field("pack", pack)
                .field("tier", tier)
                .field("dir", dir)
                .finish(),
            Self::ToggleLiveTranslate => f.write_str("ToggleLiveTranslate"),
            Self::ConfirmLiveRegion(rect) => {
                f.debug_tuple("ConfirmLiveRegion").field(rect).finish()
            }
            Self::SelectLiveWindow(id) => f.debug_tuple("SelectLiveWindow").field(id).finish(),
            Self::ToggleLivePause => f.write_str("ToggleLivePause"),
            Self::ToggleLiveVisibility => f.write_str("ToggleLiveVisibility"),
            Self::SwitchLiveDisplayMode => f.write_str("SwitchLiveDisplayMode"),
            Self::ReselectLiveRegion => f.write_str("ReselectLiveRegion"),
            Self::StopLiveTranslate => f.write_str("StopLiveTranslate"),
            Self::CopyLiveBlock(text) => f.debug_tuple("CopyLiveBlock").field(text).finish(),
            Self::Quit => f.write_str("Quit"),
        }
    }
}

/// UI update events emitted by [`AppController`] to drive Slint windows.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    /// Display the freeze-frame Square to Search overlay for the captured monitor.
    ShowSnipOverlay {
        monitor: MonitorGeometry,
        frame: CaptureFrame,
    },
    /// Display `SnipOverlayWindow` in Live Translate region-picker mode.
    ShowLiveRegionPicker {
        monitor: MonitorGeometry,
        frame: CaptureFrame,
    },
    /// Hide the Square to Search / Live region-picker overlay and reset its transient card state.
    HideSnipOverlay,
    /// Confirm an updated logical selection rectangle on the overlay.
    SelectionUpdated(LogicalRect),
    /// Switch the inline card into indeterminate loading state during OCR/translation.
    ShowCardLoading,
    /// Display extracted OCR text (and optional translated text) in the inline quick card.
    ShowInlineResult {
        ocr_text: String,
        translated_text: Option<String>,
        pack: OcrLanguagePack,
        status_text: String,
    },
    /// Display an error message inside the inline quick card.
    ShowCardError { message: String },
    /// Display the "Ask AI provider unconfigured" card with a 1-click Settings shortcut.
    ShowAskAiUnconfigured,
    /// Display the interactive Ask AI streaming card ready for prompt input or preset click.
    ShowAskAiReady { provider_label: String },
    /// Notify the overlay card that an Ask AI stream has started.
    AskAiStreamStarted { provider_label: String },
    /// Deliver an incremental Ask AI answer chunk onto the streaming card.
    AskAiStreamDelta { chunk: String },
    /// Notify the overlay card that the Ask AI stream completed (or was stopped cleanly).
    AskAiStreamFinished {
        full_answer: String,
        provider_label: String,
    },
    /// Notify the overlay card that an Ask AI streaming request failed.
    AskAiStreamError { message: String },
    /// Notify that a visual search request completed.
    VisualSearchDone(VisualSearchOutcome),
    /// Open the first-run 1-click starter OCR download modal for `(pack, tier)`.
    ShowFirstRunOcrModal {
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    },
    /// Update progress (`0.0..=1.0`) and status text on the first-run starter download modal.
    StarterDownloadProgress { progress: f32, status_text: String },
    /// Display an error message on the first-run starter download modal.
    StarterDownloadFailed { message: String },
    /// Close the first-run starter download modal.
    HideFirstRunOcrModal,
    /// Open the 5-tab Settings window populated with current configuration, model statuses, and
    /// masked keyring presence flags.
    ShowSettings {
        tab: i32,
        config: Box<AppConfig>,
        model_statuses: Vec<PackInstallStatus>,
        show_wayland_notice: bool,
        has_gemini_key: bool,
        has_openai_key: bool,
        has_deepl_key: bool,
    },
    /// Confirm that settings were persisted to `config.toml`.
    SettingsSaved {
        config: Box<AppConfig>,
        status_text: String,
    },
    /// Confirm keyring presence updates after saving or clearing a BYOK API key.
    KeyringStatusUpdated {
        has_gemini: bool,
        has_openai: bool,
        has_deepl: bool,
        ai_provider_configured: bool,
        status_text: String,
    },
    /// Report the outcome of [`AppCommand::TestAiProvider`].
    ProviderTestFinished { ok: bool, message: String },
    /// Update per-pack download progress in the Settings OCR & Models tab.
    ModelPackDownloadProgress {
        pack: OcrLanguagePack,
        tier: OcrModelTier,
        progress: f32,
        status_text: String,
    },
    /// Refresh the full list of model pack installation statuses.
    ModelPacksUpdated(Vec<PackInstallStatus>),
    /// Notify UI that a Continuous Live Translate session has started.
    LiveSessionStarted {
        display_mode: LiveDisplayMode,
        can_inplace: bool,
        overlay_monitor: MonitorGeometry,
        overlay_physical_rect: PhysicalRect,
        notice: Option<String>,
    },
    /// Deliver freshly clustered & translated Live speech-bubble blocks to Live windows.
    LiveBlocksUpdated {
        blocks: Vec<LiveRenderedBlock>,
        status_text: String,
    },
    /// Notify UI that the Live Translate pause state changed.
    LivePauseChanged(bool),
    /// Notify UI that Live Translate overlay visibility changed.
    LiveVisibilityChanged(bool),
    /// Notify UI that the active Live Translate display mode changed.
    LiveDisplayModeChanged(LiveDisplayMode),
    /// Notify UI that the translator hit HTTP 429 / rate limit while keeping the session alive.
    LiveRateLimited { message: String },
    /// Notify UI that the active Live Translate session stopped cleanly.
    LiveSessionStopped,
    /// Show a non-blocking status toast message.
    ShowToast { message: String },
    /// Terminate the UI event loop.
    Quit,
}

/// Dependency container injected into [`AppController`].
pub struct AppServices {
    pub config_store: ConfigStore,
    pub model_store: ModelStore,
    pub model_manifest: ModelManifest,
    pub downloader: Arc<dyn PackDownloader>,
    pub capturer: Arc<dyn ScreenCapturer>,
    pub clipboard: Arc<dyn ClipboardService>,
    pub ocr: Arc<dyn OcrEngine>,
    pub translator: Arc<dyn Translator>,
    pub visual_search: Arc<dyn VisualSearcher>,
    pub browser: Arc<dyn BrowserOpener>,
    pub display_server: DisplayServer,
    pub secrets: Arc<dyn SecretStore>,
    pub translation_cache: Arc<AsyncMutex<TranslationCache>>,
    pub provider_builder: Arc<dyn ProviderBuilder>,
}

struct SharedTranslator {
    inner: RwLock<Arc<dyn Translator>>,
}

impl SharedTranslator {
    fn new(initial: Arc<dyn Translator>) -> Self {
        Self {
            inner: RwLock::new(initial),
        }
    }

    fn current(&self) -> Arc<dyn Translator> {
        match self.inner.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    fn replace(&self, next: Arc<dyn Translator>) {
        match self.inner.write() {
            Ok(mut guard) => *guard = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
    }
}

impl Translator for SharedTranslator {
    fn provider_name(&self) -> &'static str {
        self.current().provider_name()
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        let translator = self.current();
        Box::pin(async move { translator.translate(request).await })
    }
}

#[derive(Debug, Clone)]
struct ActiveCapture {
    monitor: MonitorGeometry,
    frame: CaptureFrame,
}

struct ActiveLiveSession {
    handle: LiveSessionHandle,
    display_mode: LiveDisplayMode,
    can_inplace: bool,
    target_is_fullscreen: bool,
    overlay_visible: bool,
}

struct ActiveAskAiStream {
    active: Arc<AtomicBool>,
    partial: Arc<StdMutex<String>>,
    provider_label: String,
}

/// Core asynchronous state machine orchestrating Square to Search, Continuous Live Translate,
/// OCR model management, translation, visual search, BYOK / OS Keyring secrets, and persistent
/// settings.
pub struct AppController {
    services: AppServices,
    config: AppConfig,
    shared_translator: Arc<SharedTranslator>,
    vision_ai: Option<Arc<dyn VisionAiProvider>>,
    secrets_snapshot: ProviderSecrets,
    secrets_loaded: bool,
    providers_initialized: bool,
    active_snip_pack: OcrLanguagePack,
    active_capture: Option<ActiveCapture>,
    last_selection: Option<LogicalRect>,
    last_action_kind: Option<SnipActionKind>,
    pending_action: Option<PendingSnipAction>,
    live_selecting_region: bool,
    pending_live_start: bool,
    live_session: Option<ActiveLiveSession>,
    cancel_tx: watch::Sender<bool>,
    in_flight_task: Option<JoinHandle<()>>,
    active_ask_ai: Option<ActiveAskAiStream>,
    test_task: Option<JoinHandle<()>>,
    cmd_rx: UnboundedReceiver<AppCommand>,
    ui_tx: UnboundedSender<UiEvent>,
}

impl AppController {
    /// Creates a new [`AppController`] loading initial configuration from `services.config_store`.
    pub fn new(
        services: AppServices,
        cmd_rx: UnboundedReceiver<AppCommand>,
        ui_tx: UnboundedSender<UiEvent>,
    ) -> anyhow::Result<Self> {
        let outcome = services.config_store.load_or_default()?;
        if let ConfigLoadOutcome::RecoveredFromCorrupt { backup_path, .. } = &outcome {
            tracing::warn!(
                backup = %backup_path.display(),
                "corrupt config.toml was backed up and recovered with default settings"
            );
        }
        let config = outcome.into_config();
        let active_snip_pack = config.ocr.snip_pack;
        let shared_translator = Arc::new(SharedTranslator::new(Arc::clone(&services.translator)));
        let (cancel_tx, _) = watch::channel(false);

        Ok(Self {
            services,
            config,
            shared_translator,
            vision_ai: None,
            secrets_snapshot: ProviderSecrets::default(),
            secrets_loaded: false,
            providers_initialized: false,
            active_snip_pack,
            active_capture: None,
            last_selection: None,
            last_action_kind: None,
            pending_action: None,
            live_selecting_region: false,
            pending_live_start: false,
            live_session: None,
            cancel_tx,
            in_flight_task: None,
            active_ask_ai: None,
            test_task: None,
            cmd_rx,
            ui_tx,
        })
    }

    /// Spawns an [`AppController`] task on the current Tokio runtime and returns its command
    /// sender, UI event receiver, and task join handle.
    pub fn spawn(
        services: AppServices,
    ) -> anyhow::Result<(
        UnboundedSender<AppCommand>,
        UnboundedReceiver<UiEvent>,
        JoinHandle<()>,
    )> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = Self::new(services, cmd_rx, ui_tx)?;
        let handle = tokio::spawn(controller.run());
        Ok((cmd_tx, ui_rx, handle))
    }

    /// Runs the controller command loop until the command channel closes or [`AppCommand::Quit`]
    /// is received.
    pub async fn run(mut self) {
        self.ensure_providers_initialized().await;
        while let Some(cmd) = self.cmd_rx.recv().await {
            if !self.handle_command(cmd).await {
                break;
            }
        }
        self.stop_live_session_silent();
        self.abort_in_flight_task().await;
        self.abort_test_task().await;
    }

    /// Processes a single [`AppCommand`]. Returns `false` when [`AppCommand::Quit`] is processed.
    pub async fn handle_command(&mut self, cmd: AppCommand) -> bool {
        match cmd {
            AppCommand::TriggerSquareToSearch => {
                self.active_ask_ai = None;
                self.abort_in_flight_task().await;
                self.pending_action = None;
                self.pending_live_start = false;
                self.live_selecting_region = false;
                self.last_selection = None;
                self.last_action_kind = None;
                self.active_snip_pack = self.config.ocr.snip_pack;

                match self.services.capturer.capture_active_monitor() {
                    Ok((monitor, frame)) => {
                        self.active_capture = Some(ActiveCapture {
                            monitor: monitor.clone(),
                            frame: frame.clone(),
                        });
                        let _ = self.ui_tx.send(UiEvent::ShowSnipOverlay { monitor, frame });
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "failed to capture active monitor");
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!("Không thể chụp màn hình: {err}"),
                        });
                    }
                }
            }

            AppCommand::CancelOverlay => {
                self.active_ask_ai = None;
                self.abort_in_flight_task().await;
                self.active_capture = None;
                self.last_selection = None;
                self.last_action_kind = None;
                self.pending_action = None;
                self.live_selecting_region = false;
                let _ = self.ui_tx.send(UiEvent::HideSnipOverlay);
            }

            AppCommand::UpdateSelection(rect) => {
                self.last_selection = Some(rect);
                let _ = self.ui_tx.send(UiEvent::SelectionUpdated(rect));
            }

            AppCommand::RunVisualSearch(rect) => {
                self.active_ask_ai = None;
                self.abort_in_flight_task().await;
                self.last_selection = Some(rect);

                let cropped = match self.crop_active_selection(rect) {
                    Ok(frame) => frame,
                    Err(message) => {
                        let _ = self.ui_tx.send(UiEvent::ShowCardError { message });
                        return true;
                    }
                };

                let mut cancel_rx = self.reset_cancellation().await;
                let visual_search = Arc::clone(&self.services.visual_search);
                let search_cfg = self.config.search.clone();
                let ui_tx = self.ui_tx.clone();

                let handle = tokio::spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => return,
                        res = visual_search.search(&cropped, &search_cfg) => res,
                    };
                    if *cancel_rx.borrow() {
                        return;
                    }
                    match result {
                        Ok(outcome) => {
                            if let VisualSearchOutcome::FallbackClipboard { .. } = &outcome {
                                let _ = ui_tx.send(UiEvent::ShowToast {
                                    message: "Upload trực tiếp lỗi; đã copy ảnh vào Clipboard — nhấn Ctrl+V trên trang tìm ảnh.".to_owned(),
                                });
                            }
                            let _ = ui_tx.send(UiEvent::VisualSearchDone(outcome));
                            let _ = ui_tx.send(UiEvent::HideSnipOverlay);
                        }
                        Err(err) => {
                            let _ = ui_tx.send(UiEvent::ShowCardError {
                                message: err.to_string(),
                            });
                        }
                    }
                });
                self.in_flight_task = Some(handle);
            }

            AppCommand::RunOcrCopy(rect) => {
                self.start_snip_ocr_action(SnipActionKind::OcrCopy, rect, self.active_snip_pack)
                    .await;
            }

            AppCommand::RunQuickTranslate(rect) => {
                self.ensure_providers_initialized().await;
                self.start_snip_ocr_action(
                    SnipActionKind::QuickTranslate,
                    rect,
                    self.active_snip_pack,
                )
                .await;
            }

            AppCommand::RunAskAi(rect) => {
                self.ensure_providers_initialized().await;
                self.last_selection = Some(rect);
                if let Err(message) = self.crop_active_selection(rect) {
                    let _ = self.ui_tx.send(UiEvent::ShowCardError { message });
                    return true;
                }
                if let Some(provider) = &self.vision_ai {
                    let provider_label = vision_provider_display_label(&self.config.translate)
                        .unwrap_or_else(|| provider.provider_name().to_owned());
                    let _ = self.ui_tx.send(UiEvent::ShowAskAiReady { provider_label });
                } else {
                    let _ = self.ui_tx.send(UiEvent::ShowAskAiUnconfigured);
                }
            }

            AppCommand::SubmitAskAiPrompt(prompt) => {
                self.ensure_providers_initialized().await;
                let trimmed_prompt = prompt.trim().to_owned();
                if trimmed_prompt.is_empty() {
                    let _ = self.ui_tx.send(UiEvent::AskAiStreamError {
                        message: "Vui lòng nhập câu hỏi trước khi gửi tới AI.".to_owned(),
                    });
                    return true;
                }

                let Some(provider) = self.vision_ai.as_ref().map(Arc::clone) else {
                    let _ = self.ui_tx.send(UiEvent::ShowAskAiUnconfigured);
                    return true;
                };

                let Some(rect) = self.last_selection else {
                    let _ = self.ui_tx.send(UiEvent::AskAiStreamError {
                        message: "Chưa có vùng chọn nào để gửi tới AI.".to_owned(),
                    });
                    return true;
                };

                let cropped = match self.crop_active_selection(rect) {
                    Ok(frame) => frame,
                    Err(message) => {
                        let _ = self.ui_tx.send(UiEvent::AskAiStreamError { message });
                        return true;
                    }
                };

                let png_bytes = match encode_frame_as_png(&cropped) {
                    Ok(bytes) => bytes,
                    Err(message) => {
                        let _ = self.ui_tx.send(UiEvent::AskAiStreamError { message });
                        return true;
                    }
                };

                let provider_label = vision_provider_display_label(&self.config.translate)
                    .unwrap_or_else(|| provider.provider_name().to_owned());

                self.active_ask_ai = None;
                let mut cancel_rx = self.reset_cancellation().await;

                let active_flag = Arc::new(AtomicBool::new(true));
                let partial_buf = Arc::new(StdMutex::new(String::new()));
                self.active_ask_ai = Some(ActiveAskAiStream {
                    active: Arc::clone(&active_flag),
                    partial: Arc::clone(&partial_buf),
                    provider_label: provider_label.clone(),
                });

                let _ = self.ui_tx.send(UiEvent::AskAiStreamStarted {
                    provider_label: provider_label.clone(),
                });

                let pack = self.active_snip_pack;
                let tier = self.config.ocr.tier;
                let ocr_installed = self.services.model_store.is_installed(pack, tier);
                let ocr = Arc::clone(&self.services.ocr);
                let ui_tx = self.ui_tx.clone();

                let handle = tokio::spawn(async move {
                    let ocr_text = if ocr_installed {
                        let ocr_res = tokio::select! {
                            biased;
                            _ = cancel_rx.changed() => {
                                active_flag.store(false, Ordering::SeqCst);
                                return;
                            }
                            res = ocr.recognize(&cropped, pack, tier) => res,
                        };
                        if *cancel_rx.borrow() {
                            active_flag.store(false, Ordering::SeqCst);
                            return;
                        }
                        ocr_res
                            .ok()
                            .map(|lines| join_ocr_lines(&lines))
                            .filter(|text| !text.is_empty())
                    } else {
                        None
                    };

                    let request = VisionAiRequest {
                        image_png: Some(png_bytes),
                        ocr_text,
                        prompt: trimmed_prompt,
                    };

                    let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let ask_fut = provider.ask_stream(&request, delta_tx);
                    tokio::pin!(ask_fut);
                    let mut ask_result: Option<Result<VisionAiResponse, TranslateError>> = None;

                    loop {
                        tokio::select! {
                            biased;
                            _ = cancel_rx.changed() => {
                                active_flag.store(false, Ordering::SeqCst);
                                return;
                            }
                            maybe_chunk = delta_rx.recv() => {
                                if *cancel_rx.borrow() {
                                    active_flag.store(false, Ordering::SeqCst);
                                    return;
                                }
                                match maybe_chunk {
                                    Some(chunk) => {
                                        if !chunk.is_empty() {
                                            if let Ok(mut guard) = partial_buf.lock() {
                                                guard.push_str(&chunk);
                                            }
                                            let _ = ui_tx.send(UiEvent::AskAiStreamDelta { chunk });
                                        }
                                    }
                                    None => {
                                        if ask_result.is_some() {
                                            break;
                                        }
                                    }
                                }
                            }
                            res = &mut ask_fut, if ask_result.is_none() => {
                                ask_result = Some(res);
                            }
                        }
                    }

                    if *cancel_rx.borrow() {
                        active_flag.store(false, Ordering::SeqCst);
                        return;
                    }

                    if active_flag.swap(false, Ordering::SeqCst) {
                        match ask_result {
                            Some(Ok(resp)) => {
                                let full_answer = if resp.answer.is_empty() {
                                    partial_buf.lock().map(|g| g.clone()).unwrap_or_default()
                                } else {
                                    resp.answer
                                };
                                let _ = ui_tx.send(UiEvent::AskAiStreamFinished {
                                    full_answer,
                                    provider_label,
                                });
                            }
                            Some(Err(err)) => {
                                let _ = ui_tx.send(UiEvent::AskAiStreamError {
                                    message: err.to_string(),
                                });
                            }
                            None => {
                                let full_answer =
                                    partial_buf.lock().map(|g| g.clone()).unwrap_or_default();
                                let _ = ui_tx.send(UiEvent::AskAiStreamFinished {
                                    full_answer,
                                    provider_label,
                                });
                            }
                        }
                    }
                });

                self.in_flight_task = Some(handle);
            }

            AppCommand::StopAskAi => {
                self.abort_in_flight_task().await;
                if let Some(stream) = self.active_ask_ai.take()
                    && stream.active.swap(false, Ordering::SeqCst)
                {
                    let partial = stream.partial.lock().map(|g| g.clone()).unwrap_or_default();
                    let _ = self.ui_tx.send(UiEvent::AskAiStreamFinished {
                        full_answer: partial,
                        provider_label: stream.provider_label,
                    });
                }
            }

            AppCommand::CopyResultText(text) => match self.services.clipboard.set_text(&text) {
                Ok(()) => {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: "Đã sao chép văn bản vào Clipboard.".to_owned(),
                    });
                }
                Err(err) => {
                    let _ = self.ui_tx.send(UiEvent::ShowCardError {
                        message: err.to_string(),
                    });
                }
            },

            AppCommand::SearchTextGoogle(query) => {
                let trimmed = query.trim();
                match Url::parse_with_params("https://www.google.com/search", &[("q", trimmed)]) {
                    Ok(search_url) => match self.services.browser.open_url(search_url.as_str()) {
                        Ok(()) => {
                            let _ = self.ui_tx.send(UiEvent::ShowToast {
                                message: "Đã mở tìm kiếm Google trên trình duyệt.".to_owned(),
                            });
                        }
                        Err(err) => {
                            let _ = self.ui_tx.send(UiEvent::ShowCardError {
                                message: err.to_string(),
                            });
                        }
                    },
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ShowCardError {
                            message: format!("Không thể tạo URL tìm kiếm Google: {err}"),
                        });
                    }
                }
            }

            AppCommand::SwitchSnipOcrPack(pack) => {
                self.active_snip_pack = pack;
                if let (Some(kind), Some(rect)) = (self.last_action_kind, self.last_selection)
                    && self.active_capture.is_some()
                {
                    self.start_snip_ocr_action(kind, rect, pack).await;
                }
            }

            AppCommand::StartStarterDownload => {
                let (pack, tier) = if let Some(pending) = self.pending_action {
                    (pending.pack, self.config.ocr.tier)
                } else if self.pending_live_start {
                    (self.config.ocr.live_pack, self.config.ocr.tier)
                } else {
                    (OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
                };

                let Some(spec) = self.services.model_manifest.find(pack, tier).cloned() else {
                    let _ = self.ui_tx.send(UiEvent::StarterDownloadFailed {
                        message: format!(
                            "Không tìm thấy gói model {pack:?} ({tier:?}) trong manifest."
                        ),
                    });
                    return true;
                };

                let _ = self.ui_tx.send(UiEvent::StarterDownloadProgress {
                    progress: 0.0,
                    status_text: format!("Đang tải {}...", spec.display_name),
                });

                let ui_tx = self.ui_tx.clone();
                let progress_cb: ProgressCallback = Arc::new(move |p: DownloadProgress| {
                    let progress = if p.total_bytes > 0 {
                        (p.bytes_downloaded as f32 / p.total_bytes as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let _ = ui_tx.send(UiEvent::StarterDownloadProgress {
                        progress,
                        status_text: format!(
                            "Đang tải {} ({}/{})",
                            p.artifact,
                            p.artifact_index + 1,
                            p.artifact_count
                        ),
                    });
                });

                let download_res = self
                    .services
                    .downloader
                    .download_pack(&self.services.model_store, &spec, progress_cb)
                    .await;

                match download_res {
                    Ok(_) => {
                        let statuses = self
                            .services
                            .model_store
                            .list_statuses(&self.services.model_manifest);
                        let _ = self.ui_tx.send(UiEvent::ModelPacksUpdated(statuses));
                        let _ = self.ui_tx.send(UiEvent::HideFirstRunOcrModal);

                        if let Some(pending) = self.pending_action.take() {
                            self.start_snip_ocr_action(pending.kind, pending.rect, pending.pack)
                                .await;
                        } else if self.pending_live_start {
                            self.pending_live_start = false;
                            self.begin_live_region_picker().await;
                        }
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::StarterDownloadFailed {
                            message: err.to_string(),
                        });
                    }
                }
            }

            AppCommand::DismissFirstRun => {
                self.pending_action = None;
                self.pending_live_start = false;
                let _ = self.ui_tx.send(UiEvent::HideFirstRunOcrModal);
            }

            AppCommand::OpenSettingsTab(tab) => {
                if let Ok(secrets) = load_provider_secrets(&self.services.secrets).await {
                    self.secrets_snapshot = secrets;
                    self.secrets_loaded = true;
                }
                let (has_gemini_key, has_openai_key, has_deepl_key) =
                    secret_presence_flags(&self.secrets_snapshot);
                let model_statuses = self
                    .services
                    .model_store
                    .list_statuses(&self.services.model_manifest);
                let show_wayland_notice = self.services.display_server.is_wayland();
                let _ = self.ui_tx.send(UiEvent::ShowSettings {
                    tab: tab.clamp(0, 4),
                    config: Box::new(self.config.clone()),
                    model_statuses,
                    show_wayland_notice,
                    has_gemini_key,
                    has_openai_key,
                    has_deepl_key,
                });
            }

            AppCommand::SaveSettings(updated) => match self.services.config_store.save(&updated) {
                Ok(()) => {
                    let translate_changed = self.config.translate != updated.translate;
                    self.config = updated.clone();
                    self.active_snip_pack = self.config.ocr.snip_pack;
                    if translate_changed {
                        self.services.translation_cache.lock().await.clear();
                    }
                    let fallback_notice = self.reload_providers(true).await;

                    let _ = self.ui_tx.send(UiEvent::SettingsSaved {
                        config: Box::new(updated),
                        status_text: "Đã lưu cài đặt vào config.toml.".to_owned(),
                    });
                    let statuses = self
                        .services
                        .model_store
                        .list_statuses(&self.services.model_manifest);
                    let _ = self.ui_tx.send(UiEvent::ModelPacksUpdated(statuses));
                    if let Some(message) = fallback_notice {
                        let _ = self.ui_tx.send(UiEvent::ShowToast { message });
                    }
                }
                Err(err) => {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: format!("Lỗi lưu cài đặt: {err}"),
                    });
                }
            },

            AppCommand::SaveApiKey { slot, value } => {
                match save_provider_secret(&self.services.secrets, slot, value).await {
                    Ok(()) => {
                        self.finalize_keyring_mutation(format!(
                            "Đã lưu {} API Key vào OS Keyring.",
                            slot.display_name()
                        ))
                        .await;
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!("Lỗi lưu {} API Key: {err}", slot.display_name()),
                        });
                    }
                }
            }

            AppCommand::ClearApiKey(slot) => {
                match clear_provider_secret(&self.services.secrets, slot).await {
                    Ok(()) => {
                        self.finalize_keyring_mutation(format!(
                            "Đã xóa {} API Key khỏi OS Keyring.",
                            slot.display_name()
                        ))
                        .await;
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!("Lỗi xóa {} API Key: {err}", slot.display_name()),
                        });
                    }
                }
            }

            AppCommand::TestAiProvider(target) => {
                self.ensure_providers_initialized().await;
                self.abort_test_task().await;

                let ui_tx = self.ui_tx.clone();
                let translate_cfg = self.config.translate.clone();
                let secrets = self.secrets_snapshot.clone();
                let provider_builder = Arc::clone(&self.services.provider_builder);
                let active_translator = self.shared_translator.current();

                let handle = tokio::spawn(async move {
                    let event = match target {
                        ProviderTestTarget::LiveTranslator => {
                            let probe_translator = if is_free_web_provider(&translate_cfg.provider)
                            {
                                Ok(active_translator)
                            } else {
                                let probe_cache =
                                    Arc::new(AsyncMutex::new(TranslationCache::new(4)));
                                provider_builder.build_translator(
                                    &translate_cfg,
                                    &secrets,
                                    probe_cache,
                                )
                            };

                            match probe_translator {
                                Ok(translator) => {
                                    let target_lang = if translate_cfg.target_lang.trim().is_empty()
                                    {
                                        "vi".to_owned()
                                    } else {
                                        translate_cfg.target_lang.clone()
                                    };
                                    let req = TranslateRequest::new(
                                        "en",
                                        target_lang,
                                        vec!["Hello".to_owned()],
                                    );
                                    match translator.translate(&req).await {
                                        Ok(resp) => {
                                            let preview = resp
                                                .translations
                                                .first()
                                                .map(String::as_str)
                                                .unwrap_or("");
                                            UiEvent::ProviderTestFinished {
                                                ok: true,
                                                message: format!(
                                                    "Kết nối Dịch Live thành công ({}) -> \"{}\"",
                                                    resp.provider_name, preview
                                                ),
                                            }
                                        }
                                        Err(err) => UiEvent::ProviderTestFinished {
                                            ok: false,
                                            message: format!("Kiểm tra Dịch Live thất bại: {err}"),
                                        },
                                    }
                                }
                                Err(err) => UiEvent::ProviderTestFinished {
                                    ok: false,
                                    message: format!("Dịch Live chưa sẵn sàng: {err}"),
                                },
                            }
                        }
                        ProviderTestTarget::AskAi => {
                            match provider_builder.build_vision_ai(&translate_cfg, &secrets) {
                                Ok(Some(provider)) => {
                                    let req = VisionAiRequest {
                                        image_png: None,
                                        ocr_text: None,
                                        prompt: "Reply OK".to_owned(),
                                    };
                                    match provider.ask(&req).await {
                                        Ok(resp) => UiEvent::ProviderTestFinished {
                                            ok: true,
                                            message: format!(
                                                "Kết nối Hỏi AI thành công ({}) -> \"{}\"",
                                                resp.provider_name,
                                                resp.answer.trim()
                                            ),
                                        },
                                        Err(err) => UiEvent::ProviderTestFinished {
                                            ok: false,
                                            message: format!("Kiểm tra Hỏi AI thất bại: {err}"),
                                        },
                                    }
                                }
                                Ok(None) => UiEvent::ProviderTestFinished {
                                    ok: false,
                                    message: "Chưa chọn nhà cung cấp Hỏi AI.".to_owned(),
                                },
                                Err(err) => UiEvent::ProviderTestFinished {
                                    ok: false,
                                    message: format!("Hỏi AI chưa sẵn sàng: {err}"),
                                },
                            }
                        }
                    };
                    let _ = ui_tx.send(event);
                });
                self.test_task = Some(handle);
            }

            AppCommand::DownloadModelPack { pack, tier } => {
                let Some(spec) = self.services.model_manifest.find(pack, tier).cloned() else {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: format!(
                            "Không tìm thấy gói model {pack:?} ({tier:?}) trong manifest."
                        ),
                    });
                    return true;
                };

                let ui_tx = self.ui_tx.clone();
                let progress_cb: ProgressCallback = Arc::new(move |p: DownloadProgress| {
                    let progress = if p.total_bytes > 0 {
                        (p.bytes_downloaded as f32 / p.total_bytes as f32).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let _ = ui_tx.send(UiEvent::ModelPackDownloadProgress {
                        pack,
                        tier,
                        progress,
                        status_text: format!(
                            "Đang tải {} ({}/{})",
                            p.artifact,
                            p.artifact_index + 1,
                            p.artifact_count
                        ),
                    });
                });

                let res = self
                    .services
                    .downloader
                    .download_pack(&self.services.model_store, &spec, progress_cb)
                    .await;

                match res {
                    Ok(_) => {
                        let statuses = self
                            .services
                            .model_store
                            .list_statuses(&self.services.model_manifest);
                        let _ = self.ui_tx.send(UiEvent::ModelPacksUpdated(statuses));
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ModelPackDownloadProgress {
                            pack,
                            tier,
                            progress: 0.0,
                            status_text: format!("Lỗi tải gói: {err}"),
                        });
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!("Lỗi tải gói model: {err}"),
                        });
                    }
                }
            }

            AppCommand::DeleteModelPack { pack, tier } => {
                match self.services.model_store.delete_pack(pack, tier) {
                    Ok(()) => {
                        let statuses = self
                            .services
                            .model_store
                            .list_statuses(&self.services.model_manifest);
                        let _ = self.ui_tx.send(UiEvent::ModelPacksUpdated(statuses));
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!("Lỗi xóa gói model: {err}"),
                        });
                    }
                }
            }

            AppCommand::ImportOfflineModel { pack, tier, dir } => {
                match self
                    .services
                    .model_store
                    .import_offline_dir(pack, tier, &dir)
                {
                    Ok(()) => {
                        let statuses = self
                            .services
                            .model_store
                            .list_statuses(&self.services.model_manifest);
                        let _ = self.ui_tx.send(UiEvent::ModelPacksUpdated(statuses));
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!("Lỗi nhập model offline: {err}"),
                        });
                    }
                }
            }

            AppCommand::ToggleLiveTranslate => {
                if self.live_session.is_some() {
                    self.stop_live_session();
                } else if self.live_selecting_region {
                    self.live_selecting_region = false;
                    self.active_capture = None;
                    let _ = self.ui_tx.send(UiEvent::HideSnipOverlay);
                } else {
                    self.begin_live_region_picker().await;
                }
            }

            AppCommand::ConfirmLiveRegion(rect) => {
                self.stop_live_session_silent();
                self.live_selecting_region = false;

                let Some(active) = self.active_capture.take() else {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: "Chưa có khung hình chọn vùng Live Translate.".to_owned(),
                    });
                    return true;
                };

                let crop_physical = logical_to_physical_crop(rect, &active.monitor);
                if crop_physical.is_empty() {
                    let _ = self.ui_tx.send(UiEvent::HideSnipOverlay);
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: "Vùng chọn Live Translate quá nhỏ hoặc không hợp lệ.".to_owned(),
                    });
                    return true;
                }

                let pack = self.config.ocr.live_pack;
                let tier = self.config.ocr.tier;
                if !self.services.model_store.is_installed(pack, tier) {
                    self.pending_live_start = true;
                    let _ = self.ui_tx.send(UiEvent::HideSnipOverlay);
                    let _ = self
                        .ui_tx
                        .send(UiEvent::ShowFirstRunOcrModal { pack, tier });
                    return true;
                }

                let _ = self.ui_tx.send(UiEvent::HideSnipOverlay);

                let target = LiveTarget::ScreenRegion {
                    monitor: active.monitor.clone(),
                    crop_physical,
                    crop_logical: rect,
                };
                self.ensure_providers_initialized().await;
                self.start_live_session_for_target(target, active.monitor, crop_physical, None);
            }

            AppCommand::SelectLiveWindow(window_id) => {
                self.stop_live_session_silent();
                self.live_selecting_region = false;

                let pack = self.config.ocr.live_pack;
                let tier = self.config.ocr.tier;
                if !self.services.model_store.is_installed(pack, tier) {
                    self.pending_live_start = true;
                    let _ = self
                        .ui_tx
                        .send(UiEvent::ShowFirstRunOcrModal { pack, tier });
                    return true;
                }

                match self.services.capturer.capture_window(window_id) {
                    Ok((geom, _frame)) => {
                        let target_fullscreen_override =
                            if self.services.display_server.is_wayland() {
                                let monitors =
                                    self.services.capturer.list_monitors().unwrap_or_default();
                                Some(find_enclosing_monitor(&monitors, &geom).is_some_and(|mon| {
                                    is_rect_fullscreen_or_maximized(
                                        PhysicalRect {
                                            x: 0,
                                            y: 0,
                                            width: geom.width,
                                            height: geom.height,
                                        },
                                        &mon,
                                    )
                                }))
                            } else {
                                None
                            };
                        let overlay_monitor = MonitorGeometry {
                            id: geom.id,
                            name: geom.title,
                            x: geom.x,
                            y: geom.y,
                            width: geom.width,
                            height: geom.height,
                            scale_factor: geom.scale_factor,
                            is_primary: false,
                        };
                        let overlay_physical_rect = PhysicalRect {
                            x: 0,
                            y: 0,
                            width: geom.width,
                            height: geom.height,
                        };
                        self.ensure_providers_initialized().await;
                        self.start_live_session_for_target(
                            LiveTarget::Window { window_id },
                            overlay_monitor,
                            overlay_physical_rect,
                            target_fullscreen_override,
                        );
                    }
                    Err(err) => {
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: format!(
                                "Không thể chụp cửa sổ đích cho Live Translate: {err}"
                            ),
                        });
                    }
                }
            }

            AppCommand::ToggleLivePause => {
                if let Some(session) = &mut self.live_session {
                    let next_paused = !session.handle.is_paused();
                    session.handle.set_paused(next_paused);
                    let _ = self.ui_tx.send(UiEvent::LivePauseChanged(next_paused));
                }
            }

            AppCommand::ToggleLiveVisibility => {
                if let Some(session) = &mut self.live_session {
                    session.overlay_visible = !session.overlay_visible;
                    let _ = self
                        .ui_tx
                        .send(UiEvent::LiveVisibilityChanged(session.overlay_visible));
                } else {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: "Chưa có phiên Live Translate đang hoạt động.".to_owned(),
                    });
                }
            }

            AppCommand::SwitchLiveDisplayMode => {
                if let Some(session) = &mut self.live_session {
                    let requested = match session.display_mode {
                        LiveDisplayMode::InPlace => LiveDisplayMode::FloatingPanel,
                        LiveDisplayMode::FloatingPanel => LiveDisplayMode::InPlace,
                    };
                    if requested == LiveDisplayMode::InPlace && !session.can_inplace {
                        let eff = resolve_effective_live_mode_for_target(
                            LiveDisplayMode::InPlace,
                            self.services.display_server,
                            session.target_is_fullscreen,
                        );
                        let _ = self.ui_tx.send(UiEvent::ShowToast {
                            message: eff.notice.unwrap_or_else(|| {
                                "Môi trường hiển thị hiện tại không hỗ trợ Phủ đè trực tiếp."
                                    .to_owned()
                            }),
                        });
                    } else {
                        session.display_mode = requested;
                        let _ = self.ui_tx.send(UiEvent::LiveDisplayModeChanged(requested));
                    }
                }
            }

            AppCommand::ReselectLiveRegion => {
                self.stop_live_session();
                self.begin_live_region_picker().await;
            }

            AppCommand::StopLiveTranslate => {
                if self.live_session.is_some() {
                    self.stop_live_session();
                } else if self.live_selecting_region {
                    self.live_selecting_region = false;
                    self.active_capture = None;
                    let _ = self.ui_tx.send(UiEvent::HideSnipOverlay);
                }
            }

            AppCommand::CopyLiveBlock(text) => match self.services.clipboard.set_text(&text) {
                Ok(()) => {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: "Đã sao chép câu thoại vào Clipboard.".to_owned(),
                    });
                }
                Err(err) => {
                    let _ = self.ui_tx.send(UiEvent::ShowToast {
                        message: format!("Không thể sao chép câu thoại: {err}"),
                    });
                }
            },

            AppCommand::Quit => {
                self.active_ask_ai = None;
                self.stop_live_session_silent();
                self.abort_in_flight_task().await;
                self.abort_test_task().await;
                let _ = self.ui_tx.send(UiEvent::Quit);
                return false;
            }
        }

        true
    }

    async fn ensure_providers_initialized(&mut self) {
        if self.providers_initialized {
            return;
        }
        let rebuild_translator = !is_free_web_provider(&self.config.translate.provider);
        let fallback_notice = self.reload_providers(rebuild_translator).await;
        self.providers_initialized = true;
        if let Some(message) = fallback_notice {
            let _ = self.ui_tx.send(UiEvent::ShowToast { message });
        }
    }

    async fn finalize_keyring_mutation(&mut self, status_text: String) {
        if !translate_config_needs_keyring(&self.config.translate)
            && let Ok(secrets) = load_provider_secrets(&self.services.secrets).await
        {
            self.secrets_snapshot = secrets;
            self.secrets_loaded = true;
        }
        let affects_translator = !is_free_web_provider(&self.config.translate.provider);
        if affects_translator {
            self.services.translation_cache.lock().await.clear();
        }
        let fallback_notice = self.reload_providers(affects_translator).await;
        let (has_gemini, has_openai, has_deepl) = secret_presence_flags(&self.secrets_snapshot);
        let _ = self.ui_tx.send(UiEvent::KeyringStatusUpdated {
            has_gemini,
            has_openai,
            has_deepl,
            ai_provider_configured: self.vision_ai.is_some(),
            status_text,
        });
        if let Some(message) = fallback_notice {
            let _ = self.ui_tx.send(UiEvent::ShowToast { message });
        }
    }

    async fn reload_providers(&mut self, rebuild_translator: bool) -> Option<String> {
        let mut warning_notice = None;
        if translate_config_needs_keyring(&self.config.translate) {
            match load_provider_secrets(&self.services.secrets).await {
                Ok(secrets) => {
                    self.secrets_snapshot = secrets;
                    self.secrets_loaded = true;
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to load secrets from OS keyring");
                    warning_notice = Some(format!(
                        "Không thể đọc API key từ OS Keyring ({err}); tạm thời dùng cấu hình hiện có."
                    ));
                }
            }
        }

        self.vision_ai = match self
            .services
            .provider_builder
            .build_vision_ai(&self.config.translate, &self.secrets_snapshot)
        {
            Ok(provider) => provider,
            Err(err) => {
                tracing::debug!(error = %err, "Ask AI provider not ready after reload");
                None
            }
        };

        if rebuild_translator {
            match self.services.provider_builder.build_translator(
                &self.config.translate,
                &self.secrets_snapshot,
                Arc::clone(&self.services.translation_cache),
            ) {
                Ok(translator) => {
                    self.shared_translator.replace(translator);
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        provider = %self.config.translate.provider,
                        "selected translator unavailable; falling back to Free Web"
                    );
                    let mut fallback_cfg = self.config.translate.clone();
                    fallback_cfg.provider = "free_web".to_owned();
                    let fallback = self
                        .services
                        .provider_builder
                        .build_translator(
                            &fallback_cfg,
                            &self.secrets_snapshot,
                            Arc::clone(&self.services.translation_cache),
                        )
                        .unwrap_or_else(|_| {
                            Arc::new(CachedFallbackTranslator::with_default_free_providers(
                                Arc::clone(&self.services.translation_cache),
                            ))
                        });
                    self.shared_translator.replace(fallback);
                    warning_notice = Some(format!(
                        "Nhà cung cấp dịch `{}` chưa sẵn sàng ({err}); tạm thời chuyển về Free Web.",
                        self.config.translate.provider
                    ));
                }
            }
        }

        self.providers_initialized = true;
        warning_notice
    }

    async fn begin_live_region_picker(&mut self) {
        let pack = self.config.ocr.live_pack;
        let tier = self.config.ocr.tier;
        if !self.services.model_store.is_installed(pack, tier) {
            self.pending_live_start = true;
            self.pending_action = None;
            let _ = self
                .ui_tx
                .send(UiEvent::ShowFirstRunOcrModal { pack, tier });
            return;
        }

        self.pending_live_start = false;
        self.active_ask_ai = None;
        self.abort_in_flight_task().await;
        self.last_selection = None;
        self.last_action_kind = None;

        match self.services.capturer.capture_active_monitor() {
            Ok((monitor, frame)) => {
                self.active_capture = Some(ActiveCapture {
                    monitor: monitor.clone(),
                    frame: frame.clone(),
                });
                self.live_selecting_region = true;
                let _ = self
                    .ui_tx
                    .send(UiEvent::ShowLiveRegionPicker { monitor, frame });
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to capture active monitor for Live Translate");
                let _ = self.ui_tx.send(UiEvent::ShowToast {
                    message: format!("Không thể chụp màn hình cho Live Translate: {err}"),
                });
            }
        }
    }

    fn start_live_session_for_target(
        &mut self,
        target: LiveTarget,
        overlay_monitor: MonitorGeometry,
        overlay_physical_rect: PhysicalRect,
        target_fullscreen_override: Option<bool>,
    ) {
        let target_is_fullscreen = target_fullscreen_override.unwrap_or_else(|| {
            is_rect_fullscreen_or_maximized(overlay_physical_rect, &overlay_monitor)
        });
        let effective = resolve_effective_live_mode_for_target(
            self.config.live.display_mode,
            self.services.display_server,
            target_is_fullscreen,
        );
        let can_inplace = resolve_effective_live_mode_for_target(
            LiveDisplayMode::InPlace,
            self.services.display_server,
            target_is_fullscreen,
        )
        .mode
            == LiveDisplayMode::InPlace;

        let loop_config = LiveLoopConfig::from_app_config(&self.config);
        let handle = spawn_live_session(
            target,
            loop_config,
            Arc::clone(&self.services.capturer),
            Arc::clone(&self.services.ocr),
            Arc::clone(&self.shared_translator) as Arc<dyn Translator>,
            self.ui_tx.clone(),
        );

        self.live_session = Some(ActiveLiveSession {
            handle,
            display_mode: effective.mode,
            can_inplace,
            target_is_fullscreen,
            overlay_visible: true,
        });

        let _ = self.ui_tx.send(UiEvent::LiveSessionStarted {
            display_mode: effective.mode,
            can_inplace,
            overlay_monitor,
            overlay_physical_rect,
            notice: effective.notice,
        });
    }

    fn stop_live_session(&mut self) {
        if let Some(mut session) = self.live_session.take() {
            session.handle.stop();
            let _ = self.ui_tx.send(UiEvent::LiveSessionStopped);
        }
    }

    fn stop_live_session_silent(&mut self) {
        if let Some(mut session) = self.live_session.take() {
            session.handle.stop();
        }
    }

    async fn abort_in_flight_task(&mut self) {
        let _ = self.cancel_tx.send(true);
        if let Some(handle) = self.in_flight_task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    async fn abort_test_task(&mut self) {
        if let Some(handle) = self.test_task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    async fn reset_cancellation(&mut self) -> watch::Receiver<bool> {
        self.abort_in_flight_task().await;
        let (tx, rx) = watch::channel(false);
        self.cancel_tx = tx;
        rx
    }

    fn crop_active_selection(&self, rect: LogicalRect) -> Result<CaptureFrame, String> {
        let Some(active) = &self.active_capture else {
            return Err("Chưa có khung hình chụp màn hình đang hoạt động.".to_owned());
        };
        let physical = logical_to_physical_crop(rect, &active.monitor);
        crop_frame(&active.frame, physical).map_err(|err| err.to_string())
    }

    async fn start_snip_ocr_action(
        &mut self,
        kind: SnipActionKind,
        rect: LogicalRect,
        pack: OcrLanguagePack,
    ) {
        self.active_ask_ai = None;
        self.abort_in_flight_task().await;
        self.last_selection = Some(rect);
        self.last_action_kind = Some(kind);
        self.active_snip_pack = pack;

        if self.active_capture.is_none() {
            let _ = self.ui_tx.send(UiEvent::ShowCardError {
                message: "Chưa có khung hình chụp màn hình đang hoạt động.".to_owned(),
            });
            return;
        }

        let tier = self.config.ocr.tier;
        if !self.services.model_store.is_installed(pack, tier) {
            self.pending_action = Some(PendingSnipAction { kind, rect, pack });
            self.pending_live_start = false;
            let _ = self
                .ui_tx
                .send(UiEvent::ShowFirstRunOcrModal { pack, tier });
            return;
        }

        self.pending_action = None;
        let cropped = match self.crop_active_selection(rect) {
            Ok(frame) => frame,
            Err(message) => {
                let _ = self.ui_tx.send(UiEvent::ShowCardError { message });
                return;
            }
        };

        let _ = self.ui_tx.send(UiEvent::ShowCardLoading);

        let mut cancel_rx = self.reset_cancellation().await;
        let ocr = Arc::clone(&self.services.ocr);
        let translator = self.shared_translator.current();
        let clipboard = Arc::clone(&self.services.clipboard);
        let source_lang = self.config.translate.source_lang.clone();
        let target_lang = self.config.translate.target_lang.clone();
        let ui_tx = self.ui_tx.clone();

        let handle = tokio::spawn(async move {
            let ocr_res = tokio::select! {
                biased;
                _ = cancel_rx.changed() => return,
                res = ocr.recognize(&cropped, pack, tier) => res,
            };
            if *cancel_rx.borrow() {
                return;
            }

            let lines = match ocr_res {
                Ok(lines) => lines,
                Err(err) => {
                    let _ = ui_tx.send(UiEvent::ShowCardError {
                        message: err.to_string(),
                    });
                    return;
                }
            };

            let ocr_text = join_ocr_lines(&lines);

            match kind {
                SnipActionKind::OcrCopy => {
                    if !ocr_text.is_empty()
                        && let Err(err) = clipboard.set_text(&ocr_text)
                    {
                        tracing::warn!(error = %err, "failed to copy OCR text to clipboard");
                    }
                    if *cancel_rx.borrow() {
                        return;
                    }
                    let status_text =
                        format!("PP-OCRv5 ({}) · Đã trích xuất chữ", pack_to_id(pack));
                    let _ = ui_tx.send(UiEvent::ShowInlineResult {
                        ocr_text,
                        translated_text: None,
                        pack,
                        status_text,
                    });
                }
                SnipActionKind::QuickTranslate => {
                    if ocr_text.is_empty() {
                        let _ = ui_tx.send(UiEvent::ShowInlineResult {
                            ocr_text: String::new(),
                            translated_text: Some(String::new()),
                            pack,
                            status_text: format!(
                                "PP-OCRv5 ({}) · Không phát hiện chữ",
                                pack_to_id(pack)
                            ),
                        });
                        return;
                    }

                    let req =
                        TranslateRequest::new(source_lang, target_lang, vec![ocr_text.clone()]);
                    let trans_res = tokio::select! {
                        biased;
                        _ = cancel_rx.changed() => return,
                        res = translator.translate(&req) => res,
                    };
                    if *cancel_rx.borrow() {
                        return;
                    }

                    match trans_res {
                        Ok(resp) => {
                            let translated =
                                resp.translations.into_iter().next().unwrap_or_default();
                            let status_text =
                                format!("{} · PP-OCRv5 ({})", resp.provider_name, pack_to_id(pack));
                            let _ = ui_tx.send(UiEvent::ShowInlineResult {
                                ocr_text,
                                translated_text: Some(translated),
                                pack,
                                status_text,
                            });
                        }
                        Err(err) => {
                            let _ = ui_tx.send(UiEvent::ShowCardError {
                                message: err.to_string(),
                            });
                        }
                    }
                }
            }
        });

        self.in_flight_task = Some(handle);
    }
}

fn join_ocr_lines(lines: &[OcrLine]) -> String {
    let mut out = String::new();
    for line in lines {
        let trimmed = line.text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(trimmed);
    }
    out
}

fn secret_presence_flags(secrets: &ProviderSecrets) -> (bool, bool, bool) {
    let has = |opt: &Option<String>| opt.as_deref().is_some_and(|s| !s.trim().is_empty());
    (
        has(&secrets.gemini_api_key),
        has(&secrets.openai_api_key),
        has(&secrets.deepl_api_key),
    )
}

fn is_free_web_provider(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "" | "free_web" | "free-web" | "google_web" | "google-web"
    )
}

fn provider_needs_keyring(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "gemini" | "openai" | "openai-compatible" | "openai_compatible" | "deepl"
    )
}

fn translate_config_needs_keyring(cfg: &TranslateConfig) -> bool {
    provider_needs_keyring(&cfg.provider)
        || cfg
            .ai_provider
            .as_deref()
            .is_some_and(provider_needs_keyring)
}

fn find_enclosing_monitor(
    monitors: &[MonitorGeometry],
    window: &WindowGeometry,
) -> Option<MonitorGeometry> {
    let center_x = i64::from(window.x) + i64::from(window.width / 2);
    let center_y = i64::from(window.y) + i64::from(window.height / 2);
    monitors
        .iter()
        .find(|mon| {
            let x0 = i64::from(mon.x);
            let y0 = i64::from(mon.y);
            let x1 = x0 + i64::from(mon.width);
            let y1 = y0 + i64::from(mon.height);
            center_x >= x0 && center_x < x1 && center_y >= y0 && center_y < y1
        })
        .cloned()
        .or_else(|| monitors.first().cloned())
}

#[cfg(test)]
mod tests {
    use sts_platform::secrets::MemorySecretStore;

    use super::*;

    #[tokio::test]
    async fn load_provider_secrets_reads_all_slots_via_spawn_blocking() -> Result<(), PlatformError>
    {
        let store = Arc::new(MemorySecretStore::new());
        store.set_secret(SecretSlot::GeminiApiKey, "gem-secret-1")?;
        store.set_secret(SecretSlot::OpenAiApiKey, "oa-secret-2")?;
        store.set_secret(SecretSlot::DeepLApiKey, "dl-secret-3")?;

        let dyn_store: Arc<dyn SecretStore> = store;
        let loaded = load_provider_secrets(&dyn_store).await?;
        assert_eq!(loaded.gemini_api_key.as_deref(), Some("gem-secret-1"));
        assert_eq!(loaded.openai_api_key.as_deref(), Some("oa-secret-2"));
        assert_eq!(loaded.deepl_api_key.as_deref(), Some("dl-secret-3"));
        Ok(())
    }

    #[test]
    fn encode_frame_as_png_produces_valid_png_and_rejects_invalid_buffers() {
        let frame = CaptureFrame {
            width: 4,
            height: 3,
            scale_factor: 1.0,
            rgba: vec![200; 4 * 3 * 4],
        };
        let encoded = encode_frame_as_png(&frame);
        assert!(encoded.is_ok(), "expected valid PNG encoding: {encoded:?}");
        let png = encoded.unwrap_or_default();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));

        let bad_frame = CaptureFrame {
            width: 4,
            height: 3,
            scale_factor: 1.0,
            rgba: vec![0; 4],
        };
        assert!(encode_frame_as_png(&bad_frame).is_err());

        let zero_frame = CaptureFrame {
            width: 0,
            height: 3,
            scale_factor: 1.0,
            rgba: Vec::new(),
        };
        assert!(encode_frame_as_png(&zero_frame).is_err());
    }

    #[test]
    fn app_command_debug_redacts_save_api_key_secret() {
        let cmd = AppCommand::SaveApiKey {
            slot: SecretSlot::OpenAiApiKey,
            value: "sk-super-secret-value".to_owned(),
        };
        let dbg = format!("{cmd:?}");
        assert!(!dbg.contains("sk-super-secret-value"));
        assert!(dbg.contains("[REDACTED]"));
    }
}
