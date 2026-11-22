//! End-to-end integration tests for [`AppController`] using deterministic in-memory fakes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sts_app::state::{
    AppCommand, AppController, AppServices, BoxFuture, DefaultProviderBuilder, PackDownloader,
    ProgressCallback, UiEvent, VisualSearcher, pack_from_id, pack_to_id, tier_from_id, tier_to_id,
};
use sts_app::ui_bridge::{
    ask_ai_provider_from_index, ask_ai_provider_to_index, build_model_pack_rows,
    capture_frame_to_slint_image, execution_device_from_index, execution_device_to_index,
    live_display_mode_from_index, live_display_mode_to_index, live_source_lang_from_index,
    live_source_lang_to_index, ocr_pack_from_index, ocr_pack_to_index, ocr_tier_from_index,
    ocr_tier_to_index, reading_direction_from_index, reading_direction_to_index,
    target_lang_from_index, target_lang_to_index, translate_provider_from_index,
    translate_provider_to_index, ui_language_from_index, ui_language_to_index,
    visual_search_engine_from_index, visual_search_engine_to_index,
};
use sts_core::cache::TranslationCache;
use sts_core::config::{AppConfig, ConfigStore, VisualSearchConfig};
use sts_core::geometry::{LogicalRect, MonitorGeometry, PhysicalRect};
use sts_core::types::{
    CaptureFrame, ExecutionDevice, LiveDisplayMode, OcrLanguagePack, OcrLine, OcrModelTier,
    ReadingDirection, UiLanguage, VisualSearchEngine,
};
use sts_ocr::OcrError;
use sts_ocr::downloader::DownloadProgress;
use sts_ocr::engine::OcrEngine;
use sts_ocr::manifest::{
    DET_MODEL_FILENAME, DICT_FILENAME, ModelManifest, ModelPackSpec, ModelStore, REC_MODEL_FILENAME,
};
use sts_platform::PlatformError;
use sts_platform::capture::ScreenCapturer;
use sts_platform::clipboard::ClipboardService;
use sts_platform::display_env::DisplayServer;
use sts_platform::secrets::{MemorySecretStore, SecretStore};
use sts_search::{BrowserOpener, SearchError, VisualSearchOutcome};
use sts_translate::{TranslateError, TranslateRequest, TranslateResponse, Translator};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex as AsyncMutex, Notify};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
struct FakeScreenCapturer {
    monitor: MonitorGeometry,
    frame: CaptureFrame,
}

impl FakeScreenCapturer {
    fn new(width: u32, height: u32, scale_factor: f32) -> Self {
        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[(x & 0xFF) as u8, (y & 0xFF) as u8, 128, 255]);
            }
        }
        Self {
            monitor: MonitorGeometry {
                id: 1,
                name: "PrimaryFake".to_owned(),
                x: 0,
                y: 0,
                width,
                height,
                scale_factor,
                is_primary: true,
            },
            frame: CaptureFrame {
                width,
                height,
                scale_factor,
                rgba,
            },
        }
    }
}

impl ScreenCapturer for FakeScreenCapturer {
    fn list_monitors(&self) -> Result<Vec<MonitorGeometry>, PlatformError> {
        Ok(vec![self.monitor.clone()])
    }

    fn capture_monitor(&self, monitor_id: u32) -> Result<CaptureFrame, PlatformError> {
        if monitor_id == self.monitor.id {
            Ok(self.frame.clone())
        } else {
            Err(PlatformError::MonitorNotFound(monitor_id))
        }
    }

    fn capture_active_monitor(&self) -> Result<(MonitorGeometry, CaptureFrame), PlatformError> {
        Ok((self.monitor.clone(), self.frame.clone()))
    }
}

#[derive(Debug, Default)]
struct FakeClipboard {
    texts: Mutex<Vec<String>>,
    images: Mutex<Vec<(usize, usize, Vec<u8>)>>,
}

impl FakeClipboard {
    fn text_writes(&self) -> Vec<String> {
        match self.texts.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl ClipboardService for FakeClipboard {
    fn set_text(&self, text: &str) -> Result<(), PlatformError> {
        match self.texts.lock() {
            Ok(mut guard) => guard.push(text.to_owned()),
            Err(poisoned) => poisoned.into_inner().push(text.to_owned()),
        }
        Ok(())
    }

    fn set_image_rgba(
        &self,
        width: usize,
        height: usize,
        rgba: &[u8],
    ) -> Result<(), PlatformError> {
        match self.images.lock() {
            Ok(mut guard) => guard.push((width, height, rgba.to_vec())),
            Err(poisoned) => poisoned.into_inner().push((width, height, rgba.to_vec())),
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeBrowserOpener {
    opened_urls: Mutex<Vec<String>>,
}

impl FakeBrowserOpener {
    fn urls(&self) -> Vec<String> {
        match self.opened_urls.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl BrowserOpener for FakeBrowserOpener {
    fn open_url(&self, url: &str) -> Result<(), SearchError> {
        match self.opened_urls.lock() {
            Ok(mut guard) => guard.push(url.to_owned()),
            Err(poisoned) => poisoned.into_inner().push(url.to_owned()),
        }
        Ok(())
    }
}

struct FakeVisualSearcher {
    browser: Arc<FakeBrowserOpener>,
    result_url: String,
    captured_dimensions: Mutex<Vec<(u32, u32)>>,
}

impl FakeVisualSearcher {
    fn new(browser: Arc<FakeBrowserOpener>, result_url: impl Into<String>) -> Self {
        Self {
            browser,
            result_url: result_url.into(),
            captured_dimensions: Mutex::new(Vec::new()),
        }
    }

    fn dimensions(&self) -> Vec<(u32, u32)> {
        match self.captured_dimensions.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

impl VisualSearcher for FakeVisualSearcher {
    fn search<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        config: &'a VisualSearchConfig,
    ) -> BoxFuture<'a, Result<VisualSearchOutcome, SearchError>> {
        Box::pin(async move {
            match self.captured_dimensions.lock() {
                Ok(mut guard) => guard.push((frame.width, frame.height)),
                Err(poisoned) => poisoned.into_inner().push((frame.width, frame.height)),
            }
            self.browser.open_url(&self.result_url)?;
            Ok(VisualSearchOutcome::OpenedResultUrl {
                engine: config.engine,
                url: self.result_url.clone(),
            })
        })
    }
}

struct FakeOcrEngine {
    lines_text: String,
    calls: AtomicUsize,
    gate: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl FakeOcrEngine {
    fn with_text(text: impl Into<String>) -> Self {
        Self {
            lines_text: text.into(),
            calls: AtomicUsize::new(0),
            gate: None,
        }
    }

    fn with_blocking_gate(
        text: impl Into<String>,
        started: Arc<Notify>,
        release: Arc<Notify>,
    ) -> Self {
        Self {
            lines_text: text.into(),
            calls: AtomicUsize::new(0),
            gate: Some((started, release)),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl OcrEngine for FakeOcrEngine {
    fn recognize<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        _pack: OcrLanguagePack,
        _tier: OcrModelTier,
    ) -> BoxFuture<'a, Result<Vec<OcrLine>, OcrError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some((started, release)) = &self.gate {
                started.notify_one();
                release.notified().await;
            }
            Ok(vec![OcrLine {
                bbox: PhysicalRect {
                    x: 0,
                    y: 0,
                    width: frame.width,
                    height: frame.height,
                },
                text: self.lines_text.clone(),
                confidence: 0.99,
                is_vertical: false,
            }])
        })
    }
}

struct FakeTranslator {
    dictionary: HashMap<String, String>,
    calls: AtomicUsize,
}

impl FakeTranslator {
    fn with_pairs(pairs: &[(&str, &str)]) -> Self {
        let mut dictionary = HashMap::new();
        for (src, dst) in pairs {
            dictionary.insert((*src).to_owned(), (*dst).to_owned());
        }
        Self {
            dictionary,
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Translator for FakeTranslator {
    fn provider_name(&self) -> &'static str {
        "fake-google-web"
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let translations = request
                .texts
                .iter()
                .map(|t| {
                    self.dictionary
                        .get(t)
                        .cloned()
                        .unwrap_or_else(|| format!("[vi] {t}"))
                })
                .collect();
            Ok(TranslateResponse {
                translations,
                provider_name: self.provider_name().to_owned(),
            })
        })
    }
}

#[derive(Default)]
struct FakePackDownloader {
    downloads: AtomicUsize,
}

impl FakePackDownloader {
    fn download_count(&self) -> usize {
        self.downloads.load(Ordering::SeqCst)
    }
}

impl PackDownloader for FakePackDownloader {
    fn download_pack<'a>(
        &'a self,
        store: &'a ModelStore,
        spec: &'a ModelPackSpec,
        on_progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<PathBuf, OcrError>> {
        Box::pin(async move {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let pack_dir = store.pack_dir(spec.pack, spec.tier);
            std::fs::create_dir_all(&pack_dir).map_err(|source| OcrError::Io {
                path: pack_dir.clone(),
                source,
            })?;

            let total = spec.total_size_bytes().max(100);
            on_progress(DownloadProgress {
                artifact: DET_MODEL_FILENAME.to_owned(),
                artifact_index: 0,
                artifact_count: 3,
                bytes_downloaded: total / 2,
                total_bytes: total,
            });

            let paths = store.pack_paths(spec.pack, spec.tier);
            for (path, bytes) in [
                (&paths.det, &b"fake-det-onnx"[..]),
                (&paths.rec, &b"fake-rec-onnx"[..]),
                (&paths.dict, &b"a\nb\nc\n"[..]),
            ] {
                std::fs::write(path, bytes).map_err(|source| OcrError::Io {
                    path: path.clone(),
                    source,
                })?;
            }

            on_progress(DownloadProgress {
                artifact: DICT_FILENAME.to_owned(),
                artifact_index: 2,
                artifact_count: 3,
                bytes_downloaded: total,
                total_bytes: total,
            });

            Ok(pack_dir)
        })
    }
}

struct TestHarness {
    _temp_dir: TempDir,
    config_store: ConfigStore,
    model_store: ModelStore,
    clipboard: Arc<FakeClipboard>,
    browser: Arc<FakeBrowserOpener>,
    ocr: Arc<FakeOcrEngine>,
    translator: Arc<FakeTranslator>,
    visual_search: Arc<FakeVisualSearcher>,
    downloader: Arc<FakePackDownloader>,
}

impl TestHarness {
    fn new(
        ocr: Arc<FakeOcrEngine>,
        translator: Arc<FakeTranslator>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let config_store = ConfigStore::new(temp_dir.path().join("config.toml"));
        let model_store = ModelStore::new(temp_dir.path().join("models"));
        let clipboard = Arc::new(FakeClipboard::default());
        let browser = Arc::new(FakeBrowserOpener::default());
        let visual_search = Arc::new(FakeVisualSearcher::new(
            Arc::clone(&browser),
            "https://lens.google.com/search?ep=ccm&p=fake123",
        ));
        let downloader = Arc::new(FakePackDownloader::default());

        Ok(Self {
            _temp_dir: temp_dir,
            config_store,
            model_store,
            clipboard,
            browser,
            ocr,
            translator,
            visual_search,
            downloader,
        })
    }

    fn install_pack(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> TestResult {
        let dir = self.model_store.pack_dir(pack, tier);
        std::fs::create_dir_all(&dir)?;
        let paths = self.model_store.pack_paths(pack, tier);
        std::fs::write(&paths.det, b"fake-det")?;
        std::fs::write(&paths.rec, b"fake-rec")?;
        std::fs::write(&paths.dict, b"a\nb\nc\n")?;
        Ok(())
    }

    fn services(&self) -> AppServices {
        AppServices {
            config_store: self.config_store.clone(),
            model_store: self.model_store.clone(),
            model_manifest: ModelManifest::builtin(),
            downloader: Arc::clone(&self.downloader) as Arc<dyn PackDownloader>,
            capturer: Arc::new(FakeScreenCapturer::new(200, 120, 1.0)),
            clipboard: Arc::clone(&self.clipboard) as Arc<dyn ClipboardService>,
            ocr: Arc::clone(&self.ocr) as Arc<dyn OcrEngine>,
            translator: Arc::clone(&self.translator) as Arc<dyn Translator>,
            visual_search: Arc::clone(&self.visual_search) as Arc<dyn VisualSearcher>,
            browser: Arc::clone(&self.browser) as Arc<dyn BrowserOpener>,
            display_server: DisplayServer::Windows,
            secrets: Arc::new(MemorySecretStore::new()) as Arc<dyn SecretStore>,
            translation_cache: Arc::new(AsyncMutex::new(TranslationCache::new(64))),
            provider_builder: Arc::new(DefaultProviderBuilder),
        }
    }
}

async fn recv_event(
    rx: &mut UnboundedReceiver<UiEvent>,
) -> Result<UiEvent, Box<dyn std::error::Error>> {
    match tokio::time::timeout(RECV_TIMEOUT, rx.recv()).await {
        Ok(Some(ev)) => Ok(ev),
        Ok(None) => Err("UI event channel closed unexpectedly".into()),
        Err(_) => Err("timed out waiting for UiEvent".into()),
    }
}

#[tokio::test]
async fn snip_visual_search_succeeds_even_when_no_ocr_model_installed() -> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("unused")),
        Arc::new(FakeTranslator::with_pairs(&[])),
    )?;
    assert!(
        !harness
            .model_store
            .is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
    );

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let first = recv_event(&mut ui_rx).await?;
    assert!(
        matches!(first, UiEvent::ShowSnipOverlay { ref frame, .. } if frame.width == 200 && frame.height == 120),
        "expected ShowSnipOverlay, got {first:?}"
    );

    let rect = LogicalRect {
        x: 20.0,
        y: 10.0,
        width: 80.0,
        height: 50.0,
    };
    cmd_tx.send(AppCommand::RunVisualSearch(rect))?;

    let second = recv_event(&mut ui_rx).await?;
    assert_eq!(
        second,
        UiEvent::VisualSearchDone(VisualSearchOutcome::OpenedResultUrl {
            engine: VisualSearchEngine::GoogleLens,
            url: "https://lens.google.com/search?ep=ccm&p=fake123".to_owned(),
        })
    );

    let third = recv_event(&mut ui_rx).await?;
    assert_eq!(third, UiEvent::HideSnipOverlay);
    assert_eq!(harness.visual_search.dimensions(), vec![(80, 50)]);
    assert_eq!(
        harness.browser.urls(),
        vec!["https://lens.google.com/search?ep=ccm&p=fake123".to_owned()]
    );
    assert_eq!(harness.ocr.call_count(), 0);

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn snip_copy_ocr_prompts_first_run_download_when_model_missing() -> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("Hello Rust")),
        Arc::new(FakeTranslator::with_pairs(&[])),
    )?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 60.0,
        height: 30.0,
    };
    cmd_tx.send(AppCommand::RunOcrCopy(rect))?;

    let modal_event = recv_event(&mut ui_rx).await?;
    assert_eq!(
        modal_event,
        UiEvent::ShowFirstRunOcrModal {
            pack: OcrLanguagePack::CjkEn,
            tier: OcrModelTier::Mobile,
        }
    );
    assert_eq!(harness.ocr.call_count(), 0);

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn snip_copy_ocr_extracts_text_and_shows_inline_card_when_model_present() -> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("Hello Rust")),
        Arc::new(FakeTranslator::with_pairs(&[])),
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 15.0,
        y: 15.0,
        width: 100.0,
        height: 40.0,
    };
    cmd_tx.send(AppCommand::RunOcrCopy(rect))?;

    let loading = recv_event(&mut ui_rx).await?;
    assert_eq!(loading, UiEvent::ShowCardLoading);

    let result = recv_event(&mut ui_rx).await?;
    match result {
        UiEvent::ShowInlineResult {
            ocr_text,
            translated_text,
            pack,
            ..
        } => {
            assert_eq!(ocr_text, "Hello Rust");
            assert_eq!(translated_text, None);
            assert_eq!(pack, OcrLanguagePack::CjkEn);
        }
        other => return Err(format!("expected ShowInlineResult, got {other:?}").into()),
    }
    assert_eq!(
        harness.clipboard.text_writes(),
        vec!["Hello Rust".to_owned()]
    );

    // Verify SearchTextGoogle encodes query and opens browser.
    cmd_tx.send(AppCommand::SearchTextGoogle("Hello Rust".to_owned()))?;
    let toast = recv_event(&mut ui_rx).await?;
    assert!(matches!(toast, UiEvent::ShowToast { .. }));
    assert_eq!(
        harness.browser.urls(),
        vec!["https://www.google.com/search?q=Hello+Rust".to_owned()]
    );

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn snip_quick_translate_runs_ocr_then_translator_and_shows_vietnamese() -> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("Hello")),
        Arc::new(FakeTranslator::with_pairs(&[("Hello", "Xin chào")])),
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 5.0,
        y: 5.0,
        width: 90.0,
        height: 45.0,
    };
    cmd_tx.send(AppCommand::RunQuickTranslate(rect))?;

    let loading = recv_event(&mut ui_rx).await?;
    assert_eq!(loading, UiEvent::ShowCardLoading);

    let result = recv_event(&mut ui_rx).await?;
    match result {
        UiEvent::ShowInlineResult {
            ocr_text,
            translated_text,
            pack,
            ..
        } => {
            assert_eq!(ocr_text, "Hello");
            assert_eq!(translated_text, Some("Xin chào".to_owned()));
            assert_eq!(pack, OcrLanguagePack::CjkEn);
        }
        other => return Err(format!("expected ShowInlineResult, got {other:?}").into()),
    }
    assert_eq!(harness.ocr.call_count(), 1);
    assert_eq!(harness.translator.call_count(), 1);

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn snip_ask_ai_without_configured_provider_offers_settings_shortcut_and_save_persists_config()
-> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("Hello")),
        Arc::new(FakeTranslator::with_pairs(&[])),
    )?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 50.0,
        height: 50.0,
    };
    cmd_tx.send(AppCommand::RunAskAi(rect))?;
    let ask_event = recv_event(&mut ui_rx).await?;
    assert_eq!(ask_event, UiEvent::ShowAskAiUnconfigured);

    cmd_tx.send(AppCommand::OpenSettingsTab(3))?;
    let show_settings = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        show_settings,
        UiEvent::ShowSettings { tab: 3, .. }
    ));

    let mut updated_cfg = AppConfig {
        ui_language: UiLanguage::English,
        ..AppConfig::default()
    };
    updated_cfg.ocr.device = ExecutionDevice::Cpu;
    updated_cfg.ocr.snip_pack = OcrLanguagePack::VietnameseLatin;
    updated_cfg.search.engine = VisualSearchEngine::Yandex;
    updated_cfg.translate.ai_provider = Some("ollama".to_owned());

    cmd_tx.send(AppCommand::SaveSettings(updated_cfg.clone()))?;
    let saved_event = recv_event(&mut ui_rx).await?;
    match saved_event {
        UiEvent::SettingsSaved { config, .. } => {
            assert_eq!(*config, updated_cfg);
        }
        other => return Err(format!("expected SettingsSaved, got {other:?}").into()),
    }
    let packs_updated = recv_event(&mut ui_rx).await?;
    assert!(matches!(packs_updated, UiEvent::ModelPacksUpdated(_)));

    let reloaded = harness.config_store.load_or_default()?.into_config();
    assert_eq!(reloaded, updated_cfg);

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn start_starter_download_installs_pack_and_resumes_pending_snip_action() -> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("Hello")),
        Arc::new(FakeTranslator::with_pairs(&[("Hello", "Xin chào")])),
    )?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 12.0,
        y: 12.0,
        width: 80.0,
        height: 40.0,
    };
    cmd_tx.send(AppCommand::RunQuickTranslate(rect))?;
    let modal = recv_event(&mut ui_rx).await?;
    assert_eq!(
        modal,
        UiEvent::ShowFirstRunOcrModal {
            pack: OcrLanguagePack::CjkEn,
            tier: OcrModelTier::Mobile,
        }
    );

    cmd_tx.send(AppCommand::StartStarterDownload)?;

    let mut saw_progress = false;
    let mut saw_packs_updated = false;
    let mut saw_hide_modal = false;
    let mut saw_loading = false;
    let mut final_result = None;

    for _ in 0..10 {
        let ev = recv_event(&mut ui_rx).await?;
        match ev {
            UiEvent::StarterDownloadProgress { progress, .. } => {
                assert!((0.0..=1.0).contains(&progress));
                saw_progress = true;
            }
            UiEvent::ModelPacksUpdated(statuses) => {
                assert!(statuses.iter().any(|s| {
                    s.pack == OcrLanguagePack::CjkEn
                        && s.tier == OcrModelTier::Mobile
                        && s.installed
                }));
                saw_packs_updated = true;
            }
            UiEvent::HideFirstRunOcrModal => {
                saw_hide_modal = true;
            }
            UiEvent::ShowCardLoading => {
                saw_loading = true;
            }
            UiEvent::ShowInlineResult {
                ocr_text,
                translated_text,
                pack,
                ..
            } => {
                final_result = Some((ocr_text, translated_text, pack));
                break;
            }
            other => {
                return Err(format!("unexpected event during starter download: {other:?}").into());
            }
        }
    }

    assert!(saw_progress, "expected StarterDownloadProgress events");
    assert!(saw_packs_updated, "expected ModelPacksUpdated event");
    assert!(saw_hide_modal, "expected HideFirstRunOcrModal event");
    assert!(
        saw_loading,
        "expected ShowCardLoading event on resumed action"
    );
    assert_eq!(
        final_result,
        Some((
            "Hello".to_owned(),
            Some("Xin chào".to_owned()),
            OcrLanguagePack::CjkEn,
        ))
    );
    assert_eq!(harness.downloader.download_count(), 1);
    assert!(
        harness
            .model_store
            .is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
    );

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn cancel_overlay_aborts_in_flight_ocr_task_and_clears_session_state() -> TestResult {
    let ocr_started = Arc::new(Notify::new());
    let ocr_release = Arc::new(Notify::new());
    let ocr = Arc::new(FakeOcrEngine::with_blocking_gate(
        "ShouldNeverArrive",
        Arc::clone(&ocr_started),
        Arc::clone(&ocr_release),
    ));
    let harness = TestHarness::new(ocr, Arc::new(FakeTranslator::with_pairs(&[])))?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 50.0,
        height: 30.0,
    };
    cmd_tx.send(AppCommand::RunOcrCopy(rect))?;

    let loading = recv_event(&mut ui_rx).await?;
    assert_eq!(loading, UiEvent::ShowCardLoading);

    tokio::time::timeout(RECV_TIMEOUT, ocr_started.notified()).await?;

    // Cancel overlay while OCR is still blocked mid-inference.
    cmd_tx.send(AppCommand::CancelOverlay)?;
    let hide_ev = recv_event(&mut ui_rx).await?;
    assert_eq!(hide_ev, UiEvent::HideSnipOverlay);

    // Release the gate just in case and verify no stale ShowInlineResult or clipboard write occurs.
    ocr_release.notify_waiters();
    tokio::task::yield_now().await;
    assert!(harness.clipboard.text_writes().is_empty());

    // Confirm active capture was cleared: running OCR without a new TriggerSquareToSearch reports error.
    cmd_tx.send(AppCommand::RunOcrCopy(rect))?;
    let err_ev = recv_event(&mut ui_rx).await?;
    assert!(matches!(err_ev, UiEvent::ShowCardError { .. }));

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn switch_snip_ocr_pack_reruns_active_action_and_model_pack_commands_update_store()
-> TestResult {
    let harness = TestHarness::new(
        Arc::new(FakeOcrEngine::with_text("Xin chào")),
        Arc::new(FakeTranslator::with_pairs(&[])),
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;
    harness.install_pack(OcrLanguagePack::VietnameseLatin, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_event(&mut ui_rx).await?;

    let rect = LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 70.0,
        height: 30.0,
    };
    cmd_tx.send(AppCommand::RunOcrCopy(rect))?;
    let _ = recv_event(&mut ui_rx).await?; // ShowCardLoading
    let first_res = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        first_res,
        UiEvent::ShowInlineResult {
            pack: OcrLanguagePack::CjkEn,
            ..
        }
    ));

    // Switch quick OCR pack chip to VietnameseLatin -> automatically re-runs OCR with VietnameseLatin.
    cmd_tx.send(AppCommand::SwitchSnipOcrPack(
        OcrLanguagePack::VietnameseLatin,
    ))?;
    let loading = recv_event(&mut ui_rx).await?;
    assert_eq!(loading, UiEvent::ShowCardLoading);
    let second_res = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        second_res,
        UiEvent::ShowInlineResult {
            pack: OcrLanguagePack::VietnameseLatin,
            ..
        }
    ));

    // Delete Korean pack, download it via Settings command, and import an offline pack.
    cmd_tx.send(AppCommand::DownloadModelPack {
        pack: OcrLanguagePack::Korean,
        tier: OcrModelTier::Mobile,
    })?;
    let mut korean_installed = false;
    for _ in 0..5 {
        let ev = recv_event(&mut ui_rx).await?;
        if let UiEvent::ModelPacksUpdated(statuses) = ev {
            korean_installed = statuses.iter().any(|s| {
                s.pack == OcrLanguagePack::Korean && s.tier == OcrModelTier::Mobile && s.installed
            });
            break;
        }
    }
    assert!(korean_installed);

    cmd_tx.send(AppCommand::DeleteModelPack {
        pack: OcrLanguagePack::Korean,
        tier: OcrModelTier::Mobile,
    })?;
    let del_ev = recv_event(&mut ui_rx).await?;
    if let UiEvent::ModelPacksUpdated(statuses) = del_ev {
        assert!(!statuses.iter().any(|s| {
            s.pack == OcrLanguagePack::Korean && s.tier == OcrModelTier::Mobile && s.installed
        }));
    } else {
        return Err(format!("expected ModelPacksUpdated after delete, got {del_ev:?}").into());
    }

    let offline_src = TempDir::new()?;
    std::fs::write(offline_src.path().join(DET_MODEL_FILENAME), b"det")?;
    std::fs::write(offline_src.path().join(REC_MODEL_FILENAME), b"rec")?;
    std::fs::write(offline_src.path().join(DICT_FILENAME), b"dict")?;

    cmd_tx.send(AppCommand::ImportOfflineModel {
        pack: OcrLanguagePack::Korean,
        tier: OcrModelTier::Server,
        dir: offline_src.path().to_path_buf(),
    })?;
    let import_ev = recv_event(&mut ui_rx).await?;
    if let UiEvent::ModelPacksUpdated(statuses) = import_ev {
        assert!(statuses.iter().any(|s| {
            s.pack == OcrLanguagePack::Korean && s.tier == OcrModelTier::Server && s.installed
        }));
    } else {
        return Err(format!("expected ModelPacksUpdated after import, got {import_ev:?}").into());
    }

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_event(&mut ui_rx).await?;
    handle.await?;
    Ok(())
}

#[test]
fn ui_bridge_mappings_roundtrip_all_settings_and_pack_rows_cleanly() -> TestResult {
    for pack in [
        OcrLanguagePack::CjkEn,
        OcrLanguagePack::VietnameseLatin,
        OcrLanguagePack::Korean,
    ] {
        assert_eq!(pack_from_id(pack_to_id(pack)), Some(pack));
        assert_eq!(ocr_pack_from_index(ocr_pack_to_index(pack)), pack);
    }

    for tier in [OcrModelTier::Mobile, OcrModelTier::Server] {
        assert_eq!(tier_from_id(tier_to_id(tier)), Some(tier));
        assert_eq!(ocr_tier_from_index(ocr_tier_to_index(tier)), tier);
    }

    for lang in [UiLanguage::Vietnamese, UiLanguage::English] {
        assert_eq!(ui_language_from_index(ui_language_to_index(lang)), lang);
    }

    for mode in [LiveDisplayMode::InPlace, LiveDisplayMode::FloatingPanel] {
        assert_eq!(
            live_display_mode_from_index(live_display_mode_to_index(mode)),
            mode
        );
    }

    for src in ["auto", "ja", "ko", "zh-CN", "en"] {
        assert_eq!(
            live_source_lang_from_index(live_source_lang_to_index(src)),
            src
        );
    }

    for dir in [
        ReadingDirection::Auto,
        ReadingDirection::RightToLeft,
        ReadingDirection::LeftToRight,
    ] {
        assert_eq!(
            reading_direction_from_index(reading_direction_to_index(dir)),
            dir
        );
    }

    for tgt in ["vi", "en"] {
        assert_eq!(target_lang_from_index(target_lang_to_index(tgt)), tgt);
    }

    for dev in [
        ExecutionDevice::Auto,
        ExecutionDevice::Cpu,
        ExecutionDevice::DirectMl,
    ] {
        assert_eq!(
            execution_device_from_index(execution_device_to_index(dev)),
            dev
        );
    }

    for prov in ["free_web", "gemini", "openai", "deepl", "ollama"] {
        assert_eq!(
            translate_provider_from_index(translate_provider_to_index(prov)),
            prov
        );
    }
    assert_eq!(translate_provider_to_index("openai-compatible"), 2);
    assert_eq!(translate_provider_to_index("openai_compatible"), 2);

    for ai in [None, Some("gemini"), Some("openai"), Some("ollama")] {
        assert_eq!(
            ask_ai_provider_from_index(ask_ai_provider_to_index(ai)).as_deref(),
            ai
        );
    }
    assert_eq!(ask_ai_provider_to_index(Some("openai-compatible")), 2);

    for engine in [
        VisualSearchEngine::GoogleLens,
        VisualSearchEngine::Yandex,
        VisualSearchEngine::Bing,
    ] {
        assert_eq!(
            visual_search_engine_from_index(visual_search_engine_to_index(engine)),
            engine
        );
    }

    let tmp = TempDir::new()?;
    let store = ModelStore::new(tmp.path().to_path_buf());
    let manifest = ModelManifest::builtin();
    let statuses = store.list_statuses(&manifest);
    let rows = build_model_pack_rows(&statuses, OcrModelTier::Mobile);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].pack_id.as_str(), "cjk-en");
    assert_eq!(rows[1].pack_id.as_str(), "vi-latin");
    assert_eq!(rows[2].pack_id.as_str(), "korean");

    let valid_frame = CaptureFrame {
        width: 4,
        height: 2,
        scale_factor: 1.0,
        rgba: vec![255; 4 * 2 * 4],
    };
    let img = capture_frame_to_slint_image(&valid_frame);
    assert!(img.is_some());

    let invalid_frame = CaptureFrame {
        width: 4,
        height: 2,
        scale_factor: 1.0,
        rgba: vec![255; 4],
    };
    assert!(capture_frame_to_slint_image(&invalid_frame).is_none());

    Ok(())
}
