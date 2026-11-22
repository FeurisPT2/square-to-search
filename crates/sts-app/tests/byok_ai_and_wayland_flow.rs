//! End-to-end integration tests for Milestone 3:
//! - Streaming Ask AI with cropped PNG + preset prompt + clean Stop/Cancel (`Review Focus #2`)
//! - OS Keyring secret isolation vs `config.toml` and `Debug` redaction (`Review Focus #1`, `#3`)
//! - Hot-reloading BYOK providers without restarting `AppController`
//! - Wayland Decision 2A fullscreen `can_inplace` policy (`Review Focus #4`)

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sts_app::state::{
    AppCommand, AppController, AppServices, BoxFuture, DefaultProviderBuilder, PackDownloader,
    ProgressCallback, ProviderTestTarget, UiEvent, VisualSearcher,
};
use sts_core::cache::TranslationCache;
use sts_core::config::{AppConfig, ConfigStore, SecretSlot, VisualSearchConfig};
use sts_core::geometry::{LogicalRect, MonitorGeometry, PhysicalRect};
use sts_core::types::{CaptureFrame, LiveDisplayMode, OcrLanguagePack, OcrLine, OcrModelTier};
use sts_ocr::OcrError;
use sts_ocr::engine::OcrEngine;
use sts_ocr::manifest::{ModelManifest, ModelPackSpec, ModelStore};
use sts_platform::PlatformError;
use sts_platform::capture::ScreenCapturer;
use sts_platform::clipboard::ClipboardService;
use sts_platform::display_env::{DisplayServer, WaylandCompositor};
use sts_platform::secrets::{MemorySecretStore, SecretStore};
use sts_search::{BrowserOpener, SearchError, VisualSearchOutcome};
use sts_translate::{TranslateError, TranslateRequest, TranslateResponse, Translator};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc::UnboundedReceiver};
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

struct StaticCapturer {
    monitor: MonitorGeometry,
    frame: CaptureFrame,
}

impl StaticCapturer {
    fn new(width: u32, height: u32, scale_factor: f32) -> Self {
        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[(x & 0xFF) as u8, (y & 0xFF) as u8, 200, 255]);
            }
        }
        Self {
            monitor: MonitorGeometry {
                id: 1,
                name: "PrimaryTestMonitor".to_owned(),
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

impl ScreenCapturer for StaticCapturer {
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

#[derive(Default)]
struct NoopClipboard;

impl ClipboardService for NoopClipboard {
    fn set_text(&self, _text: &str) -> Result<(), PlatformError> {
        Ok(())
    }

    fn set_image_rgba(&self, _w: usize, _h: usize, _rgba: &[u8]) -> Result<(), PlatformError> {
        Ok(())
    }
}

#[derive(Default)]
struct NoopBrowser;

impl BrowserOpener for NoopBrowser {
    fn open_url(&self, _url: &str) -> Result<(), SearchError> {
        Ok(())
    }
}

#[derive(Default)]
struct NoopVisualSearcher;

impl VisualSearcher for NoopVisualSearcher {
    fn search<'a>(
        &'a self,
        _frame: &'a CaptureFrame,
        config: &'a VisualSearchConfig,
    ) -> BoxFuture<'a, Result<VisualSearchOutcome, SearchError>> {
        Box::pin(async move {
            Ok(VisualSearchOutcome::OpenedResultUrl {
                engine: config.engine,
                url: "https://example.invalid".to_owned(),
            })
        })
    }
}

#[derive(Default)]
struct NoopDownloader;

impl PackDownloader for NoopDownloader {
    fn download_pack<'a>(
        &'a self,
        _store: &'a ModelStore,
        _spec: &'a ModelPackSpec,
        _on_progress: ProgressCallback,
    ) -> BoxFuture<'a, Result<PathBuf, OcrError>> {
        Box::pin(async { Err(OcrError::InferenceFailed("unused in test".to_owned())) })
    }
}

struct DynamicOcr {
    current_text: Mutex<String>,
    calls: AtomicUsize,
}

impl DynamicOcr {
    fn new(text: impl Into<String>) -> Self {
        Self {
            current_text: Mutex::new(text.into()),
            calls: AtomicUsize::new(0),
        }
    }

    fn set_text(&self, next: impl Into<String>) {
        match self.current_text.lock() {
            Ok(mut guard) => *guard = next.into(),
            Err(poisoned) => *poisoned.into_inner() = next.into(),
        }
    }
}

impl OcrEngine for DynamicOcr {
    fn recognize<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        _pack: OcrLanguagePack,
        _tier: OcrModelTier,
    ) -> BoxFuture<'a, Result<Vec<OcrLine>, OcrError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = match self.current_text.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            Ok(vec![OcrLine {
                bbox: PhysicalRect {
                    x: 0,
                    y: 0,
                    width: frame.width,
                    height: frame.height,
                },
                text,
                confidence: 0.99,
                is_vertical: false,
            }])
        })
    }
}

struct StubInitialTranslator;

impl Translator for StubInitialTranslator {
    fn provider_name(&self) -> &'static str {
        "stub-free-web"
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(async move {
            let translations = request
                .texts
                .iter()
                .map(|t| format!("[fake] {t}"))
                .collect();
            Ok(TranslateResponse {
                translations,
                provider_name: self.provider_name().to_owned(),
            })
        })
    }
}

struct M3TestHarness {
    temp_dir: TempDir,
    config_store: ConfigStore,
    model_store: ModelStore,
    secrets: Arc<MemorySecretStore>,
    ocr: Arc<DynamicOcr>,
    display_server: DisplayServer,
    capturer: Arc<StaticCapturer>,
    translation_cache: Arc<AsyncMutex<TranslationCache>>,
}

impl M3TestHarness {
    fn new(
        display_server: DisplayServer,
        width: u32,
        height: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let config_store = ConfigStore::new(temp_dir.path().join("config.toml"));
        let model_store = ModelStore::new(temp_dir.path().join("models"));
        Ok(Self {
            temp_dir,
            config_store,
            model_store,
            secrets: Arc::new(MemorySecretStore::new()),
            ocr: Arc::new(DynamicOcr::new("セリフ")),
            display_server,
            capturer: Arc::new(StaticCapturer::new(width, height, 1.0)),
            translation_cache: Arc::new(AsyncMutex::new(TranslationCache::new(64))),
        })
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
            downloader: Arc::new(NoopDownloader),
            capturer: Arc::clone(&self.capturer) as Arc<dyn ScreenCapturer>,
            clipboard: Arc::new(NoopClipboard),
            ocr: Arc::clone(&self.ocr) as Arc<dyn OcrEngine>,
            translator: Arc::new(StubInitialTranslator),
            visual_search: Arc::new(NoopVisualSearcher),
            browser: Arc::new(NoopBrowser),
            display_server: self.display_server,
            secrets: Arc::clone(&self.secrets) as Arc<dyn SecretStore>,
            translation_cache: Arc::clone(&self.translation_cache),
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

async fn recv_until<F>(
    rx: &mut UnboundedReceiver<UiEvent>,
    mut predicate: F,
) -> Result<UiEvent, Box<dyn std::error::Error>>
where
    F: FnMut(&UiEvent) -> bool,
{
    for _ in 0..32 {
        let ev = recv_event(rx).await?;
        if predicate(&ev) {
            return Ok(ev);
        }
    }
    Err("expected UiEvent did not arrive within 32 events".into())
}

fn drain_events(rx: &mut UnboundedReceiver<UiEvent>) -> Vec<UiEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

fn decode_base64_for_test(input: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut out = Vec::with_capacity((input.len() / 4) * 3);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            other => return Err(format!("invalid base64 byte: {other}").into()),
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Ok(out)
}

async fn spawn_gated_sse_server(
    chunk1: &'static str,
    chunk2: &'static str,
    release_chunk2: Arc<Notify>,
) -> Result<(String, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut buf = [0u8; 8192];
        let _ = socket.read(&mut buf).await;

        let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(headers.as_bytes()).await;

        let part1 =
            format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{chunk1}\"}}}}]}}\n\n");
        let frame1 = format!("{:X}\r\n{}\r\n", part1.len(), part1);
        let _ = socket.write_all(frame1.as_bytes()).await;
        let _ = socket.flush().await;

        // Hold stream open mid-flight until test releases chunk2.
        release_chunk2.notified().await;

        let part2 = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{chunk2}\"}}}}]}}\n\ndata: [DONE]\n\n"
        );
        let frame2 = format!("{:X}\r\n{}\r\n0\r\n\r\n", part2.len(), part2);
        let _ = socket.write_all(frame2.as_bytes()).await;
        let _ = socket.flush().await;
    });
    Ok((format!("http://{addr}/v1"), handle))
}

#[tokio::test]
async fn ask_ai_streams_with_cropped_png_and_preset_prompt_then_stops_cleanly() -> TestResult {
    let server = MockServer::start().await;
    let sse_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Đây là \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"cảnh manga \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hành động.\"}}]}\n\n",
        "data: [DONE]\n\n"
    );

    let preset_prompt = "Giải thích ngắn gọn nội dung và ngữ cảnh trong vùng ảnh này.";
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-ask-ai-e2e"))
        .and(body_string_contains("\"stream\":true"))
        .and(body_string_contains("data:image/png;base64,"))
        .and(body_string_contains(preset_prompt))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let harness = M3TestHarness::new(DisplayServer::Windows, 320, 240)?;
    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    // Save OpenAI key in OS keyring and configure Ask AI to use mock OpenAI-compatible server.
    cmd_tx.send(AppCommand::SaveApiKey {
        slot: SecretSlot::OpenAiApiKey,
        value: "sk-ask-ai-e2e".to_owned(),
    })?;
    let keyring_ev = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        keyring_ev,
        UiEvent::KeyringStatusUpdated {
            has_openai: true,
            ..
        }
    ));

    let mut cfg = AppConfig::default();
    cfg.translate.ai_provider = Some("openai".to_owned());
    cfg.translate.openai_base_url = format!("{}/v1", server.uri());
    cfg.translate.openai_model = "gpt-4o-mini".to_owned();
    cmd_tx.send(AppCommand::SaveSettings(cfg))?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ModelPacksUpdated(_))).await?;

    // Capture freeze frame and invoke Ask AI on a 100x60 crop.
    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowSnipOverlay { .. })
    })
    .await?;

    let rect = LogicalRect {
        x: 20.0,
        y: 10.0,
        width: 100.0,
        height: 60.0,
    };
    cmd_tx.send(AppCommand::RunAskAi(rect))?;
    let ready = recv_event(&mut ui_rx).await?;
    match ready {
        UiEvent::ShowAskAiReady { provider_label } => {
            assert!(provider_label.contains("OpenAI"));
            assert!(provider_label.contains("gpt-4o-mini"));
        }
        other => return Err(format!("expected ShowAskAiReady, got {other:?}").into()),
    }

    cmd_tx.send(AppCommand::SubmitAskAiPrompt(preset_prompt.to_owned()))?;
    let started = recv_event(&mut ui_rx).await?;
    assert!(matches!(started, UiEvent::AskAiStreamStarted { .. }));

    let mut streamed = String::new();
    let mut delta_count = 0usize;
    let mut finished_answer = None;
    for _ in 0..16 {
        match recv_event(&mut ui_rx).await? {
            UiEvent::AskAiStreamDelta { chunk } => {
                delta_count += 1;
                streamed.push_str(&chunk);
            }
            UiEvent::AskAiStreamFinished { full_answer, .. } => {
                finished_answer = Some(full_answer);
                break;
            }
            other => {
                return Err(format!("unexpected Ask AI stream event: {other:?}").into());
            }
        }
    }

    assert!(
        delta_count >= 2,
        "expected multiple stream deltas, got {delta_count}"
    );
    assert_eq!(streamed, "Đây là cảnh manga hành động.");
    assert_eq!(
        finished_answer.as_deref(),
        Some("Đây là cảnh manga hành động.")
    );

    // Verify HTTP body contained a valid 100x60 PNG encoded from the cropped CaptureFrame.
    let requests = server
        .received_requests()
        .await
        .ok_or("wiremock request recording unavailable")?;
    assert_eq!(requests.len(), 1);
    let body_text = std::str::from_utf8(&requests[0].body)?;
    let prefix = "data:image/png;base64,";
    let b64_start = body_text
        .find(prefix)
        .ok_or("missing PNG data URL prefix in OpenAI request")?
        + prefix.len();
    let b64_tail = &body_text[b64_start..];
    let b64_end = b64_tail
        .find('"')
        .ok_or("unterminated PNG data URL string in OpenAI request")?;
    let png_bytes = decode_base64_for_test(&b64_tail[..b64_end])?;
    let decoded = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)?;
    assert_eq!(decoded.width(), 100);
    assert_eq!(decoded.height(), 60);

    // Also verify TestAiProvider(AskAi) hits wiremock and best-effort OCR runs when installed.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-ask-ai-e2e"))
        .and(body_string_contains("\"stream\":false"))
        .and(body_string_contains("Reply OK"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"choices":[{"message":{"role":"assistant","content":"OK"}}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    cmd_tx.send(AppCommand::TestAiProvider(ProviderTestTarget::AskAi))?;
    let ask_test = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ProviderTestFinished { .. })
    })
    .await?;
    assert!(matches!(
        ask_test,
        UiEvent::ProviderTestFinished { ok: true, ref message } if message.contains("openai")
    ));

    // Part B (Review Focus #2): StopAskAi mid-stream guarantees ZERO trailing AskAiStreamDelta.
    let release_stop = Arc::new(Notify::new());
    let (gated_url_stop, server_stop) = spawn_gated_sse_server(
        "Phần một ",
        "DELTA_AFTER_STOP_FORBIDDEN",
        Arc::clone(&release_stop),
    )
    .await?;

    let mut stop_cfg = AppConfig::default();
    stop_cfg.translate.ai_provider = Some("openai".to_owned());
    stop_cfg.translate.openai_base_url = gated_url_stop;
    stop_cfg.translate.openai_model = "gpt-4o-mini".to_owned();
    cmd_tx.send(AppCommand::SaveSettings(stop_cfg))?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ModelPacksUpdated(_))).await?;

    cmd_tx.send(AppCommand::SubmitAskAiPrompt("Tóm tắt cảnh này".to_owned()))?;
    let _ = recv_until(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::AskAiStreamDelta { chunk } if chunk == "Phần một "),
    )
    .await?;

    cmd_tx.send(AppCommand::StopAskAi)?;
    let stop_finished = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::AskAiStreamFinished { .. })
    })
    .await?;
    assert!(matches!(
        stop_finished,
        UiEvent::AskAiStreamFinished { ref full_answer, .. } if full_answer == "Phần một "
    ));

    // Release delayed second SSE chunk and verify ZERO trailing AskAiStreamDelta arrive.
    release_stop.notify_waiters();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_stop).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let trailing_after_stop = drain_events(&mut ui_rx);
    assert!(
        !trailing_after_stop
            .iter()
            .any(|ev| matches!(ev, UiEvent::AskAiStreamDelta { .. })),
        "unexpected trailing AskAiStreamDelta after StopAskAi: {trailing_after_stop:?}"
    );

    // Part C (Review Focus #2): CancelOverlay mid-stream also aborts cleanly with ZERO trailing deltas.
    let release_cancel = Arc::new(Notify::new());
    let (gated_url_cancel, server_cancel) = spawn_gated_sse_server(
        "Khởi đầu ",
        "DELTA_AFTER_CANCEL_FORBIDDEN",
        Arc::clone(&release_cancel),
    )
    .await?;

    let mut cancel_cfg = AppConfig::default();
    cancel_cfg.translate.ai_provider = Some("openai".to_owned());
    cancel_cfg.translate.openai_base_url = gated_url_cancel;
    cancel_cfg.translate.openai_model = "gpt-4o-mini".to_owned();
    cmd_tx.send(AppCommand::SaveSettings(cancel_cfg))?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ModelPacksUpdated(_))).await?;

    cmd_tx.send(AppCommand::SubmitAskAiPrompt("Dịch tự nhiên".to_owned()))?;
    let _ = recv_until(
        &mut ui_rx,
        |ev| matches!(ev, UiEvent::AskAiStreamDelta { chunk } if chunk == "Khởi đầu "),
    )
    .await?;

    cmd_tx.send(AppCommand::CancelOverlay)?;
    let hide_ev = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::HideSnipOverlay)).await?;
    assert_eq!(hide_ev, UiEvent::HideSnipOverlay);

    release_cancel.notify_waiters();
    let _ = tokio::time::timeout(Duration::from_secs(2), server_cancel).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let trailing_after_cancel = drain_events(&mut ui_rx);
    assert!(
        !trailing_after_cancel.iter().any(|ev| matches!(
            ev,
            UiEvent::AskAiStreamDelta { .. }
                | UiEvent::AskAiStreamFinished { .. }
                | UiEvent::AskAiStreamError { .. }
        )),
        "unexpected trailing Ask AI events after CancelOverlay: {trailing_after_cancel:?}"
    );

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::Quit)).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn saving_api_keys_stores_only_in_secret_store_never_in_config_toml() -> TestResult {
    let harness = M3TestHarness::new(DisplayServer::Windows, 240, 180)?;
    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    let gemini_secret = "AIzaSy-SENTINEL-GEMINI-SECRET-111";
    let openai_secret = "sk-SENTINEL-OPENAI-SECRET-222";
    let deepl_secret = "dl-SENTINEL-DEEPL-SECRET-333:fx";

    // Review Focus #1: AppCommand Debug output must redact API key values.
    let save_cmd = AppCommand::SaveApiKey {
        slot: SecretSlot::GeminiApiKey,
        value: gemini_secret.to_owned(),
    };
    let debug_cmd = format!("{save_cmd:?}");
    assert!(
        !debug_cmd.contains(gemini_secret),
        "AppCommand Debug leaked secret: {debug_cmd}"
    );
    cmd_tx.send(save_cmd)?;
    let ev1 = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        ev1,
        UiEvent::KeyringStatusUpdated {
            has_gemini: true,
            has_openai: false,
            has_deepl: false,
            ..
        }
    ));

    cmd_tx.send(AppCommand::SaveApiKey {
        slot: SecretSlot::OpenAiApiKey,
        value: openai_secret.to_owned(),
    })?;
    let ev2 = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        ev2,
        UiEvent::KeyringStatusUpdated {
            has_gemini: true,
            has_openai: true,
            has_deepl: false,
            ..
        }
    ));

    cmd_tx.send(AppCommand::SaveApiKey {
        slot: SecretSlot::DeepLApiKey,
        value: deepl_secret.to_owned(),
    })?;
    let ev3 = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        ev3,
        UiEvent::KeyringStatusUpdated {
            has_gemini: true,
            has_openai: true,
            has_deepl: true,
            ..
        }
    ));

    // Persist BYOK settings and verify ShowSettings reports keyring presence without secrets.
    let mut cfg = AppConfig::default();
    cfg.translate.provider = "openai".to_owned();
    cfg.translate.ai_provider = Some("gemini".to_owned());
    cfg.translate.gemini_model = "gemini-2.5-flash".to_owned();
    cfg.translate.openai_base_url = "https://proxy.example.invalid/v1".to_owned();
    cfg.translate.openai_model = "gpt-4o-mini".to_owned();
    cfg.translate.deepl_free_tier = false;
    cmd_tx.send(AppCommand::SaveSettings(cfg))?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ModelPacksUpdated(_))).await?;

    cmd_tx.send(AppCommand::OpenSettingsTab(3))?;
    let show_settings = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        show_settings,
        UiEvent::ShowSettings {
            tab: 3,
            has_gemini_key: true,
            has_openai_key: true,
            has_deepl_key: true,
            ..
        }
    ));

    // Verify raw config.toml on disk NEVER contains any literal API key or api_key field.
    let config_path = harness.temp_dir.path().join("config.toml");
    let raw_bytes = std::fs::read(&config_path)?;
    let raw_text = String::from_utf8(raw_bytes)?;
    for secret in [gemini_secret, openai_secret, deepl_secret] {
        assert!(
            !raw_text.contains(secret),
            "secret leaked into config.toml:\n{raw_text}"
        );
    }
    assert!(
        !raw_text.to_ascii_lowercase().contains("api_key"),
        "unexpected api_key field in config.toml:\n{raw_text}"
    );
    assert!(raw_text.contains("gemini_model = \"gemini-2.5-flash\""));
    assert!(raw_text.contains("deepl_free_tier = false"));

    // Verify SecretStore holds all 3 secrets and Debug redacts them.
    assert_eq!(
        harness
            .secrets
            .get_secret(SecretSlot::GeminiApiKey)?
            .as_deref(),
        Some(gemini_secret)
    );
    let store_dbg = format!("{:?}", harness.secrets);
    assert!(!store_dbg.contains(gemini_secret));
    assert!(!store_dbg.contains(openai_secret));
    assert!(!store_dbg.contains(deepl_secret));

    // Clear Gemini key -> RunAskAi must immediately see ShowAskAiUnconfigured.
    cmd_tx.send(AppCommand::ClearApiKey(SecretSlot::GeminiApiKey))?;
    let cleared = recv_event(&mut ui_rx).await?;
    assert!(matches!(
        cleared,
        UiEvent::KeyringStatusUpdated {
            has_gemini: false,
            has_openai: true,
            has_deepl: true,
            ai_provider_configured: false,
            ..
        }
    ));

    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowSnipOverlay { .. })
    })
    .await?;
    cmd_tx.send(AppCommand::RunAskAi(LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 80.0,
        height: 40.0,
    }))?;
    let ask_ev = recv_event(&mut ui_rx).await?;
    assert_eq!(ask_ev, UiEvent::ShowAskAiUnconfigured);

    // Switching Live translator to "gemini" without its key falls back gracefully to Free Web + toast.
    let mut missing_live_cfg = AppConfig::default();
    missing_live_cfg.translate.provider = "gemini".to_owned();
    cmd_tx.send(AppCommand::SaveSettings(missing_live_cfg))?;
    let toast = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ShowToast { .. })).await?;
    assert!(matches!(
        toast,
        UiEvent::ShowToast { ref message } if message.contains("Free Web") || message.contains("gemini")
    ));

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::Quit)).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn hot_reloading_settings_switches_live_translator_to_byok_openai() -> TestResult {
    let harness = M3TestHarness::new(DisplayServer::Windows, 320, 240)?;
    harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;
    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(harness.services())?;

    // Step 1: Initial default uses injected Free Web / stub translator.
    cmd_tx.send(AppCommand::TriggerSquareToSearch)?;
    let _ = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowSnipOverlay { .. })
    })
    .await?;

    let rect = LogicalRect {
        x: 10.0,
        y: 10.0,
        width: 120.0,
        height: 60.0,
    };
    cmd_tx.send(AppCommand::RunQuickTranslate(rect))?;
    let first_result = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowInlineResult { .. })
    })
    .await?;
    assert!(matches!(
        first_result,
        UiEvent::ShowInlineResult {
            translated_text: Some(ref text),
            ..
        } if text == "[fake] セリフ"
    ));

    // Step 2: Mount local wiremock OpenAI endpoint & hot-reload settings WITHOUT restarting AppController.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-hotreload-live"))
        .and(body_string_contains("\"stream\":false"))
        .and(body_string_contains("Hello"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"choices":[{"message":{"role":"assistant","content":"Xin chào!"}}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-hotreload-live"))
        .and(body_string_contains("\"stream\":false"))
        .and(body_string_contains("新しいセリフ"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"choices":[{"message":{"role":"assistant","content":"Lời thoại mới từ OpenAI BYOK"}}]}"#,
            "application/json",
        ))
        .expect(1)
        .mount(&server)
        .await;

    cmd_tx.send(AppCommand::SaveApiKey {
        slot: SecretSlot::OpenAiApiKey,
        value: "sk-hotreload-live".to_owned(),
    })?;
    let _ = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::KeyringStatusUpdated { .. })
    })
    .await?;

    let mut cfg = AppConfig::default();
    cfg.translate.provider = "openai".to_owned();
    cfg.translate.openai_base_url = format!("{}/v1", server.uri());
    cfg.translate.openai_model = "gpt-4o-mini".to_owned();
    cfg.translate.custom_prompt = "Dịch tự nhiên".to_owned();
    cmd_tx.send(AppCommand::SaveSettings(cfg))?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ModelPacksUpdated(_))).await?;

    // Step 3: TestAiProvider(LiveTranslator) hits wiremock OpenAI endpoint, while unconfigured AskAi reports ok=false.
    cmd_tx.send(AppCommand::TestAiProvider(ProviderTestTarget::AskAi))?;
    let ask_unconf_test = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ProviderTestFinished { .. })
    })
    .await?;
    assert!(matches!(
        ask_unconf_test,
        UiEvent::ProviderTestFinished { ok: false, .. }
    ));

    cmd_tx.send(AppCommand::TestAiProvider(
        ProviderTestTarget::LiveTranslator,
    ))?;
    let test_ev = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ProviderTestFinished { .. })
    })
    .await?;
    match test_ev {
        UiEvent::ProviderTestFinished { ok, message } => {
            assert!(ok, "expected live translator test to succeed: {message}");
            assert!(
                message.contains("openai"),
                "expected provider name in test message, got: {message}"
            );
        }
        other => return Err(format!("expected ProviderTestFinished, got {other:?}").into()),
    }

    // Step 4: RunQuickTranslate now uses the hot-reloaded OpenAI BYOK translator!
    harness.ocr.set_text("新しいセリフ");
    cmd_tx.send(AppCommand::RunQuickTranslate(rect))?;
    let second_result = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowInlineResult { .. })
    })
    .await?;
    match second_result {
        UiEvent::ShowInlineResult {
            ocr_text,
            translated_text,
            status_text,
            ..
        } => {
            assert_eq!(ocr_text, "新しいセリフ");
            assert_eq!(
                translated_text.as_deref(),
                Some("Lời thoại mới từ OpenAI BYOK")
            );
            assert!(
                status_text.contains("openai"),
                "expected openai in status_text, got: {status_text}"
            );
        }
        other => return Err(format!("expected ShowInlineResult, got {other:?}").into()),
    }

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::Quit)).await?;
    handle.await?;
    Ok(())
}

#[tokio::test]
async fn wayland_fullscreen_enables_inplace_on_kde_while_gnome_and_windowed_use_floating_panel()
-> TestResult {
    // Part A: KDE Wayland supports InPlace when fullscreen/maximized, but uses FloatingPanel when windowed.
    let kde_harness = M3TestHarness::new(
        DisplayServer::LinuxWayland {
            compositor: WaylandCompositor::Kde,
        },
        1920,
        1080,
    )?;
    kde_harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;
    let mut kde_cfg = AppConfig::default();
    kde_cfg.live.display_mode = LiveDisplayMode::InPlace;
    kde_harness.config_store.save(&kde_cfg)?;

    let (cmd_tx, mut ui_rx, handle) = AppController::spawn(kde_harness.services())?;

    // Fullscreen region (1920x1080 >= 90% x 85%) on KDE Wayland -> InPlace!
    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowLiveRegionPicker { .. })
    })
    .await?;
    cmd_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    }))?;
    let kde_full = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::LiveSessionStarted { .. })
    })
    .await?;
    match kde_full {
        UiEvent::LiveSessionStarted {
            display_mode,
            can_inplace,
            notice,
            ..
        } => {
            assert_eq!(display_mode, LiveDisplayMode::InPlace);
            assert!(can_inplace);
            assert_eq!(notice, None);
        }
        other => return Err(format!("expected LiveSessionStarted, got {other:?}").into()),
    }

    // SwitchLiveDisplayMode works both ways when can_inplace == true.
    cmd_tx.send(AppCommand::SwitchLiveDisplayMode)?;
    assert_eq!(
        recv_until(&mut ui_rx, |ev| matches!(
            ev,
            UiEvent::LiveDisplayModeChanged(_)
        ))
        .await?,
        UiEvent::LiveDisplayModeChanged(LiveDisplayMode::FloatingPanel)
    );
    cmd_tx.send(AppCommand::SwitchLiveDisplayMode)?;
    assert_eq!(
        recv_until(&mut ui_rx, |ev| matches!(
            ev,
            UiEvent::LiveDisplayModeChanged(_)
        ))
        .await?,
        UiEvent::LiveDisplayModeChanged(LiveDisplayMode::InPlace)
    );

    cmd_tx.send(AppCommand::StopLiveTranslate)?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::LiveSessionStopped)).await?;

    // Floating/windowed region (800x600) on KDE Wayland -> FloatingPanel with notice!
    cmd_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::ShowLiveRegionPicker { .. })
    })
    .await?;
    cmd_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 100.0,
        y: 80.0,
        width: 800.0,
        height: 600.0,
    }))?;
    let kde_windowed = recv_until(&mut ui_rx, |ev| {
        matches!(ev, UiEvent::LiveSessionStarted { .. })
    })
    .await?;
    match kde_windowed {
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
                "expected Vietnamese Wayland floating-window notice, got {notice:?}"
            );
        }
        other => return Err(format!("expected LiveSessionStarted, got {other:?}").into()),
    }

    // Switching back to InPlace is blocked with toast when floating on Wayland.
    cmd_tx.send(AppCommand::SwitchLiveDisplayMode)?;
    let toast = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::ShowToast { .. })).await?;
    assert!(matches!(
        toast,
        UiEvent::ShowToast { ref message } if message.contains("Wayland")
    ));

    cmd_tx.send(AppCommand::Quit)?;
    let _ = recv_until(&mut ui_rx, |ev| matches!(ev, UiEvent::Quit)).await?;
    handle.await?;

    // Part B: GNOME Wayland ALWAYS uses FloatingPanel with notice even when fullscreen!
    let gnome_harness = M3TestHarness::new(
        DisplayServer::LinuxWayland {
            compositor: WaylandCompositor::Gnome,
        },
        1920,
        1080,
    )?;
    gnome_harness.install_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)?;
    let mut gnome_cfg = AppConfig::default();
    gnome_cfg.live.display_mode = LiveDisplayMode::InPlace;
    gnome_harness.config_store.save(&gnome_cfg)?;

    let (gnome_tx, mut gnome_rx, gnome_handle) = AppController::spawn(gnome_harness.services())?;
    gnome_tx.send(AppCommand::ToggleLiveTranslate)?;
    let _ = recv_until(&mut gnome_rx, |ev| {
        matches!(ev, UiEvent::ShowLiveRegionPicker { .. })
    })
    .await?;
    gnome_tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    }))?;
    let gnome_full = recv_until(&mut gnome_rx, |ev| {
        matches!(ev, UiEvent::LiveSessionStarted { .. })
    })
    .await?;
    match gnome_full {
        UiEvent::LiveSessionStarted {
            display_mode,
            can_inplace,
            notice,
            ..
        } => {
            assert_eq!(display_mode, LiveDisplayMode::FloatingPanel);
            assert!(!can_inplace);
            assert!(
                notice
                    .as_deref()
                    .is_some_and(|n| n.contains("GNOME") && n.contains("Wayland")),
                "expected Vietnamese GNOME Wayland notice, got {notice:?}"
            );
        }
        other => return Err(format!("expected LiveSessionStarted, got {other:?}").into()),
    }

    gnome_tx.send(AppCommand::Quit)?;
    let _ = recv_until(&mut gnome_rx, |ev| matches!(ev, UiEvent::Quit)).await?;
    gnome_handle.await?;

    Ok(())
}
