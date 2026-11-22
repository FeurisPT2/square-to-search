//! Downscaled frame-difference signatures and debounce state machine for continuous Live Translate.

use std::time::Duration;

use crate::types::CaptureFrame;

/// Width and height of the fixed downscaled luma thumbnail grid.
const GRID_DIM: usize = 32;

/// Total number of luma samples in a [`FrameSignature`] (`32 * 32 = 1024`).
const GRID_LEN: usize = GRID_DIM * GRID_DIM;

/// Default normalized mean absolute luma difference threshold (1.5%).
pub const DEFAULT_MOTION_THRESHOLD: f32 = 0.015;

/// Default sampling interval in milliseconds (~5 Hz) for the Live Translate capture loop.
pub const DEFAULT_SAMPLE_INTERVAL_MS: u64 = 200;

/// Compact 32x32 downscaled luma thumbnail of a captured frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSignature {
    width: u32,
    height: u32,
    luma: [u8; GRID_LEN],
}

impl FrameSignature {
    /// Builds a deterministic 32x32 luma signature from a valid RGBA8 [`CaptureFrame`].
    ///
    /// Returns `None` when dimensions are zero or `frame.rgba.len()` does not match
    /// `width * height * 4`.
    #[must_use]
    pub fn from_frame(frame: &CaptureFrame) -> Option<Self> {
        if frame.width == 0 || frame.height == 0 {
            return None;
        }

        let width = frame.width as usize;
        let height = frame.height as usize;
        let expected_len = width.checked_mul(height)?.checked_mul(4)?;
        if frame.rgba.len() != expected_len {
            return None;
        }

        let mut x_coords = [0usize; GRID_DIM];
        for (gx, slot) in x_coords.iter_mut().enumerate() {
            let sampled = ((((2 * gx + 1) as u64) * u64::from(frame.width))
                / ((2 * GRID_DIM) as u64)) as usize;
            *slot = sampled.min(width - 1);
        }

        let mut luma = [0u8; GRID_LEN];
        for gy in 0..GRID_DIM {
            let py = ((((2 * gy + 1) as u64) * u64::from(frame.height)) / ((2 * GRID_DIM) as u64))
                as usize;
            let py = py.min(height - 1);
            let row_offset = py * width * 4;
            let grid_row = gy * GRID_DIM;

            for (gx, &px) in x_coords.iter().enumerate() {
                let idx = row_offset + px * 4;
                if let Some([r, g, b, _]) = frame.rgba.get(idx..idx + 4) {
                    let y = (77 * u32::from(*r) + 150 * u32::from(*g) + 29 * u32::from(*b)) >> 8;
                    luma[grid_row + gx] = y as u8;
                }
            }
        }

        Some(Self {
            width: frame.width,
            height: frame.height,
            luma,
        })
    }

    /// Returns the original physical width of the sampled frame.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Returns the original physical height of the sampled frame.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Returns the 32x32 downscaled luma grid.
    #[must_use]
    pub const fn luma_grid(&self) -> &[u8; GRID_LEN] {
        &self.luma
    }

    /// Computes normalized mean absolute luma difference in `0.0..=1.0`.
    ///
    /// Returns `1.0` when the two signatures come from frames of different dimensions.
    #[must_use]
    pub fn diff_ratio(&self, other: &Self) -> f32 {
        if self.width != other.width || self.height != other.height {
            return 1.0;
        }

        let sum: u32 = self
            .luma
            .iter()
            .zip(other.luma.iter())
            .map(|(&a, &b)| u32::from(a.abs_diff(b)))
            .sum();

        (sum as f32 / (GRID_LEN as f32 * 255.0)).clamp(0.0, 1.0)
    }
}

/// Decision emitted by [`LiveDebounceTracker::observe`] for each sampled frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebounceDecision {
    /// Screen has not meaningfully changed since the last translated frame (or tracker is paused).
    NoChange,
    /// Motion was detected or the new frame has not settled for the full debounce duration yet.
    Scrolling,
    /// Frame differs from the last translated frame and has remained stable for `>= debounce`.
    TriggerPipeline,
}

/// Stateful frame-diff + debounce tracker that suppresses OCR/translation during active scrolling.
#[derive(Debug, Clone)]
pub struct LiveDebounceTracker {
    debounce: Duration,
    motion_threshold: f32,
    last_translated: Option<FrameSignature>,
    last_observed: Option<FrameSignature>,
    stable_since: Option<Duration>,
    paused: bool,
}

impl LiveDebounceTracker {
    /// Creates a new tracker with the given settle `debounce` duration and `motion_threshold`.
    ///
    /// Non-finite thresholds fall back to [`DEFAULT_MOTION_THRESHOLD`]; finite values are clamped
    /// to `0.0..=1.0`.
    #[must_use]
    pub fn new(debounce: Duration, motion_threshold: f32) -> Self {
        let sanitized_threshold = if motion_threshold.is_finite() {
            motion_threshold.clamp(0.0, 1.0)
        } else {
            DEFAULT_MOTION_THRESHOLD
        };
        Self {
            debounce,
            motion_threshold: sanitized_threshold,
            last_translated: None,
            last_observed: None,
            stable_since: None,
            paused: false,
        }
    }

    /// Pauses or resumes pipeline triggering.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Returns `true` when the tracker is paused.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.paused
    }

    /// Clears observed and translated frame history while preserving configuration and pause state.
    pub fn reset(&mut self) {
        self.last_translated = None;
        self.last_observed = None;
        self.stable_since = None;
    }

    /// Feeds a newly sampled [`FrameSignature`] observed at monotonic timestamp `now`.
    pub fn observe(&mut self, sig: FrameSignature, now: Duration) -> DebounceDecision {
        let moved_from_observed = match &self.last_observed {
            Some(prev) => self.exceeds_motion(prev, &sig),
            None => true,
        };

        if self.paused {
            if moved_from_observed {
                self.last_observed = Some(sig);
                self.stable_since = Some(now);
            }
            return DebounceDecision::NoChange;
        }

        if moved_from_observed {
            let had_prev = self.last_observed.is_some();
            self.last_observed = Some(sig.clone());
            self.stable_since = Some(now);
            if had_prev || !self.debounce.is_zero() {
                return DebounceDecision::Scrolling;
            }
        }

        let since = match self.stable_since {
            Some(ts) => ts,
            None => {
                self.stable_since = Some(now);
                now
            }
        };

        if let Some(translated) = &self.last_translated
            && !self.exceeds_motion(translated, &sig)
        {
            return DebounceDecision::NoChange;
        }

        if now.saturating_sub(since) >= self.debounce {
            self.last_observed = Some(sig);
            DebounceDecision::TriggerPipeline
        } else {
            DebounceDecision::Scrolling
        }
    }

    /// Records `sig` as the most recently translated frame so identical frames return `NoChange`.
    pub fn mark_translated(&mut self, sig: FrameSignature) {
        self.last_translated = Some(sig);
    }

    fn exceeds_motion(&self, a: &FrameSignature, b: &FrameSignature) -> bool {
        let diff = a.diff_ratio(b);
        diff > 0.0 && diff >= self.motion_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(width: u32, height: u32, rgba: [u8; 4]) -> CaptureFrame {
        let pixels = (width as usize) * (height as usize);
        let mut buf = Vec::with_capacity(pixels * 4);
        for _ in 0..pixels {
            buf.extend_from_slice(&rgba);
        }
        CaptureFrame {
            width,
            height,
            scale_factor: 1.0,
            rgba: buf,
        }
    }

    fn striped_frame(width: u32, height: u32, stripe_offset: u32) -> CaptureFrame {
        let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height {
            let luma = if ((y + stripe_offset) / 4).is_multiple_of(2) {
                245u8
            } else {
                20u8
            };
            for _ in 0..width {
                buf.extend_from_slice(&[luma, luma, luma, 255]);
            }
        }
        CaptureFrame {
            width,
            height,
            scale_factor: 1.0,
            rgba: buf,
        }
    }

    fn require_sig(frame: &CaptureFrame) -> FrameSignature {
        match FrameSignature::from_frame(frame) {
            Some(sig) => sig,
            None => panic!(
                "expected valid FrameSignature for {}x{}",
                frame.width, frame.height
            ),
        }
    }

    #[test]
    fn default_constants_match_spec_values() {
        assert!((DEFAULT_MOTION_THRESHOLD - 0.015).abs() < f32::EPSILON);
        assert_eq!(DEFAULT_SAMPLE_INTERVAL_MS, 200);
    }

    #[test]
    fn frame_signature_rejects_empty_or_malformed_frames() {
        let zero_w = CaptureFrame {
            width: 0,
            height: 64,
            scale_factor: 1.0,
            rgba: Vec::new(),
        };
        assert!(FrameSignature::from_frame(&zero_w).is_none());

        let zero_h = CaptureFrame {
            width: 64,
            height: 0,
            scale_factor: 1.0,
            rgba: Vec::new(),
        };
        assert!(FrameSignature::from_frame(&zero_h).is_none());

        let truncated = CaptureFrame {
            width: 32,
            height: 32,
            scale_factor: 1.0,
            rgba: vec![255; 100],
        };
        assert!(FrameSignature::from_frame(&truncated).is_none());

        // 1x1 valid frame should still downsample cleanly.
        let tiny = solid_frame(1, 1, [128, 128, 128, 255]);
        let sig = require_sig(&tiny);
        assert_eq!(sig.width(), 1);
        assert_eq!(sig.height(), 1);
        assert_eq!(sig.diff_ratio(&sig), 0.0);
    }

    #[test]
    fn frame_signature_diff_is_zero_for_identical_and_high_for_changed_frames() {
        let white_64 = require_sig(&solid_frame(64, 64, [255, 255, 255, 255]));
        let white_64_b = require_sig(&solid_frame(64, 64, [255, 255, 255, 255]));
        let black_64 = require_sig(&solid_frame(64, 64, [0, 0, 0, 255]));
        let white_32 = require_sig(&solid_frame(32, 32, [255, 255, 255, 255]));

        assert_eq!(white_64.width(), 64);
        assert_eq!(white_64.height(), 64);
        assert_eq!(white_64.luma_grid().len(), 1024);
        assert_eq!(white_64.diff_ratio(&white_64_b), 0.0);
        assert_eq!(white_64.diff_ratio(&black_64), 1.0);
        assert_eq!(white_64.diff_ratio(&white_32), 1.0);
    }

    #[test]
    fn continuous_scrolling_never_triggers_until_screen_settles_for_debounce_ms() {
        // Review Focus #1:
        // Feed 10 distinct frames at t = 0, 100, 200, ..., 900 ms with debounce = 350ms.
        let mut tracker =
            LiveDebounceTracker::new(Duration::from_millis(350), DEFAULT_MOTION_THRESHOLD);

        let mut last_sig = None;
        for step in 0..10u32 {
            // Alternating distinct gray levels / scrolling stripes exceed motion threshold every 100ms.
            let gray = ((step * 27) % 256) as u8;
            let frame = if step.is_multiple_of(2) {
                solid_frame(64, 64, [gray, gray, gray, 255])
            } else {
                striped_frame(64, 64, step * 3)
            };
            let sig = require_sig(&frame);
            let decision =
                tracker.observe(sig.clone(), Duration::from_millis(u64::from(step) * 100));
            assert_ne!(
                decision,
                DebounceDecision::TriggerPipeline,
                "must not trigger while scrolling at step {step}"
            );
            last_sig = Some(sig);
        }

        let Some(settled_sig) = last_sig else {
            panic!("loop always executes 10 steps");
        };

        // t = 1100ms (200ms stable < 350ms) -> still Scrolling
        assert_eq!(
            tracker.observe(settled_sig.clone(), Duration::from_millis(1100)),
            DebounceDecision::Scrolling
        );

        // t = 1250ms (350ms stable >= 350ms) -> triggers exactly once
        assert_eq!(
            tracker.observe(settled_sig.clone(), Duration::from_millis(1250)),
            DebounceDecision::TriggerPipeline
        );

        // Mark translated; identical frame at t = 1600ms -> NoChange
        tracker.mark_translated(settled_sig.clone());
        assert_eq!(
            tracker.observe(settled_sig, Duration::from_millis(1600)),
            DebounceDecision::NoChange
        );
    }

    #[test]
    fn paused_tracker_suppresses_triggers_and_resumes_cleanly() {
        let mut tracker =
            LiveDebounceTracker::new(Duration::from_millis(350), DEFAULT_MOTION_THRESHOLD);
        let page1 = require_sig(&solid_frame(64, 64, [240, 240, 240, 255]));
        let page2 = require_sig(&solid_frame(64, 64, [30, 30, 30, 255]));

        assert_eq!(
            tracker.observe(page1.clone(), Duration::from_millis(0)),
            DebounceDecision::Scrolling
        );
        assert_eq!(
            tracker.observe(page1.clone(), Duration::from_millis(350)),
            DebounceDecision::TriggerPipeline
        );
        tracker.mark_translated(page1);

        tracker.set_paused(true);
        assert!(tracker.is_paused());

        // Distinct settled frame while paused -> NoChange
        assert_eq!(
            tracker.observe(page2.clone(), Duration::from_millis(500)),
            DebounceDecision::NoChange
        );
        assert_eq!(
            tracker.observe(page2.clone(), Duration::from_millis(1000)),
            DebounceDecision::NoChange
        );

        // Resume -> settled changed frame triggers cleanly
        tracker.set_paused(false);
        assert!(!tracker.is_paused());
        assert_eq!(
            tracker.observe(page2.clone(), Duration::from_millis(1100)),
            DebounceDecision::TriggerPipeline
        );

        // Reset clears translation history so the same frame can trigger again after debounce
        tracker.mark_translated(page2.clone());
        assert_eq!(
            tracker.observe(page2.clone(), Duration::from_millis(1200)),
            DebounceDecision::NoChange
        );
        tracker.reset();
        assert_eq!(
            tracker.observe(page2.clone(), Duration::from_millis(1300)),
            DebounceDecision::Scrolling
        );
        assert_eq!(
            tracker.observe(page2, Duration::from_millis(1650)),
            DebounceDecision::TriggerPipeline
        );
    }

    #[test]
    fn tracker_ignores_sub_threshold_flicker_and_scroll_back_to_translated_page() {
        let mut tracker = LiveDebounceTracker::new(Duration::from_millis(350), f32::NAN);
        let page1 = require_sig(&solid_frame(64, 64, [200, 200, 200, 255]));
        // 1/255 ≈ 0.0039 < DEFAULT_MOTION_THRESHOLD (0.015)
        let page1_flicker = require_sig(&solid_frame(64, 64, [201, 201, 201, 255]));
        let page2 = require_sig(&solid_frame(64, 64, [40, 40, 40, 255]));

        assert_eq!(
            tracker.observe(page1.clone(), Duration::from_millis(0)),
            DebounceDecision::Scrolling
        );
        assert_eq!(
            tracker.observe(page1.clone(), Duration::from_millis(400)),
            DebounceDecision::TriggerPipeline
        );
        tracker.mark_translated(page1.clone());

        // Minor flicker does not trigger re-OCR.
        assert_eq!(
            tracker.observe(page1_flicker, Duration::from_millis(800)),
            DebounceDecision::NoChange
        );

        // Brief peek at page 2, then scroll back to already-translated page 1.
        assert_eq!(
            tracker.observe(page2, Duration::from_millis(1000)),
            DebounceDecision::Scrolling
        );
        assert_eq!(
            tracker.observe(page1.clone(), Duration::from_millis(1100)),
            DebounceDecision::Scrolling
        );
        assert_eq!(
            tracker.observe(page1, Duration::from_millis(1500)),
            DebounceDecision::NoChange
        );
    }
}
