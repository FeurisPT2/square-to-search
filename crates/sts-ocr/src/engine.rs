//! Local PP-OCRv5 ONNX inference engine (`OarOcrEngine`) and idle session manager
//! (`IdleManagedOcrEngine`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use oar_ocr::core::config::{OrtExecutionProvider, OrtSessionConfig};
use oar_ocr::prelude::{OAROCR, OAROCRBuilder, TextRegion};
use sts_core::geometry::PhysicalRect;
use sts_core::types::{CaptureFrame, ExecutionDevice, OcrLanguagePack, OcrLine, OcrModelTier};
use tokio::sync::Mutex;
use tokio::time::{Instant, MissedTickBehavior};

use crate::OcrError;
use crate::manifest::{ModelStore, PackFilePaths};

/// Default idle duration (5 minutes / 300 seconds) after which loaded OCR sessions are unloaded
/// to reclaim RAM.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Aspect ratio threshold above which a detected text box is classified as vertical (e.g. manga).
const VERTICAL_ASPECT_RATIO_THRESHOLD: f64 = 1.4;

/// Boxed `Send` future returned by object-safe async traits in `sts-ocr`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Asynchronous local OCR engine interface consumed by `sts-app`.
pub trait OcrEngine: Send + Sync {
    /// Runs text detection and recognition on a captured RGBA frame using the specified
    /// language pack and accuracy tier.
    fn recognize<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    ) -> BoxFuture<'a, Result<Vec<OcrLine>, OcrError>>;
}

/// A loaded synchronous OCR session whose drop releases ONNX Runtime memory.
pub trait OcrSession: Send + Sync {
    /// Runs synchronous OCR inference on an RGB image (invoked inside `spawn_blocking`).
    fn recognize_rgb(&self, image: image::RgbImage) -> Result<Vec<OcrLine>, OcrError>;
}

/// Factory that loads an [`OcrSession`] for a `(OcrLanguagePack, OcrModelTier)` pair.
pub trait OcrSessionFactory: Send + Sync + 'static {
    /// Concrete loaded session type produced by this factory.
    type Session: OcrSession + 'static;

    /// Loads a new session synchronously (invoked inside `spawn_blocking`).
    fn load_session(
        &self,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    ) -> Result<Self::Session, OcrError>;
}

/// Loaded `oar-ocr` ONNX Runtime pipeline session.
#[derive(Debug)]
pub struct OarOcrSession {
    pipeline: OAROCR,
}

impl OcrSession for OarOcrSession {
    fn recognize_rgb(&self, image: image::RgbImage) -> Result<Vec<OcrLine>, OcrError> {
        let (image_width, image_height) = image.dimensions();
        let results = self
            .pipeline
            .predict(vec![image])
            .map_err(|err| OcrError::InferenceFailed(err.to_string()))?;

        let Some(page) = results.into_iter().next() else {
            return Ok(Vec::new());
        };

        let lines = page
            .text_regions
            .iter()
            .filter_map(|region| text_region_to_ocr_line(region, image_width, image_height))
            .collect();
        Ok(lines)
    }
}

/// Local PP-OCRv5 engine backed by `oar-ocr` / ONNX Runtime (CPU + DirectML on Windows; CPU on Linux).
#[derive(Debug, Clone)]
pub struct OarOcrEngine {
    store: ModelStore,
    device: ExecutionDevice,
}

impl OarOcrEngine {
    /// Creates an engine reading installed packs from `store` and executing on `device`.
    #[must_use]
    pub const fn new(store: ModelStore, device: ExecutionDevice) -> Self {
        Self { store, device }
    }

    /// Returns the configured model store.
    #[must_use]
    pub const fn store(&self) -> &ModelStore {
        &self.store
    }

    /// Returns the configured execution device preference.
    #[must_use]
    pub const fn device(&self) -> ExecutionDevice {
        self.device
    }

    fn build_pipeline(paths: &PackFilePaths, config: OrtSessionConfig) -> Result<OAROCR, OcrError> {
        OAROCRBuilder::new(&paths.det, &paths.rec, &paths.dict)
            .ort_session(config)
            .build()
            .map_err(|err| OcrError::InferenceFailed(err.to_string()))
    }
}

impl OcrSessionFactory for OarOcrEngine {
    type Session = OarOcrSession;

    fn load_session(
        &self,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    ) -> Result<Self::Session, OcrError> {
        if !self.store.is_installed(pack, tier) {
            return Err(OcrError::ModelNotInstalled(pack, tier));
        }

        let paths = self.store.pack_paths(pack, tier);
        let primary_cfg = build_ort_session_config(self.device);

        let pipeline = match Self::build_pipeline(&paths, primary_cfg) {
            Ok(pipeline) => pipeline,
            Err(primary_err) if self.device != ExecutionDevice::Cpu => {
                // On Windows, DirectML initialization can fail on headless/unsupported GPUs;
                // fall back gracefully to CPU execution instead of failing OCR outright.
                tracing::warn!(
                    ?pack,
                    ?tier,
                    device = ?self.device,
                    error = %primary_err,
                    "primary OCR execution provider failed; falling back to CPU"
                );
                let cpu_cfg = build_ort_session_config(ExecutionDevice::Cpu);
                Self::build_pipeline(&paths, cpu_cfg)?
            }
            Err(err) => return Err(err),
        };

        tracing::info!(?pack, ?tier, device = ?self.device, "loaded PP-OCRv5 session");
        Ok(OarOcrSession { pipeline })
    }
}

impl OcrEngine for OarOcrEngine {
    fn recognize<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    ) -> BoxFuture<'a, Result<Vec<OcrLine>, OcrError>> {
        Box::pin(async move {
            let rgb = capture_frame_to_rgb_image(frame)?;
            let engine = self.clone();

            tokio::task::spawn_blocking(move || {
                let session = engine.load_session(pack, tier)?;
                session.recognize_rgb(rgb)
            })
            .await
            .map_err(|err| OcrError::InferenceFailed(format!("OCR worker task failed: {err}")))?
        })
    }
}

struct LoadedSession<S> {
    session: Arc<S>,
    last_used: Instant,
}

type SessionMap<S> = HashMap<(OcrLanguagePack, OcrModelTier), LoadedSession<S>>;

fn evict_idle_sessions<S>(sessions: &mut SessionMap<S>, idle_timeout: Duration) -> usize {
    let now = Instant::now();
    let before = sessions.len();
    sessions.retain(|&(pack, tier), entry| {
        let keep = now.saturating_duration_since(entry.last_used) < idle_timeout;
        if !keep {
            tracing::info!(?pack, ?tier, "unloading idle OCR session to reclaim memory");
        }
        keep
    });
    before.saturating_sub(sessions.len())
}

/// Wraps an [`OcrSessionFactory`] (such as [`OarOcrEngine`]) to cache loaded sessions per
/// `(OcrLanguagePack, OcrModelTier)` and automatically unload idle sessions after `idle_timeout`.
pub struct IdleManagedOcrEngine<B: OcrSessionFactory = OarOcrEngine> {
    backend: Arc<B>,
    sessions: Arc<Mutex<SessionMap<B::Session>>>,
    idle_timeout: Duration,
}

impl<B: OcrSessionFactory> IdleManagedOcrEngine<B> {
    /// Creates an idle-managed engine with [`DEFAULT_IDLE_TIMEOUT`] (300 seconds).
    #[must_use]
    pub fn new(backend: B) -> Self {
        Self::with_idle_timeout(backend, DEFAULT_IDLE_TIMEOUT)
    }

    /// Creates an idle-managed engine with an explicit `idle_timeout`.
    #[must_use]
    pub fn with_idle_timeout(backend: B, idle_timeout: Duration) -> Self {
        Self {
            backend: Arc::new(backend),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            idle_timeout,
        }
    }

    /// Returns the configured idle unload timeout.
    #[must_use]
    pub const fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Returns a reference to the underlying session factory backend.
    #[must_use]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Returns the number of currently loaded OCR sessions kept in memory.
    pub async fn loaded_sessions_count(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Unloads all sessions whose `last_used` timestamp is older than or equal to `idle_timeout`.
    /// Returns the number of sessions unloaded.
    pub async fn sweep_idle(&self) -> usize {
        let mut guard = self.sessions.lock().await;
        evict_idle_sessions(&mut guard, self.idle_timeout)
    }

    /// Immediately unloads a specific `(pack, tier)` session if loaded.
    pub async fn unload_pack(&self, pack: OcrLanguagePack, tier: OcrModelTier) -> bool {
        self.sessions.lock().await.remove(&(pack, tier)).is_some()
    }

    /// Immediately unloads all cached sessions. Returns the number unloaded.
    pub async fn unload_all(&self) -> usize {
        let mut guard = self.sessions.lock().await;
        let count = guard.len();
        guard.clear();
        count
    }

    /// Spawns a lightweight Tokio background task that periodically invokes idle eviction.
    /// The task terminates automatically once all strong references to the session map are dropped.
    #[must_use]
    pub fn spawn_idle_reaper(&self, check_interval: Duration) -> tokio::task::JoinHandle<()> {
        let period = if check_interval.is_zero() {
            Duration::from_secs(1)
        } else {
            check_interval
        };
        let weak_sessions = Arc::downgrade(&self.sessions);
        let idle_timeout = self.idle_timeout;
        // Anchor the first tick at `now + period` synchronously so the deadline is deterministic
        // even before the spawned task receives its first poll.
        let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        tokio::spawn(async move {
            loop {
                ticker.tick().await;
                let Some(sessions) = weak_sessions.upgrade() else {
                    break;
                };
                let mut guard = sessions.lock().await;
                let _ = evict_idle_sessions(&mut guard, idle_timeout);
            }
        })
    }

    async fn get_or_load_session(
        &self,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    ) -> Result<Arc<B::Session>, OcrError> {
        // Fast path: reuse an already loaded session without holding the mutex across blocking I/O.
        {
            let mut guard = self.sessions.lock().await;
            if let Some(entry) = guard.get_mut(&(pack, tier)) {
                entry.last_used = Instant::now();
                return Ok(Arc::clone(&entry.session));
            }
        }

        // Slow path: load the ONNX session off the Tokio worker pool.
        let backend = Arc::clone(&self.backend);
        let loaded = tokio::task::spawn_blocking(move || backend.load_session(pack, tier))
            .await
            .map_err(|err| {
                OcrError::InferenceFailed(format!("OCR session load task failed: {err}"))
            })??;

        let loaded = Arc::new(loaded);
        let mut guard = self.sessions.lock().await;
        let entry = guard.entry((pack, tier)).or_insert_with(|| LoadedSession {
            session: Arc::clone(&loaded),
            last_used: Instant::now(),
        });
        entry.last_used = Instant::now();
        Ok(Arc::clone(&entry.session))
    }
}

impl<B: OcrSessionFactory> OcrEngine for IdleManagedOcrEngine<B> {
    fn recognize<'a>(
        &'a self,
        frame: &'a CaptureFrame,
        pack: OcrLanguagePack,
        tier: OcrModelTier,
    ) -> BoxFuture<'a, Result<Vec<OcrLine>, OcrError>> {
        Box::pin(async move {
            let rgb = capture_frame_to_rgb_image(frame)?;
            let session = self.get_or_load_session(pack, tier).await?;

            let lines = tokio::task::spawn_blocking(move || session.recognize_rgb(rgb))
                .await
                .map_err(|err| {
                    OcrError::InferenceFailed(format!("OCR inference task failed: {err}"))
                })??;

            // Refresh `last_used` after inference finishes so inference time does not eat into idle budget.
            let mut guard = self.sessions.lock().await;
            if let Some(entry) = guard.get_mut(&(pack, tier)) {
                entry.last_used = Instant::now();
            }

            Ok(lines)
        })
    }
}

/// Builds an [`OrtSessionConfig`] obeying Decision 3A (CPU + DirectML on Windows, CPU on Linux;
/// NEVER CUDA).
pub(crate) fn build_ort_session_config(device: ExecutionDevice) -> OrtSessionConfig {
    match device {
        ExecutionDevice::Cpu => {
            OrtSessionConfig::new().with_execution_providers(vec![OrtExecutionProvider::CPU])
        }
        ExecutionDevice::Auto => {
            #[cfg(target_os = "windows")]
            {
                // With `directml` enabled and `cuda` disabled on Windows, `OrtSessionConfig::auto()`
                // probes DirectML (device 0) and automatically falls back to CPU if unavailable.
                OrtSessionConfig::auto()
            }
            #[cfg(not(target_os = "windows"))]
            {
                OrtSessionConfig::new().with_execution_providers(vec![OrtExecutionProvider::CPU])
            }
        }
        ExecutionDevice::DirectMl => {
            #[cfg(target_os = "windows")]
            {
                // DirectML requires sequential execution and disabled memory pattern optimization
                // per ONNX Runtime DirectML EP requirements.
                OrtSessionConfig::new()
                    .with_execution_providers(vec![
                        OrtExecutionProvider::DirectML { device_id: Some(0) },
                        OrtExecutionProvider::CPU,
                    ])
                    .with_parallel_execution(false)
                    .with_memory_pattern(false)
            }
            #[cfg(not(target_os = "windows"))]
            {
                tracing::warn!("DirectML is only supported on Windows; falling back to CPU");
                OrtSessionConfig::new().with_execution_providers(vec![OrtExecutionProvider::CPU])
            }
        }
    }
}

/// Converts an RGBA8 [`CaptureFrame`] into an `image::RgbImage` suitable for `oar-ocr`.
pub(crate) fn capture_frame_to_rgb_image(
    frame: &CaptureFrame,
) -> Result<image::RgbImage, OcrError> {
    if frame.width == 0 || frame.height == 0 {
        return Err(OcrError::InferenceFailed(
            "capture frame has zero dimensions".to_owned(),
        ));
    }

    let expected = frame.expected_byte_len();
    if frame.rgba.len() != expected {
        return Err(OcrError::InferenceFailed(format!(
            "invalid RGBA buffer length: expected {expected} bytes for {}x{}, got {}",
            frame.width,
            frame.height,
            frame.rgba.len()
        )));
    }

    let pixel_count = (frame.width as usize).saturating_mul(frame.height as usize);
    let mut rgb = Vec::with_capacity(pixel_count.saturating_mul(3));
    let (chunks, _) = frame.rgba.as_chunks::<4>();
    for rgba in chunks {
        rgb.push(rgba[0]);
        rgb.push(rgba[1]);
        rgb.push(rgba[2]);
    }

    image::RgbImage::from_raw(frame.width, frame.height, rgb).ok_or_else(|| {
        OcrError::InferenceFailed("failed to construct RGB image from capture frame".to_owned())
    })
}

/// Maps a single `oar-ocr` [`TextRegion`] into an [`OcrLine`] with physical bounds clamped to the
/// input image dimensions.
pub(crate) fn text_region_to_ocr_line(
    region: &TextRegion,
    image_width: u32,
    image_height: u32,
) -> Option<OcrLine> {
    let raw_text = region.text.as_deref()?;
    let trimmed = raw_text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;

    for point in &region.bounding_box.points {
        if point.x.is_finite() && point.y.is_finite() {
            min_x = min_x.min(point.x);
            min_y = min_y.min(point.y);
            max_x = max_x.max(point.x);
            max_y = max_y.max(point.y);
        }
    }

    let bbox = if min_x.is_finite() && min_y.is_finite() && max_x.is_finite() && max_y.is_finite() {
        let max_w = f64::from(image_width);
        let max_h = f64::from(image_height);
        let x0 = f64::from(min_x).floor().clamp(0.0, max_w) as u32;
        let y0 = f64::from(min_y).floor().clamp(0.0, max_h) as u32;
        let x1 = f64::from(max_x).ceil().clamp(0.0, max_w) as u32;
        let y1 = f64::from(max_y).ceil().clamp(0.0, max_h) as u32;
        PhysicalRect {
            x: x0,
            y: y0,
            width: x1.saturating_sub(x0),
            height: y1.saturating_sub(y0),
        }
    } else {
        PhysicalRect::default()
    };

    let confidence = region
        .confidence
        .filter(|score| score.is_finite())
        .unwrap_or(0.0);
    let is_vertical =
        f64::from(bbox.height) > f64::from(bbox.width) * VERTICAL_ASPECT_RATIO_THRESHOLD;

    Some(OcrLine {
        bbox,
        text: trimmed.to_owned(),
        confidence,
        is_vertical,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use oar_ocr::core::config::OrtExecutionProvider;
    use oar_ocr::processors::{BoundingBox, Point};
    use sts_core::geometry::PhysicalRect;
    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct FakeSession {
        pack: OcrLanguagePack,
        tier: OcrModelTier,
        drop_count: Arc<AtomicUsize>,
    }

    impl Drop for FakeSession {
        fn drop(&mut self) {
            self.drop_count.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl OcrSession for FakeSession {
        fn recognize_rgb(&self, image: image::RgbImage) -> Result<Vec<OcrLine>, OcrError> {
            Ok(vec![OcrLine {
                bbox: PhysicalRect {
                    x: 0,
                    y: 0,
                    width: image.width(),
                    height: image.height(),
                },
                text: format!("{:?}-{:?}", self.pack, self.tier),
                confidence: 0.99,
                is_vertical: false,
            }])
        }
    }

    struct FakeSessionFactory {
        load_count: Arc<AtomicUsize>,
        drop_count: Arc<AtomicUsize>,
    }

    impl FakeSessionFactory {
        fn new() -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
            let load_count = Arc::new(AtomicUsize::new(0));
            let drop_count = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    load_count: Arc::clone(&load_count),
                    drop_count: Arc::clone(&drop_count),
                },
                load_count,
                drop_count,
            )
        }
    }

    impl OcrSessionFactory for FakeSessionFactory {
        type Session = FakeSession;

        fn load_session(
            &self,
            pack: OcrLanguagePack,
            tier: OcrModelTier,
        ) -> Result<Self::Session, OcrError> {
            self.load_count.fetch_add(1, Ordering::SeqCst);
            Ok(FakeSession {
                pack,
                tier,
                drop_count: Arc::clone(&self.drop_count),
            })
        }
    }

    fn sample_frame() -> CaptureFrame {
        CaptureFrame {
            width: 4,
            height: 4,
            scale_factor: 1.0,
            rgba: vec![255; 4 * 4 * 4],
        }
    }

    #[tokio::test]
    async fn idle_managed_engine_unloads_session_after_idle_timeout() -> TestResult {
        tokio::time::pause();

        let (factory, load_count, drop_count) = FakeSessionFactory::new();
        let engine = IdleManagedOcrEngine::new(factory);
        assert_eq!(engine.idle_timeout(), Duration::from_secs(300));

        let frame = sample_frame();

        // First recognize loads session.
        let lines = engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await?;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "CjkEn-Mobile");
        assert_eq!(engine.loaded_sessions_count().await, 1);
        assert_eq!(load_count.load(Ordering::SeqCst), 1);

        // Advance 200s (< 300s) -> session stays loaded.
        tokio::time::advance(Duration::from_secs(200)).await;
        assert_eq!(engine.sweep_idle().await, 0);
        assert_eq!(engine.loaded_sessions_count().await, 1);

        // Re-use session -> refreshes last_used without reloading.
        let _ = engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await?;
        assert_eq!(load_count.load(Ordering::SeqCst), 1);

        // Advance 250s from last use -> still within 300s idle window.
        tokio::time::advance(Duration::from_secs(250)).await;
        assert_eq!(engine.sweep_idle().await, 0);
        assert_eq!(engine.loaded_sessions_count().await, 1);

        // Advance another 51s (301s total since last use) -> unloaded!
        tokio::time::advance(Duration::from_secs(51)).await;
        assert_eq!(engine.sweep_idle().await, 1);
        assert_eq!(engine.loaded_sessions_count().await, 0);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);

        // Subsequent recognize loads a fresh session.
        let _ = engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await?;
        assert_eq!(load_count.load(Ordering::SeqCst), 2);
        assert_eq!(engine.loaded_sessions_count().await, 1);
        Ok(())
    }

    #[tokio::test]
    async fn idle_managed_engine_tracks_packs_independently() -> TestResult {
        tokio::time::pause();

        let (factory, _, drop_count) = FakeSessionFactory::new();
        let engine = IdleManagedOcrEngine::new(factory);
        let frame = sample_frame();

        engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await?;
        tokio::time::advance(Duration::from_secs(200)).await;
        engine
            .recognize(&frame, OcrLanguagePack::Korean, OcrModelTier::Mobile)
            .await?;
        assert_eq!(engine.loaded_sessions_count().await, 2);

        // Advance 150s -> CjkEn idle 350s (evicted), Korean idle 150s (kept).
        tokio::time::advance(Duration::from_secs(150)).await;
        assert_eq!(engine.sweep_idle().await, 1);
        assert_eq!(engine.loaded_sessions_count().await, 1);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test]
    async fn idle_managed_engine_background_reaper_unloads_automatically() -> TestResult {
        tokio::time::pause();

        let (factory, _, drop_count) = FakeSessionFactory::new();
        let engine = IdleManagedOcrEngine::new(factory);
        let reaper = engine.spawn_idle_reaper(Duration::from_secs(60));
        let frame = sample_frame();

        engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await?;
        assert_eq!(engine.loaded_sessions_count().await, 1);

        // Advance past 300s + reaper interval.
        tokio::time::advance(Duration::from_secs(365)).await;
        tokio::task::yield_now().await;

        assert_eq!(engine.loaded_sessions_count().await, 0);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);

        drop(engine);
        tokio::time::advance(Duration::from_secs(65)).await;
        reaper.await?;
        Ok(())
    }

    #[tokio::test]
    async fn idle_managed_engine_explicit_unload_and_dyn_dispatch_work() -> TestResult {
        let (factory, _, drop_count) = FakeSessionFactory::new();
        let managed = Arc::new(IdleManagedOcrEngine::new(factory));
        let dyn_engine: Arc<dyn OcrEngine> = managed.clone();
        let frame = sample_frame();

        let lines = dyn_engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await?;
        assert_eq!(lines[0].text, "CjkEn-Mobile");
        let _ = dyn_engine
            .recognize(&frame, OcrLanguagePack::Korean, OcrModelTier::Server)
            .await?;
        assert_eq!(managed.loaded_sessions_count().await, 2);

        assert!(
            managed
                .unload_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
                .await
        );
        assert!(
            !managed
                .unload_pack(OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
                .await
        );
        assert_eq!(managed.loaded_sessions_count().await, 1);
        assert_eq!(drop_count.load(Ordering::SeqCst), 1);

        assert_eq!(managed.unload_all().await, 1);
        assert_eq!(managed.loaded_sessions_count().await, 0);
        assert_eq!(drop_count.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn oar_ocr_engine_returns_model_not_installed_when_pack_missing() -> TestResult {
        let tmp = TempDir::new()?;
        let store = ModelStore::new(tmp.path().to_path_buf());
        let engine = OarOcrEngine::new(store, ExecutionDevice::Cpu);
        let frame = sample_frame();

        let res = engine
            .recognize(&frame, OcrLanguagePack::CjkEn, OcrModelTier::Mobile)
            .await;
        assert!(matches!(
            res,
            Err(OcrError::ModelNotInstalled(
                OcrLanguagePack::CjkEn,
                OcrModelTier::Mobile
            ))
        ));
        Ok(())
    }

    #[test]
    fn capture_frame_to_rgb_converts_pixels_and_rejects_invalid_buffers() -> TestResult {
        let frame = CaptureFrame {
            width: 2,
            height: 1,
            scale_factor: 1.0,
            rgba: vec![10, 20, 30, 255, 40, 50, 60, 128],
        };
        let rgb = capture_frame_to_rgb_image(&frame)?;
        assert_eq!(rgb.width(), 2);
        assert_eq!(rgb.height(), 1);
        assert_eq!(rgb.get_pixel(0, 0).0, [10, 20, 30]);
        assert_eq!(rgb.get_pixel(1, 0).0, [40, 50, 60]);

        let zero_frame = CaptureFrame {
            width: 0,
            height: 4,
            scale_factor: 1.0,
            rgba: Vec::new(),
        };
        assert!(matches!(
            capture_frame_to_rgb_image(&zero_frame),
            Err(OcrError::InferenceFailed(_))
        ));

        let truncated = CaptureFrame {
            width: 2,
            height: 2,
            scale_factor: 1.0,
            rgba: vec![0; 12],
        };
        assert!(matches!(
            capture_frame_to_rgb_image(&truncated),
            Err(OcrError::InferenceFailed(_))
        ));
        Ok(())
    }

    #[test]
    fn text_region_to_ocr_line_clamps_bounds_and_detects_vertical_text() {
        let horiz_region = TextRegion::with_recognition(
            BoundingBox::new(vec![
                Point::new(-5.0, 10.0),
                Point::new(105.0, 10.0),
                Point::new(105.0, 30.0),
                Point::new(-5.0, 30.0),
            ]),
            Some(Arc::from("Hello world")),
            Some(0.95),
        );
        let horiz = text_region_to_ocr_line(&horiz_region, 100, 100);
        assert_eq!(
            horiz,
            Some(OcrLine {
                bbox: PhysicalRect {
                    x: 0,
                    y: 10,
                    width: 100,
                    height: 20,
                },
                text: "Hello world".to_owned(),
                confidence: 0.95,
                is_vertical: false,
            })
        );

        let vert_region = TextRegion::with_recognition(
            BoundingBox::from_coords(10.0, 10.0, 30.0, 60.0),
            Some(Arc::from("縦書き")),
            Some(0.88),
        );
        let vert = text_region_to_ocr_line(&vert_region, 100, 100);
        assert_eq!(
            vert,
            Some(OcrLine {
                bbox: PhysicalRect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 50,
                },
                text: "縦書き".to_owned(),
                confidence: 0.88,
                is_vertical: true,
            })
        );

        // Threshold boundary: height == width * 1.4 is NOT strictly greater.
        let boundary = TextRegion::with_recognition(
            BoundingBox::from_coords(0.0, 0.0, 10.0, 14.0),
            Some(Arc::from("X")),
            Some(0.9),
        );
        assert!(text_region_to_ocr_line(&boundary, 100, 100).is_some_and(|l| !l.is_vertical));

        // Blank text regions are skipped.
        let empty = TextRegion::with_recognition(
            BoundingBox::from_coords(0.0, 0.0, 10.0, 10.0),
            Some(Arc::from("   ")),
            Some(0.5),
        );
        assert_eq!(text_region_to_ocr_line(&empty, 100, 100), None);
    }

    #[test]
    fn ort_session_config_never_selects_cuda_and_configures_platform_providers() {
        for device in [
            ExecutionDevice::Auto,
            ExecutionDevice::Cpu,
            ExecutionDevice::DirectMl,
        ] {
            let cfg = build_ort_session_config(device);
            let providers = cfg.get_execution_providers();
            assert!(
                providers
                    .iter()
                    .all(|p| !matches!(p, OrtExecutionProvider::CUDA { .. })),
                "CUDA must never be enabled in v1 (Decision 3A)"
            );
            assert!(
                providers.contains(&OrtExecutionProvider::CPU),
                "CPU fallback must always be present"
            );
        }

        let cpu_cfg = build_ort_session_config(ExecutionDevice::Cpu);
        assert_eq!(
            cpu_cfg.get_execution_providers(),
            vec![OrtExecutionProvider::CPU]
        );

        #[cfg(target_os = "windows")]
        {
            let dml_cfg = build_ort_session_config(ExecutionDevice::DirectMl);
            assert_eq!(
                dml_cfg.get_execution_providers(),
                vec![
                    OrtExecutionProvider::DirectML { device_id: Some(0) },
                    OrtExecutionProvider::CPU,
                ]
            );
            assert_eq!(dml_cfg.enable_mem_pattern, Some(false));
            assert_eq!(dml_cfg.parallel_execution, Some(false));

            let auto_cfg = build_ort_session_config(ExecutionDevice::Auto);
            assert_eq!(
                auto_cfg.get_execution_providers(),
                vec![
                    OrtExecutionProvider::DirectML { device_id: Some(0) },
                    OrtExecutionProvider::CPU,
                ]
            );
        }

        #[cfg(not(target_os = "windows"))]
        {
            let auto_cfg = build_ort_session_config(ExecutionDevice::Auto);
            assert_eq!(
                auto_cfg.get_execution_providers(),
                vec![OrtExecutionProvider::CPU]
            );
            let dml_cfg = build_ort_session_config(ExecutionDevice::DirectMl);
            assert_eq!(
                dml_cfg.get_execution_providers(),
                vec![OrtExecutionProvider::CPU]
            );
        }
    }
}
