//! Continuous Live Translate background capture, frame-diff/debounce, OCR, speech-bubble
//! clustering, batched translation, and adaptive typography pipeline.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sts_core::bubbles::{
    cluster_ocr_lines, compute_fitted_font_size, contrasting_text_color, sample_bubble_background,
};
use sts_core::config::AppConfig;
use sts_core::geometry::{LogicalRect, MonitorGeometry, PhysicalRect};
use sts_core::live::{
    DEFAULT_MOTION_THRESHOLD, DEFAULT_SAMPLE_INTERVAL_MS, DebounceDecision, FrameSignature,
    LiveDebounceTracker,
};
use sts_core::types::{CaptureFrame, OcrLanguagePack, OcrModelTier, ReadingDirection};
use sts_ocr::engine::OcrEngine;
use sts_platform::PlatformError;
use sts_platform::capture::{ScreenCapturer, crop_frame};
use sts_translate::{TranslateError, TranslateRequest, Translator};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::state::UiEvent;

/// Default minimum logical font size (px) when shrinking long Vietnamese translations to fit
/// narrow vertical manga speech bubbles.
pub const DEFAULT_MIN_FONT_SIZE: f32 = 9.0;

/// Minimum allowed sampling interval to prevent zero-duration `tokio::time::interval` panics.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(16);

/// Target screen region or application window continuously monitored by a Live Translate session.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveTarget {
    /// Fixed sub-region on a specific monitor (Windows `WDA_EXCLUDEFROMCAPTURE` or Linux X11).
    ScreenRegion {
        monitor: MonitorGeometry,
        crop_physical: PhysicalRect,
        crop_logical: LogicalRect,
    },
    /// Specific application window captured directly by window ID (Linux X11 anti-self-capture).
    Window { window_id: u32 },
}

/// Clustered, translated, and styled speech-bubble block ready for Slint rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRenderedBlock {
    /// 1-based reading-order index (`#1`, `#2`, ...).
    pub index: usize,
    /// Bounding box in logical UI coordinates relative to the Live target origin.
    pub logical_rect: LogicalRect,
    /// Clustered original OCR text before translation.
    pub original_text: String,
    /// Translated target-language text (Vietnamese by default).
    pub translated_text: String,
    /// Adaptive backdrop RGBA sampled around the bubble perimeter.
    pub bg_rgba: [u8; 4],
    /// WCAG-contrasting text RGBA selected against `bg_rgba`.
    pub text_rgba: [u8; 4],
    /// Fitted logical font size in pixels.
    pub font_size: f32,
}

/// Runtime parameters governing a continuous Live Translate background loop.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveLoopConfig {
    /// Sampling cadence for lightweight screen capture + downscaled luma diff (~5 Hz default).
    pub sample_interval: Duration,
    /// Required screen stability duration before triggering OCR + translation.
    pub debounce: Duration,
    /// Normalized mean absolute luma diff threshold (`0.0..=1.0`) separating motion from stillness.
    pub motion_threshold: f32,
    /// OCR language pack used for Live Translate.
    pub ocr_pack: OcrLanguagePack,
    /// OCR accuracy tier (`Mobile` or `Server`).
    pub ocr_tier: OcrModelTier,
    /// Reading order for speech-bubble clustering (`Auto`, `RightToLeft`, or `LeftToRight`).
    pub reading_direction: ReadingDirection,
    /// Source language hint passed to the translator (e.g. `"auto"`, `"ja"`, `"ko"`).
    pub source_lang: String,
    /// Target language code passed to the translator (e.g. `"vi"`, `"en"`).
    pub target_lang: String,
    /// Preferred logical font size (px) before shrinking to fit narrow bubbles.
    pub base_font_size: f32,
    /// Floor logical font size (px) for very long translations in narrow bubbles.
    pub min_font_size: f32,
}

impl LiveLoopConfig {
    /// Constructs a [`LiveLoopConfig`] from the user's persisted [`AppConfig`].
    #[must_use]
    pub fn from_app_config(config: &AppConfig) -> Self {
        let base_font = if config.live.overlay_font_size.is_finite() {
            config.live.overlay_font_size.clamp(10.0, 28.0)
        } else {
            16.0
        };
        let source_lang = if config.live.source_lang.trim().is_empty() {
            config.translate.source_lang.clone()
        } else {
            config.live.source_lang.clone()
        };
        let target_lang = if config.live.target_lang.trim().is_empty() {
            config.translate.target_lang.clone()
        } else {
            config.live.target_lang.clone()
        };

        Self {
            sample_interval: Duration::from_millis(DEFAULT_SAMPLE_INTERVAL_MS),
            debounce: Duration::from_millis(config.live.debounce_ms),
            motion_threshold: DEFAULT_MOTION_THRESHOLD,
            ocr_pack: config.ocr.live_pack,
            ocr_tier: config.ocr.tier,
            reading_direction: config.live.reading_direction,
            source_lang,
            target_lang,
            base_font_size: base_font,
            min_font_size: DEFAULT_MIN_FONT_SIZE.min(base_font),
        }
    }
}

/// Control handle for an active background Live Translate session.
///
/// Dropping or calling [`Self::stop`] immediately invalidates the active generation counter,
/// aborts any in-flight OCR/translation pipeline task, and terminates the background capture loop.
#[derive(Debug)]
pub struct LiveSessionHandle {
    paused_tx: watch::Sender<bool>,
    stop_tx: watch::Sender<bool>,
    generation: Arc<AtomicU64>,
    in_flight: Arc<Mutex<Option<JoinHandle<()>>>>,
    loop_handle: Option<JoinHandle<()>>,
}

impl LiveSessionHandle {
    /// Pauses or resumes continuous capture and translation for this session.
    pub fn set_paused(&self, paused: bool) {
        let _ = self.paused_tx.send(paused);
    }

    /// Returns `true` when the session is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        *self.paused_tx.borrow()
    }

    /// Stops the background capture loop and aborts any in-flight OCR/translation task.
    pub fn stop(&mut self) {
        abort_in_flight(&self.in_flight, &self.generation);
        let _ = self.stop_tx.send(true);
        if let Some(handle) = self.loop_handle.take() {
            handle.abort();
        }
    }

    /// Returns `true` if the background capture loop task has terminated.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.loop_handle
            .as_ref()
            .is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for LiveSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sanitize_motion_threshold(threshold: f32) -> f32 {
    if threshold.is_finite() {
        threshold.clamp(0.0, 1.0)
    } else {
        DEFAULT_MOTION_THRESHOLD
    }
}

fn sanitize_scale_factor(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

fn abort_in_flight(in_flight: &Mutex<Option<JoinHandle<()>>>, generation: &AtomicU64) -> bool {
    generation.fetch_add(1, Ordering::SeqCst);
    let prev = match in_flight.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    };
    if let Some(handle) = prev {
        let was_running = !handle.is_finished();
        handle.abort();
        was_running
    } else {
        false
    }
}

fn store_in_flight(in_flight: &Mutex<Option<JoinHandle<()>>>, handle: JoinHandle<()>) {
    let prev = match in_flight.lock() {
        Ok(mut guard) => guard.replace(handle),
        Err(poisoned) => poisoned.into_inner().replace(handle),
    };
    if let Some(old) = prev {
        old.abort();
    }
}

fn clear_finished_in_flight(in_flight: &Mutex<Option<JoinHandle<()>>>) {
    let mut guard = match in_flight.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.as_ref().is_some_and(JoinHandle::is_finished) {
        *guard = None;
    }
}

fn capture_target(
    capturer: &dyn ScreenCapturer,
    target: &LiveTarget,
) -> Result<CaptureFrame, PlatformError> {
    match target {
        LiveTarget::ScreenRegion {
            monitor,
            crop_physical,
            ..
        } => {
            let full = capturer.capture_monitor(monitor.id)?;
            crop_frame(&full, *crop_physical)
        }
        LiveTarget::Window { window_id } => {
            let (_geometry, frame) = capturer.capture_window(*window_id)?;
            Ok(frame)
        }
    }
}

/// Spawns the asynchronous continuous Live Translate loop on the current Tokio runtime.
///
/// The loop polls `capturer` on `tokio::time::interval(config.sample_interval)` using
/// `tokio::time::Instant` (supporting deterministic virtual-time tests via `tokio::time::pause()`).
/// Active scrolling is suppressed via [`LiveDebounceTracker`]; rapid page flips bump a monotonic
/// [`AtomicU64`] generation guard and abort any stale in-flight OCR/translation task so older pages
/// can never overwrite newer pages.
#[must_use]
pub fn spawn_live_session(
    target: LiveTarget,
    config: LiveLoopConfig,
    capturer: Arc<dyn ScreenCapturer>,
    ocr: Arc<dyn OcrEngine>,
    translator: Arc<dyn Translator>,
    ui_tx: UnboundedSender<UiEvent>,
) -> LiveSessionHandle {
    let (paused_tx, mut paused_rx) = watch::channel(false);
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let generation = Arc::new(AtomicU64::new(0));
    let in_flight: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

    let loop_generation = Arc::clone(&generation);
    let loop_in_flight = Arc::clone(&in_flight);

    let loop_handle = tokio::spawn(async move {
        let motion_threshold = sanitize_motion_threshold(config.motion_threshold);
        let sample_interval = config.sample_interval.max(MIN_SAMPLE_INTERVAL);
        let start_instant = Instant::now();
        let mut ticker = tokio::time::interval(sample_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut tracker = LiveDebounceTracker::new(config.debounce, motion_threshold);
        let mut in_flight_sig: Option<FrameSignature> = None;
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<(u64, FrameSignature)>();

        loop {
            tokio::select! {
                biased;

                changed = stop_rx.changed() => {
                    if changed.is_err() || *stop_rx.borrow() {
                        break;
                    }
                }

                changed = paused_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let paused = *paused_rx.borrow();
                    tracker.set_paused(paused);
                    if paused {
                        let _ = abort_in_flight(&loop_in_flight, &loop_generation);
                        in_flight_sig = None;
                    }
                }

                completed = completed_rx.recv() => {
                    if let Some((done_gen, done_sig)) = completed
                        && done_gen == loop_generation.load(Ordering::SeqCst)
                    {
                        tracker.mark_translated(done_sig);
                        in_flight_sig = None;
                        clear_finished_in_flight(&loop_in_flight);
                    }
                }

                _ = ticker.tick() => {
                    if *stop_rx.borrow() {
                        break;
                    }
                    if tracker.is_paused() {
                        continue;
                    }

                    let cropped = match capture_target(capturer.as_ref(), &target) {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::debug!(error = %err, "live capture tick failed; retrying on next interval");
                            continue;
                        }
                    };

                    let Some(sig) = FrameSignature::from_frame(&cropped) else {
                        continue;
                    };

                    if let Some(active_sig) = &in_flight_sig {
                        let diff = active_sig.diff_ratio(&sig);
                        if diff == 0.0 || diff < motion_threshold {
                            // Still translating this exact settled frame; let the in-flight task finish.
                            continue;
                        }
                        // Screen moved away from the in-flight frame: cancel stale pipeline immediately.
                        let _ = abort_in_flight(&loop_in_flight, &loop_generation);
                        in_flight_sig = None;
                    }

                    let now = Instant::now().saturating_duration_since(start_instant);
                    match tracker.observe(sig.clone(), now) {
                        DebounceDecision::NoChange | DebounceDecision::Scrolling => {}
                        DebounceDecision::TriggerPipeline => {
                            let _ = abort_in_flight(&loop_in_flight, &loop_generation);
                            let my_gen = loop_generation.fetch_add(1, Ordering::SeqCst) + 1;
                            in_flight_sig = Some(sig.clone());

                            let pipeline_gen = Arc::clone(&loop_generation);
                            let pipeline_ocr = Arc::clone(&ocr);
                            let pipeline_translator = Arc::clone(&translator);
                            let pipeline_ui_tx = ui_tx.clone();
                            let pipeline_done_tx = completed_tx.clone();
                            let pipeline_cfg = config.clone();

                            let task = tokio::spawn(async move {
                                run_page_pipeline(
                                    my_gen,
                                    pipeline_gen,
                                    cropped,
                                    sig,
                                    pipeline_cfg,
                                    pipeline_ocr,
                                    pipeline_translator,
                                    pipeline_ui_tx,
                                    pipeline_done_tx,
                                )
                                .await;
                            });
                            store_in_flight(&loop_in_flight, task);
                        }
                    }
                }
            }
        }

        let _ = abort_in_flight(&loop_in_flight, &loop_generation);
    });

    LiveSessionHandle {
        paused_tx,
        stop_tx,
        generation,
        in_flight,
        loop_handle: Some(loop_handle),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_page_pipeline(
    my_gen: u64,
    current_gen: Arc<AtomicU64>,
    cropped: CaptureFrame,
    sig: FrameSignature,
    config: LiveLoopConfig,
    ocr: Arc<dyn OcrEngine>,
    translator: Arc<dyn Translator>,
    ui_tx: UnboundedSender<UiEvent>,
    completed_tx: UnboundedSender<(u64, FrameSignature)>,
) {
    if current_gen.load(Ordering::SeqCst) != my_gen {
        return;
    }

    let lines = match ocr
        .recognize(&cropped, config.ocr_pack, config.ocr_tier)
        .await
    {
        Ok(lines) => lines,
        Err(err) => {
            if current_gen.load(Ordering::SeqCst) == my_gen {
                tracing::warn!(error = %err, "live OCR pass failed");
                let _ = ui_tx.send(UiEvent::ShowToast {
                    message: format!("Lỗi OCR Live Translate: {err}"),
                });
                let _ = completed_tx.send((my_gen, sig));
            }
            return;
        }
    };

    if current_gen.load(Ordering::SeqCst) != my_gen {
        return;
    }

    let blocks = cluster_ocr_lines(&lines, config.reading_direction);
    if blocks.is_empty() {
        if current_gen.load(Ordering::SeqCst) == my_gen {
            let _ = ui_tx.send(UiEvent::LiveBlocksUpdated {
                blocks: Vec::new(),
                status_text: "Đang dịch trực tiếp · 0 bóng thoại".to_owned(),
            });
            let _ = completed_tx.send((my_gen, sig));
        }
        return;
    }

    let texts: Vec<String> = blocks.iter().map(|block| block.text.clone()).collect();
    let request = TranslateRequest::new(config.source_lang, config.target_lang, texts);

    let response = match translator.translate(&request).await {
        Ok(resp) => resp,
        Err(TranslateError::RateLimited(detail)) => {
            if current_gen.load(Ordering::SeqCst) == my_gen {
                tracing::warn!(detail = %detail, "live translator rate-limited");
                let _ = ui_tx.send(UiEvent::LiveRateLimited {
                    message: format!(
                        "Bộ dịch miễn phí đang bị giới hạn tốc độ (429): {detail}. Hãy chờ giây lát hoặc chuyển sang BYOK / Ollama trong Settings → Dịch & AI."
                    ),
                });
                let _ = completed_tx.send((my_gen, sig));
            }
            return;
        }
        Err(err) => {
            if current_gen.load(Ordering::SeqCst) == my_gen {
                tracing::warn!(error = %err, "live translation failed");
                let _ = ui_tx.send(UiEvent::ShowToast {
                    message: format!("Lỗi dịch Live Translate: {err}"),
                });
                let _ = completed_tx.send((my_gen, sig));
            }
            return;
        }
    };

    if current_gen.load(Ordering::SeqCst) != my_gen {
        return;
    }

    let scale = sanitize_scale_factor(cropped.scale_factor);
    let mut rendered = Vec::with_capacity(blocks.len());
    for (idx, block) in blocks.into_iter().enumerate() {
        let translated_text = response
            .translations
            .get(idx)
            .cloned()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| block.text.clone());

        let logical_rect = LogicalRect {
            x: block.bbox.x as f32 / scale,
            y: block.bbox.y as f32 / scale,
            width: block.bbox.width as f32 / scale,
            height: block.bbox.height as f32 / scale,
        };
        let bg_rgba = sample_bubble_background(&cropped, block.bbox);
        let text_rgba = contrasting_text_color(bg_rgba);
        let font_size = compute_fitted_font_size(
            &translated_text,
            logical_rect.width,
            logical_rect.height,
            config.base_font_size,
            config.min_font_size,
        );

        rendered.push(LiveRenderedBlock {
            index: idx + 1,
            logical_rect,
            original_text: block.text,
            translated_text,
            bg_rgba,
            text_rgba,
            font_size,
        });
    }

    if current_gen.load(Ordering::SeqCst) != my_gen {
        return;
    }

    let status_text = format!(
        "Đang dịch trực tiếp · {} bóng thoại · {}",
        rendered.len(),
        response.provider_name
    );
    let _ = ui_tx.send(UiEvent::LiveBlocksUpdated {
        blocks: rendered,
        status_text,
    });
    let _ = completed_tx.send((my_gen, sig));
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use sts_core::types::OcrLine;
    use sts_ocr::OcrError;
    use sts_translate::{TranslateResponse, Translator};

    use super::*;

    struct StaticCapturer {
        monitor: MonitorGeometry,
        frame: CaptureFrame,
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

    struct EmptyOcr;

    impl OcrEngine for EmptyOcr {
        fn recognize<'a>(
            &'a self,
            _frame: &'a CaptureFrame,
            _pack: OcrLanguagePack,
            _tier: OcrModelTier,
        ) -> sts_ocr::engine::BoxFuture<'a, Result<Vec<OcrLine>, OcrError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct CountingTranslator {
        calls: AtomicUsize,
    }

    impl Translator for CountingTranslator {
        fn provider_name(&self) -> &'static str {
            "counting"
        }

        fn translate<'a>(
            &'a self,
            request: &'a TranslateRequest,
        ) -> sts_translate::BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(TranslateResponse {
                    translations: request.texts.clone(),
                    provider_name: "counting".to_owned(),
                })
            })
        }
    }

    #[test]
    fn live_loop_config_sanitizes_non_finite_font_and_empty_languages() {
        let mut cfg = AppConfig::default();
        cfg.live.overlay_font_size = f32::NAN;
        cfg.live.source_lang = "   ".to_owned();
        cfg.live.target_lang = String::new();
        cfg.translate.source_lang = "ja".to_owned();
        cfg.translate.target_lang = "vi".to_owned();

        let live_cfg = LiveLoopConfig::from_app_config(&cfg);
        assert!((live_cfg.base_font_size - 16.0).abs() < f32::EPSILON);
        assert_eq!(live_cfg.source_lang, "ja");
        assert_eq!(live_cfg.target_lang, "vi");
        assert!(live_cfg.min_font_size <= live_cfg.base_font_size);
    }

    #[tokio::test]
    async fn textless_page_emits_empty_blocks_without_calling_translator_and_stop_finishes_task() {
        tokio::time::pause();

        let monitor = MonitorGeometry {
            id: 1,
            name: "M1".to_owned(),
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            scale_factor: 1.0,
            is_primary: true,
        };
        let frame = CaptureFrame {
            width: 64,
            height: 64,
            scale_factor: 1.0,
            rgba: vec![240; 64 * 64 * 4],
        };
        let capturer: Arc<dyn ScreenCapturer> = Arc::new(StaticCapturer {
            monitor: monitor.clone(),
            frame,
        });
        let ocr: Arc<dyn OcrEngine> = Arc::new(EmptyOcr);
        let translator = Arc::new(CountingTranslator {
            calls: AtomicUsize::new(0),
        });
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();

        let mut cfg = LiveLoopConfig::from_app_config(&AppConfig::default());
        cfg.sample_interval = Duration::ZERO; // Must safely clamp to MIN_SAMPLE_INTERVAL

        let mut handle = spawn_live_session(
            LiveTarget::ScreenRegion {
                monitor,
                crop_physical: PhysicalRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
                crop_logical: LogicalRect {
                    x: 0.0,
                    y: 0.0,
                    width: 64.0,
                    height: 64.0,
                },
            },
            cfg,
            capturer,
            ocr,
            Arc::clone(&translator) as Arc<dyn Translator>,
            ui_tx,
        );

        for _ in 0..15 {
            tokio::time::advance(Duration::from_millis(50)).await;
            tokio::task::yield_now().await;
            if let Ok(ev) = ui_rx.try_recv() {
                assert!(matches!(
                    ev,
                    UiEvent::LiveBlocksUpdated { ref blocks, .. } if blocks.is_empty()
                ));
                break;
            }
        }
        assert_eq!(translator.calls.load(Ordering::SeqCst), 0);

        handle.stop();
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }
}
