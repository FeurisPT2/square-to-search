//! End-to-end integration tests for Continuous Live Translate using deterministic in-memory fakes
//! and Tokio virtual time (`tokio::time::pause()`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sts_app::live::LiveRenderedBlock;
use sts_app::state::{
    AppCommand, AppController, AppServices, BoxFuture, DefaultProviderBuilder, PackDownloader,
    ProgressCallback, UiEvent, VisualSearcher,
};
use sts_app::ui_bridge::{live_blocks_to_rows, live_display_mode_to_id};
use sts_core::cache::TranslationCache;
use sts_core::config::{AppConfig, ConfigStore, VisualSearchConfig};
use sts_core::geometry::{LogicalRect, MonitorGeometry, PhysicalRect};
use sts_core::types::{
    CaptureFrame, LiveDisplayMode, OcrLanguagePack, OcrLine, OcrModelTier, ReadingDirection,
};
use sts_ocr::OcrError;
use sts_ocr::downloader::DownloadProgress;
use sts_ocr::engine::OcrEngine;
use sts_ocr::manifest::{
    DET_MODEL_FILENAME, DICT_FILENAME, ModelManifest, ModelPackSpec, ModelStore,
};
use sts_platform::PlatformError;
use sts_platform::capture::{ScreenCapturer, WindowGeometry};
use sts_platform::clipboard::ClipboardService;
use sts_platform::display_env::{DisplayServer, WaylandCompositor};
use sts_platform::secrets::{MemorySecretStore, SecretStore};
use sts_search::{BrowserOpener, SearchError, VisualSearchOutcome};
use sts_translate::{TranslateError, TranslateRequest, TranslateResponse, Translator};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex as AsyncMutex, Notify};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn solid_frame(width: u32, height: u32, scale_factor: f32, rgba: [u8; 4]) -> CaptureFrame {
    let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for _ in 0..(width as usize) * (height as usize) {
        buf.extend_from_slice(&rgba);
    }
    CaptureFrame {
        width,
        height,
        scale_factor,
        rgba: buf,
    }
}

fn striped_frame(width: u32, height: u32, scale_factor: f32, phase: u32) -> CaptureFrame {
    let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
    for y in 0..height {
        let luma = if ((y + phase) / 8).is_multiple_of(2) {
            245u8
        } else {
            25u8
        };
        for _ in 0..width {
            buf.extend_from_slice(&[luma, luma, luma, 255]);
        }
    }
    CaptureFrame {
        width,
        height,
        scale_factor,
        rgba: buf,
    }
}

/// Capturer whose active monitor/window frame can be mutated deterministically between virtual-time ticks.
struct FakeLiveCapturer {
    monitor: MonitorGeometry,
    current_frame: Mutex<CaptureFrame>,
    window: Mutex<Option<(WindowGeometry, CaptureFrame)>>,
    monitor_captures: AtomicUsize,
    window_captures: AtomicUsize,
}

impl FakeLiveCapturer {
    fn new(width: u32, height: u32, scale_factor: f32, initial_frame: CaptureFrame) -> Self {
        Self {
            monitor: MonitorGeometry {
                id: 1,
                name: "PrimaryLiveFake".to_owned(),
                x: 0,
                y: 0,
                width,
                height,
                scale_factor,
                is_primary: true,
            },
            current_frame: Mutex::new(initial_frame),
            window: Mutex::new(None),
            monitor_captures: AtomicUsize::new(0),
            window_captures: AtomicUsize::new(0),
        }
    }

    fn set_frame(&self, frame: CaptureFrame) {
        match self.current_frame.lock() {
            Ok(mut guard) => *guard = frame,
            Err(poisoned) => *poisoned.into_inner() = frame,
        }
    }

    fn set_window(&self, geometry: WindowGeometry, frame: CaptureFrame) {
        match self.window.lock() {
            Ok(mut guard) => *guard = Some((geometry, frame)),
            Err(poisoned) => *poisoned.into_inner() = Some((geometry, frame)),
        }
    }

    fn monitor_capture_count(&self) -> usize {
        self.monitor_captures.load(Ordering::SeqCst)
    }

    fn window_capture_count(&self) -> usize {
        self.window_captures.load(Ordering::SeqCst)
    }
}

impl ScreenCapturer for FakeLiveCapturer {
    fn list_monitors(&self) -> Result<Vec<MonitorGeometry>, PlatformError> {
        Ok(vec![self.monitor.clone()])
    }

    fn capture_monitor(&self, monitor_id: u32) -> Result<CaptureFrame, PlatformError> {
        if monitor_id != self.monitor.id {
            return Err(PlatformError::MonitorNotFound(monitor_id));
        }
        self.monitor_captures.fetch_add(1, Ordering::SeqCst);
        let frame = match self.current_frame.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        Ok(frame)
    }

    fn capture_active_monitor(&self) -> Result<(MonitorGeometry, CaptureFrame), PlatformError> {
        let frame = self.capture_monitor(self.monitor.id)?;
        Ok((self.monitor.clone(), frame))
    }

    fn list_windows(&self) -> Result<Vec<WindowGeometry>, PlatformError> {
        let guard = match self.window.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        Ok(guard
            .as_ref()
            .map(|(g, _)| vec![g.clone()])
            .unwrap_or_default())
    }

    fn capture_window(
        &self,
        window_id: u32,
    ) -> Result<(WindowGeometry, CaptureFrame), PlatformError> {
        self.window_captures.fetch_add(1, Ordering::SeqCst);
        let guard = match self.window.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_ref() {
            Some((geom, frame)) if geom.id == window_id => Ok((geom.clone(), frame.clone())),
            _ => Err(PlatformError::WindowNotFound(window_id)),
        }
    }
}

#[derive(Debug, Default)]
struct FakeClipboard {
    texts: Mutex<Vec<String>>,
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

    fn set_image_rgba(&self, _w: usize, _h: usize, _rgba: &[u8]) -> Result<(), PlatformError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeBrowserOpener;

impl BrowserOpener for FakeBrowserOpener {
    fn open_url(&self, _url: &str) -> Result<(), SearchError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct FakeVisualSearcher;

impl VisualSearcher for FakeVisualSearcher {
    fn search<'a>(
        &'a self,
        _frame: &'a CaptureFrame,
        config: &'a VisualSearchConfig,
    ) -> BoxFuture<'a, Result<VisualSearchOutcome, SearchError>> {
        Box::pin(async move {
            Ok(VisualSearchOutcome::OpenedResultUrl {
                engine: config.engine,
                url: "https://lens.google.com/search?p=unused".to_owned(),
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
                (&paths.det, &b"fake-det"[..]),
                (&paths.rec, &b"fake-rec"[..]),
                (&paths.dict, &b"a\nb\n"[..]),
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

type OcrResponder = Box<dyn Fn(&CaptureFrame) -> Vec<OcrLine> + Send + Sync>;

struct FakeLiveOcr {
    responder: OcrResponder,
    calls: AtomicUsize,
}

impl FakeLiveOcr {
    fn new(responder: impl Fn(&CaptureFrame) -> Vec<OcrLine> + Send + Sync + 'static) -> Self {
        Self {
            responder: Box::new(responder),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl OcrEngine for FakeLiveOcr {
    fn recognize<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        _pack: OcrLanguagePack,
        _tier: OcrModelTier,
    ) -> BoxFuture<'a, Result<Vec<OcrLine>, OcrError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok((self.responder)(frame))
        })
    }
}

enum TranslateBehavior {
    Dictionary(HashMap<String, String>),
    GateOnMarker {
        marker: String,
        started: Arc<Notify>,
        release: Arc<Notify>,
        dictionary: HashMap<String, String>,
    },
    RateLimitOnMarker {
        marker: String,
        dictionary: HashMap<String, String>,
    },
}

struct FakeLiveTranslator {
    behavior: TranslateBehavior,
    calls: AtomicUsize,
    recorded_batches: Mutex<Vec<Vec<String>>>,
}

impl FakeLiveTranslator {
    fn with_pairs(pairs: &[(&str, &str)]) -> Self {
        let dictionary = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Self {
            behavior: TranslateBehavior::Dictionary(dictionary),
            calls: AtomicUsize::new(0),
            recorded_batches: Mutex::new(Vec::new()),
        }
    }

    fn blocking_on_marker(
        marker: impl Into<String>,
        started: Arc<Notify>,
        release: Arc<Notify>,
        pairs: &[(&str, &str)],
    ) -> Self {
        let dictionary = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Self {
            behavior: TranslateBehavior::GateOnMarker {
                marker: marker.into(),
                started,
                release,
                dictionary,
            },
            calls: AtomicUsize::new(0),
            recorded_batches: Mutex::new(Vec::new()),
        }
    }

    fn rate_limit_on_marker(marker: impl Into<String>, pairs: &[(&str, &str)]) -> Self {
        let dictionary = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Self {
            behavior: TranslateBehavior::RateLimitOnMarker {
                marker: marker.into(),
                dictionary,
            },
            calls: AtomicUsize::new(0),
            recorded_batches: Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn batches(&self) -> Vec<Vec<String>> {
        match self.recorded_batches.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

fn map_translations(dict: &HashMap<String, String>, texts: &[String]) -> Vec<String> {
    texts
        .iter()
        .map(|text| {
            dict.get(text)
                .cloned()
                .unwrap_or_else(|| format!("[vi] {text}"))
        })
        .collect()
}

impl Translator for FakeLiveTranslator {
    fn provider_name(&self) -> &'static str {
        "fake-live-translator"
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.recorded_batches.lock() {
                Ok(mut guard) => guard.push(request.texts.clone()),
                Err(poisoned) => poisoned.into_inner().push(request.texts.clone()),
            }

            match &self.behavior {
                TranslateBehavior::Dictionary(dict) => Ok(TranslateResponse {
                    translations: map_translations(dict, &request.texts),
                    provider_name: self.provider_name().to_owned(),
                }),
                TranslateBehavior::GateOnMarker {
                    marker,
                    started,
                    release,
                    dictionary,
                } => {
                    if request.texts.iter().any(|t| t.contains(marker)) {
                        started.notify_one();
                        release.notified().await;
                    }
                    Ok(TranslateResponse {
                        translations: map_translations(dictionary, &request.texts),
                        provider_name: self.provider_name().to_owned(),
                    })
                }
                TranslateBehavior::RateLimitOnMarker { marker, dictionary } => {
                    if request.texts.iter().any(|t| t.contains(marker)) {
                        return Err(TranslateError::RateLimited(
                            "HTTP 429 Too Many Requests".to_owned(),
                        ));
                    }
                    Ok(TranslateResponse {
                        translations: map_translations(dictionary, &request.texts),
                        provider_name: self.provider_name().to_owned(),
                    })
                }
            }
        })
    }
}

struct LiveHarness {
    _temp_dir: TempDir,
    config_store: ConfigStore,
    model_store: ModelStore,
    capturer: Arc<FakeLiveCapturer>,
    clipboard: Arc<FakeClipboard>,
    ocr: Arc<FakeLiveOcr>,
    translator: Arc<FakeLiveTranslator>,
    downloader: Arc<FakePackDownloader>,
    display_server: DisplayServer,
}

impl LiveHarness {
    fn new(
        capturer: Arc<FakeLiveCapturer>,
        ocr: Arc<FakeLiveOcr>,
        translator: Arc<FakeLiveTranslator>,
        display_server: DisplayServer,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let config_store = ConfigStore::new(temp_dir.path().join("config.toml"));
        let model_store = ModelStore::new(temp_dir.path().join("models"));
        Ok(Self {
            _temp_dir: temp_dir,
            config_store,
            model_store,
            capturer,
            clipboard: Arc::new(FakeClipboard::default()),
            ocr,
            translator,
            downloader: Arc::new(FakePackDownloader::default()),
            display_server,
        })
    }

    fn save_config(&self, config: &AppConfig) -> TestResult {
        self.config_store.save(config)?;
        Ok(())
    }

    fn install_pack(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> TestResult {
        let dir = self.model_store.pack_dir(pack, tier);
        std::fs::create_dir_all(&dir)?;
        let paths = self.model_store.pack_paths(pack, tier);
        std::fs::write(&paths.det, b"fake-det")?;
        std::fs::write(&paths.rec, b"fake-rec")?;
        std::fs::write(&paths.dict, b"a\nb\n")?;
        Ok(())
    }

    fn services(&self) -> AppServices {
        AppServices {
            config_store: self.config_store.clone(),
            model_store: self.model_store.clone(),
            model_manifest: ModelManifest::builtin(),
            downloader: Arc::clone(&self.downloader) as Arc<dyn PackDownloader>,
            capturer: Arc::clone(&self.capturer) as Arc<dyn ScreenCapturer>,
            clipboard: Arc::clone(&self.clipboard) as Arc<dyn ClipboardService>,
            ocr: Arc::clone(&self.ocr) as Arc<dyn OcrEngine>,
            translator: Arc::clone(&self.translator) as Arc<dyn Translator>,
            visual_search: Arc::new(FakeVisualSearcher),
            browser: Arc::new(FakeBrowserOpener),
            display_server: self.display_server,
            secrets: Arc::new(MemorySecretStore::new()) as Arc<dyn SecretStore>,
            translation_cache: Arc::new(AsyncMutex::new(TranslationCache::new(64))),
            provider_builder: Arc::new(DefaultProviderBuilder),
        }
    }
}

async fn yield_steps(count: usize) {
    for _ in 0..count {
        tokio::task::yield_now().await;
    }
}

async fn step_virtual_time(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
    yield_steps(6).await;
}

fn drain_events(rx: &mut UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

async fn wait_for_event<F>(
    rx: &mut UnboundedReceiver<UiEvent>,
    predicate: F,
    max_steps: usize,
) -> Result<UiEvent, Box<dyn std::error::Error>>
where
    F: Fn(&UiEvent) -> bool,
{
    for _ in 0..max_steps {
        yield_steps(4).await;
        while let Ok(ev) = rx.try_recv() {
            if predicate(&ev) {
                return Ok(ev);
            }
        }
        tokio::time::advance(Duration::from_millis(50)).await;
    }
    Err("expected UiEvent did not arrive within virtual-time budget".into())
}

#[tokio::test]
async fn live_translate_prompts_first_run_download_when_live_pack_missing_then_resumes()
-> TestResult {
    // Review Focus #5: missing live OCR pack opens FirstRunOcrModal, then resumes Live region picker.
    let capturer = Arc::new(FakeLiveCapturer::new(
        600,
        400,
        1.0,
        solid_frame(600, 400, 1.0, [245, 245, 245, 255]),
    ));
    let ocr = Arc::new(FakeLiveOcr::new(|_| Vec::new()));
    let translator = Arc::new(FakeLiveTranslator::with_pairs(&[]));
    let harness = LiveHarness::new(capturer, ocr, translator, DisplayServer::Windows)?;

    assert!(
        !harness
            .model_store
            .is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
    );

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let first = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
        .await?
        .ok_or("channel closed")?;
    assert_eq!(
        first,
        UiEvent::ShowFirstRunOcrModal {
            pack: OcrLanguagePack::CjkEn,
            tier: OcrModelTier::Mobile,
        }
    );

    cmd_tx.send(AppCommand::StartStarterDownload)?;

    let mut saw_hide_modal = false;
    let mut saw_picker = false;
    for _ in 0..12 {
        let ev = tokio::time::timeout(Duration::from_secs(2), ui_rx.recv())
            .await?
            .ok_or("channel closed")?;
        match ev {
            UiEvent::HideFirstRunOcrModal => saw_hide_modal = true,
            UiEvent::ShowLiveRegionPicker { monitor, frame } => {
                assert_eq!(monitor.width, 600);
                assert_eq!(frame.height, 400);
                saw_picker = true;
                break;
            }
            UiEvent::StarterDownloadProgress { .. } | UiEvent::ModelPacksUpdated(_) => {}
            other => {
                return Err(
                    format!("unexpected event during live starter resume: {other:?}").into(),
                );
            }
        }
    }

    assert!(saw_hide_modal, "expected HideFirstRunOcrModal");
    assert!(
        saw_picker,
        "expected ShowLiveRegionPicker automatically after starter pack download"
    );
    assert_eq!(harness.downloader.download_count(), 1);
    assert!(
        harness
            .model_store
            .is_installed(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
    );

    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn live_loop_ignores_rapid_scrolling_triggers_once_after_debounce_and_batches_page_bubbles()
-> TestResult {
    // Review Focus #1 + single batched page translation + RTL manga bubble ordering.
    tokio::time::pause();

    let capturer = Arc::new(FakeLiveCapturer::new(
        600,
        400,
        1.0,
        striped_frame(600, 400, 1.0, 0),
    ));
    let ocr = Arc::new(FakeLiveOcr::new(|_frame| {
        // Two vertical Japanese manga speech bubbles (2 columns each) inside the cropped region:
        // Bubble A (right, x=380..450): "危ない" (x=420) + "逃げろ！" (x=385) -> "危ない逃げろ！"
        // Bubble B (left,  x=70..140):  "大丈夫" (x=110) + "か？" (x=75)     -> "大丈夫か？"
        vec![
            OcrLine {
                bbox: PhysicalRect {
                    x: 75,
                    y: 65,
                    width: 28,
                    height: 80,
                },
                text: "か？".to_owned(),
                confidence: 0.94,
                is_vertical: true,
            },
            OcrLine {
                bbox: PhysicalRect {
                    x: 385,
                    y: 55,
                    width: 28,
                    height: 110,
                },
                text: "逃げろ！".to_owned(),
                confidence: 0.93,
                is_vertical: true,
            },
            OcrLine {
                bbox: PhysicalRect {
                    x: 110,
                    y: 60,
                    width: 28,
                    height: 95,
                },
                text: "大丈夫".to_owned(),
                confidence: 0.96,
                is_vertical: true,
            },
            OcrLine {
                bbox: PhysicalRect {
                    x: 420,
                    y: 50,
                    width: 28,
                    height: 105,
                },
                text: "危ない".to_owned(),
                confidence: 0.95,
                is_vertical: true,
            },
        ]
    }));
    let translator = Arc::new(FakeLiveTranslator::with_pairs(&[
        ("危ない逃げろ！", "Nguy hiểm, chạy mau!"),
        ("大丈夫か？", "Cậu có ổn không?"),
    ]));

    let harness = LiveHarness::new(
        Arc::clone(&capturer),
        Arc::clone(&ocr),
        Arc::clone(&translator),
        DisplayServer::Windows,
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::ShowLiveRegionPicker { .. }),
        10,
    )
    .await?;

    let region = LogicalRect {
        x: 40.0,
        y: 20.0,
        width: 500.0,
        height: 300.0,
    };
    cmd_tx.send(AppCommand::ConfirmLiveRegion(region))?;
    let started = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStarted { .. }),
        10,
    )
    .await?;
    assert!(matches!(
        started,
        UiEvent::LiveSessionStarted {
            display_mode: LiveDisplayMode::InPlace,
            can_inplace: true,
            notice: None,
            ..
        }
    ));

    // Rapid scrolling: change frames every 100ms for 500ms (< 350ms debounce).
    for step in 1..=5u32 {
        let frame = if step.is_multiple_of(2) {
            let v = (step * 45) as u8;
            solid_frame(600, 400, 1.0, [v, v, v, 255])
        } else {
            striped_frame(600, 400, 1.0, step * 5)
        };
        capturer.set_frame(frame);
        step_virtual_time(100).await;
        assert_eq!(
            ocr.call_count(),
            0,
            "OCR must NOT trigger while actively scrolling (step {step})"
        );
        assert_eq!(translator.call_count(), 0);
    }

    // Settle on a clean white manga page.
    capturer.set_frame(solid_frame(600, 400, 1.0, [250, 250, 250, 255]));
    // First observation of settled page (at t = 600ms) + 200ms stable (< 350ms): still 0 runs.
    step_virtual_time(300).await;
    assert_eq!(
        ocr.call_count(),
        0,
        "OCR must wait until screen has settled for >= 350ms"
    );

    // Advance past >= 350ms stable -> triggers exactly once.
    let updated = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. }),
        20,
    )
    .await?;

    match updated {
        UiEvent::LiveBlocksUpdated {
            blocks,
            status_text,
        } => {
            assert_eq!(blocks.len(), 2);
            assert_eq!(blocks[0].index, 1);
            assert_eq!(blocks[0].original_text, "危ない逃げろ！");
            assert_eq!(blocks[0].translated_text, "Nguy hiểm, chạy mau!");
            assert_eq!(blocks[1].index, 2);
            assert_eq!(blocks[1].original_text, "大丈夫か？");
            assert_eq!(blocks[1].translated_text, "Cậu có ổn không?");
            // White backdrop -> dark text [17, 24, 39, 255]
            assert_eq!(blocks[0].text_rgba, [17, 24, 39, 255]);
            assert!(blocks[0].font_size.is_finite() && blocks[0].font_size >= 9.0);
            assert!(!status_text.is_empty());

            // Also verify Slint row mapping helper cleanly converts rendered blocks.
            let rows = live_blocks_to_rows(&blocks);
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].index, 1);
            assert_eq!(rows[0].translated_text.as_str(), "Nguy hiểm, chạy mau!");
        }
        other => return Err(format!("expected LiveBlocksUpdated, got {other:?}").into()),
    }

    assert_eq!(ocr.call_count(), 1);
    assert_eq!(translator.call_count(), 1);
    assert_eq!(
        translator.batches(),
        vec![vec!["危ない逃げろ！".to_owned(), "大丈夫か？".to_owned()]]
    );

    // Screen remains identical for another 1000ms -> must NOT re-trigger OCR or translate.
    step_virtual_time(1000).await;
    assert_eq!(ocr.call_count(), 1);
    assert_eq!(translator.call_count(), 1);

    cmd_tx.send(AppCommand::StopLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStopped),
        10,
    )
    .await?;
    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn rapid_page_flip_discards_stale_slow_translation_and_displays_newest_page() -> TestResult {
    // Review Focus #2: slow Page 1 is aborted/dropped when user flips to Page 2.
    tokio::time::pause();

    let page1_frame = solid_frame(400, 300, 1.0, [240, 240, 240, 255]);
    let page2_frame = solid_frame(400, 300, 1.0, [30, 30, 35, 255]);

    let capturer = Arc::new(FakeLiveCapturer::new(400, 300, 1.0, page1_frame));
    let ocr = Arc::new(FakeLiveOcr::new(|frame| {
        let is_page1 = frame.rgba.first().copied().unwrap_or(0) > 128;
        let text = if is_page1 { "PAGE_ONE" } else { "PAGE_TWO" };
        vec![OcrLine {
            bbox: PhysicalRect {
                x: 20,
                y: 20,
                width: 120,
                height: 40,
            },
            text: text.to_owned(),
            confidence: 0.98,
            is_vertical: false,
        }]
    }));

    let page1_started = Arc::new(Notify::new());
    let page1_release = Arc::new(Notify::new());
    let translator = Arc::new(FakeLiveTranslator::blocking_on_marker(
        "PAGE_ONE",
        Arc::clone(&page1_started),
        Arc::clone(&page1_release),
        &[
            ("PAGE_ONE", "Trang cũ đã lỗi thời"),
            ("PAGE_TWO", "Trang mới nhất"),
        ],
    ));

    let harness = LiveHarness::new(
        Arc::clone(&capturer),
        Arc::clone(&ocr),
        Arc::clone(&translator),
        DisplayServer::Windows,
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;
    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::ShowLiveRegionPicker { .. }),
        10,
    )
    .await?;

    cmd_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 0.0,
        y: 0.0,
        width: 400.0,
        height: 300.0,
    }))?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStarted { .. }),
        10,
    )
    .await?;

    // Advance until Page 1 reaches the slow blocking translator.
    let mut reached_page1 = false;
    for _ in 0..20 {
        step_virtual_time(100).await;
        if translator.call_count() >= 1 {
            reached_page1 = true;
            break;
        }
    }
    assert!(reached_page1, "Page 1 should enter translator");

    // Flip to Page 2 BEFORE Page 1 returns!
    capturer.set_frame(page2_frame);

    // Wait for Page 2 to settle and emit LiveBlocksUpdated.
    let page2_event = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. }),
        30,
    )
    .await?;
    match page2_event {
        UiEvent::LiveBlocksUpdated { blocks, .. } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].translated_text, "Trang mới nhất");
            // Dark Page 2 backdrop -> light text [248, 250, 252, 255]
            assert_eq!(blocks[0].text_rgba, [248, 250, 252, 255]);
        }
        other => return Err(format!("expected Page 2 LiveBlocksUpdated, got {other:?}").into()),
    }

    // Now release the blocked Page 1 future and advance time: stale Page 1 MUST NEVER overwrite Page 2!
    page1_release.notify_waiters();
    step_virtual_time(600).await;

    let trailing = drain_events(&mut ui_rx);
    assert!(
        !trailing
            .iter()
            .any(|ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. })),
        "stale Page 1 blocks must never be emitted after Page 2, got {trailing:?}"
    );

    cmd_tx.send(AppCommand::StopLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStopped),
        10,
    )
    .await?;
    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn live_controls_toggle_pause_visibility_display_mode_and_stop_cleanly() -> TestResult {
    tokio::time::pause();

    let capturer = Arc::new(FakeLiveCapturer::new(
        320,
        240,
        1.0,
        solid_frame(320, 240, 1.0, [230, 230, 230, 255]),
    ));
    let ocr = Arc::new(FakeLiveOcr::new(|frame| {
        let bright = frame.rgba.first().copied().unwrap_or(0) > 128;
        vec![OcrLine {
            bbox: PhysicalRect {
                x: 10,
                y: 10,
                width: 100,
                height: 30,
            },
            text: if bright { "Hello" } else { "World" }.to_owned(),
            confidence: 0.95,
            is_vertical: false,
        }]
    }));
    let translator = Arc::new(FakeLiveTranslator::with_pairs(&[
        ("Hello", "Xin chào"),
        ("World", "Thế giới"),
    ]));

    let harness = LiveHarness::new(
        Arc::clone(&capturer),
        Arc::clone(&ocr),
        Arc::clone(&translator),
        DisplayServer::Windows,
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;
    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::ShowLiveRegionPicker { .. }),
        10,
    )
    .await?;
    cmd_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 0.0,
        y: 0.0,
        width: 320.0,
        height: 240.0,
    }))?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStarted { .. }),
        10,
    )
    .await?;

    // Wait for initial translation ("Xin chào").
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. }),
        20,
    )
    .await?;
    assert_eq!(ocr.call_count(), 1);

    // Pause -> suppresses capture & OCR even when screen changes.
    cmd_tx.send(AppCommand::ToggleLivePause)?;
    let paused = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LivePauseChanged(_)),
        10,
    )
    .await?;
    assert_eq!(paused, UiEvent::LivePauseChanged(true));

    capturer.set_frame(solid_frame(320, 240, 1.0, [20, 20, 20, 255]));
    step_virtual_time(1000).await;
    assert_eq!(
        ocr.call_count(),
        1,
        "paused Live Translate must suppress OCR/translation"
    );

    // Resume -> changed settled page now translates.
    cmd_tx.send(AppCommand::ToggleLivePause)?;
    let resumed = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LivePauseChanged(_)),
        10,
    )
    .await?;
    assert_eq!(resumed, UiEvent::LivePauseChanged(false));

    let second_blocks = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. }),
        25,
    )
    .await?;
    match second_blocks {
        UiEvent::LiveBlocksUpdated { blocks, .. } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].translated_text, "Thế giới");
        }
        other => return Err(format!("expected resumed LiveBlocksUpdated, got {other:?}").into()),
    }

    // Visibility toggle (Ctrl+Shift+H / Hide-Show button).
    cmd_tx.send(AppCommand::ToggleLiveVisibility)?;
    assert_eq!(
        wait_for_event(
            &mut ui_rx,
            |ev| matches!(ev, UiEvent::LiveVisibilityChanged(_)),
            10
        )
        .await?,
        UiEvent::LiveVisibilityChanged(false)
    );
    cmd_tx.send(AppCommand::ToggleLiveVisibility)?;
    assert_eq!(
        wait_for_event(
            &mut ui_rx,
            |ev| matches!(ev, UiEvent::LiveVisibilityChanged(_)),
            10
        )
        .await?,
        UiEvent::LiveVisibilityChanged(true)
    );

    // Display mode switch (InPlace <-> FloatingPanel).
    cmd_tx.send(AppCommand::SwitchLiveDisplayMode)?;
    assert_eq!(
        wait_for_event(
            &mut ui_rx,
            |ev| matches!(ev, UiEvent::LiveDisplayModeChanged(_)),
            10
        )
        .await?,
        UiEvent::LiveDisplayModeChanged(LiveDisplayMode::FloatingPanel)
    );
    assert_eq!(
        live_display_mode_to_id(LiveDisplayMode::FloatingPanel),
        "floating-panel"
    );
    assert_eq!(
        live_display_mode_to_id(LiveDisplayMode::InPlace),
        "in-place"
    );

    // Copy block to clipboard.
    cmd_tx.send(AppCommand::CopyLiveBlock("Thế giới".to_owned()))?;
    let _ = wait_for_event(&mut ui_rx, |ev| matches!(ev, UiEvent::ShowToast { .. }), 10).await?;
    assert_eq!(harness.clipboard.text_writes(), vec!["Thế giới".to_owned()]);

    // Stop Live Translate -> emits LiveSessionStopped and background loop terminates cleanly.
    cmd_tx.send(AppCommand::StopLiveTranslate)?;
    let stopped = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStopped),
        10,
    )
    .await?;
    assert_eq!(stopped, UiEvent::LiveSessionStopped);

    yield_steps(6).await;
    let captures_at_stop = capturer.monitor_capture_count();
    step_virtual_time(2000).await;
    assert_eq!(
        capturer.monitor_capture_count(),
        captures_at_stop,
        "background capture loop must terminate completely after StopLiveTranslate"
    );

    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn live_rate_limit_emits_actionable_banner_event_without_crashing() -> TestResult {
    tokio::time::pause();

    let page1 = solid_frame(320, 240, 1.0, [240, 240, 240, 255]);
    let page2 = solid_frame(320, 240, 1.0, [30, 30, 30, 255]);

    let capturer = Arc::new(FakeLiveCapturer::new(320, 240, 1.0, page1));
    let ocr = Arc::new(FakeLiveOcr::new(|frame| {
        let is_page1 = frame.rgba.first().copied().unwrap_or(0) > 128;
        vec![OcrLine {
            bbox: PhysicalRect {
                x: 20,
                y: 20,
                width: 100,
                height: 30,
            },
            text: if is_page1 {
                "RATE_LIMIT_ME"
            } else {
                "RECOVERED"
            }
            .to_owned(),
            confidence: 0.96,
            is_vertical: false,
        }]
    }));
    let translator = Arc::new(FakeLiveTranslator::rate_limit_on_marker(
        "RATE_LIMIT_ME",
        &[("RECOVERED", "Đã hồi phục bình thường")],
    ));

    let harness = LiveHarness::new(
        Arc::clone(&capturer),
        Arc::clone(&ocr),
        Arc::clone(&translator),
        DisplayServer::Windows,
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;
    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::ShowLiveRegionPicker { .. }),
        10,
    )
    .await?;
    cmd_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 0.0,
        y: 0.0,
        width: 320.0,
        height: 240.0,
    }))?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStarted { .. }),
        10,
    )
    .await?;

    let rate_limited = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveRateLimited { .. }),
        25,
    )
    .await?;
    match rate_limited {
        UiEvent::LiveRateLimited { message } => {
            assert!(
                message.contains("429") || message.contains("giới hạn"),
                "expected actionable rate-limit message, got: {message}"
            );
        }
        other => return Err(format!("expected LiveRateLimited, got {other:?}").into()),
    }

    // Loop must NOT spam requests on the same static frame while rate-limited.
    let calls_after_429 = translator.call_count();
    step_virtual_time(1000).await;
    assert_eq!(translator.call_count(), calls_after_429);

    // Flipping to Page 2 proves the loop stayed alive and recovers cleanly.
    capturer.set_frame(page2);
    let recovered = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. }),
        25,
    )
    .await?;
    match recovered {
        UiEvent::LiveBlocksUpdated { blocks, .. } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].translated_text, "Đã hồi phục bình thường");
        }
        other => return Err(format!("expected recovery LiveBlocksUpdated, got {other:?}").into()),
    }

    cmd_tx.send(AppCommand::StopLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStopped),
        10,
    )
    .await?;
    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn wayland_inplace_falls_back_to_floating_panel_with_notice_and_blocks_inplace_switch()
-> TestResult {
    // Review Focus #5: Wayland InPlace automatically resolves to FloatingPanel with Vietnamese notice.
    tokio::time::pause();

    let capturer = Arc::new(FakeLiveCapturer::new(
        400,
        300,
        1.0,
        solid_frame(400, 300, 1.0, [245, 245, 245, 255]),
    ));
    let ocr = Arc::new(FakeLiveOcr::new(|_| Vec::new()));
    let translator = Arc::new(FakeLiveTranslator::with_pairs(&[]));

    let harness = LiveHarness::new(
        capturer,
        ocr,
        translator,
        DisplayServer::LinuxWayland {
            compositor: WaylandCompositor::Gnome,
        },
    )?;
    let mut cfg = AppConfig::default();
    cfg.live.display_mode = LiveDisplayMode::InPlace;
    cfg.live.reading_direction = ReadingDirection::Auto;
    harness.save_config(&cfg)?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;
    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::ShowLiveRegionPicker { .. }),
        10,
    )
    .await?;

    cmd_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 300.0,
        height: 200.0,
    }))?;
    let started = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStarted { .. }),
        10,
    )
    .await?;
    match started {
        UiEvent::LiveSessionStarted {
            display_mode,
            can_inplace,
            notice,
            ..
        } => {
            assert_eq!(display_mode, LiveDisplayMode::FloatingPanel);
            assert!(!can_inplace);
            assert!(
                notice.as_deref().is_some_and(|n| n.contains("Wayland")),
                "expected Vietnamese Wayland fallback notice, got {notice:?}"
            );
        }
        other => return Err(format!("expected LiveSessionStarted, got {other:?}").into()),
    }

    // Attempting to switch back to InPlace on Wayland must NOT emit LiveDisplayModeChanged(InPlace).
    cmd_tx.send(AppCommand::SwitchLiveDisplayMode)?;
    let toast =
        wait_for_event(&mut ui_rx, |ev| matches!(ev, UiEvent::ShowToast { .. }), 10).await?;
    assert!(matches!(toast, UiEvent::ShowToast { ref message } if message.contains("Wayland")));

    cmd_tx.send(AppCommand::StopLiveTranslate)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStopped),
        10,
    )
    .await?;
    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn reselect_live_region_and_select_live_window_run_cleanly() -> TestResult {
    tokio::time::pause();

    let capturer = Arc::new(FakeLiveCapturer::new(
        500,
        360,
        1.0,
        solid_frame(500, 360, 1.0, [240, 240, 240, 255]),
    ));
    capturer.set_window(
        WindowGeometry {
            id: 77,
            title: "Manga Reader".to_owned(),
            app_name: "reader".to_owned(),
            x: 100,
            y: 50,
            width: 300,
            height: 200,
            scale_factor: 1.0,
            is_minimized: false,
        },
        solid_frame(300, 200, 1.0, [250, 250, 250, 255]),
    );
    let ocr = Arc::new(FakeLiveOcr::new(|_| {
        vec![OcrLine {
            bbox: PhysicalRect {
                x: 20,
                y: 20,
                width: 100,
                height: 30,
            },
            text: "Start!".to_owned(),
            confidence: 0.99,
            is_vertical: false,
        }]
    }));
    let translator = Arc::new(FakeLiveTranslator::with_pairs(&[("Start!", "Bắt đầu!")]));

    let harness = LiveHarness::new(
        Arc::clone(&capturer),
        Arc::clone(&ocr),
        Arc::clone(&translator),
        DisplayServer::LinuxX11,
    )?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    // Target a specific window (Linux X11 anti-self-capture path).
    cmd_tx.send(AppCommand::SelectLiveWindow(77))?;
    let started = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStarted { .. }),
        10,
    )
    .await?;
    assert!(matches!(
        started,
        UiEvent::LiveSessionStarted {
            display_mode: LiveDisplayMode::InPlace,
            can_inplace: true,
            ..
        }
    ));

    let updated = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveBlocksUpdated { .. }),
        25,
    )
    .await?;
    match updated {
        UiEvent::LiveBlocksUpdated { blocks, .. } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].translated_text, "Bắt đầu!");
            let _: &LiveRenderedBlock = &blocks[0];
        }
        other => return Err(format!("expected LiveBlocksUpdated, got {other:?}").into()),
    }
    assert!(capturer.window_capture_count() >= 2);

    // Reselect region stops current session and reopens region picker.
    cmd_tx.send(AppCommand::ReselectLiveRegion)?;
    let _ = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::LiveSessionStopped),
        10,
    )
    .await?;
    let picker = wait_for_event(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::ShowLiveRegionPicker { .. }),
        10,
    )
    .await?;
    assert!(matches!(picker, UiEvent::ShowLiveRegionPicker { .. }));

    cmd_tx.send(AppCommand::Quit)?;
    let _ = handle.await;
    Ok(())
}
