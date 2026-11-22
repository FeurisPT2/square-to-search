//! Shared domain data structures and configuration enums across the workspace.

use serde::{Deserialize, Serialize};

use crate::geometry::PhysicalRect;

/// Captured RGBA8 framebuffer along with its physical dimensions and display scale factor.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureFrame {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub rgba: Vec<u8>,
}

impl CaptureFrame {
    /// Expected RGBA8 byte length (`width * height * 4`).
    #[must_use]
    pub const fn expected_byte_len(&self) -> usize {
        (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(4)
    }
}

/// Single line/column detected by local OCR in physical framebuffer coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OcrLine {
    pub bbox: PhysicalRect,
    pub text: String,
    pub confidence: f32,
    pub is_vertical: bool,
}

/// Clustered text block (e.g., a manga speech bubble) ready for translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextBlock {
    pub bbox: PhysicalRect,
    pub text: String,
    pub is_vertical: bool,
}

/// Translated text block with adaptive background color for in-place overlay rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslatedBlock {
    pub bbox: PhysicalRect,
    pub original_text: String,
    pub translated_text: String,
    pub bg_rgba: [u8; 4],
}

/// PP-OCRv5 language pack selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrLanguagePack {
    /// Unified Chinese, Japanese, and English pack (default).
    #[default]
    CjkEn,
    /// Dedicated Korean pack for manhwa.
    Korean,
    /// Latin / Vietnamese pack with accurate diacritics.
    VietnameseLatin,
}

/// Accuracy vs. speed tier for PP-OCRv5 ONNX models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrModelTier {
    /// Lightweight mobile models (~16-25 MB), default for fast CPU inference.
    #[default]
    Mobile,
    /// Larger server models (~85 MB+) for stylized comic fonts.
    Server,
}

/// OCR inference execution device (Decision 3A: CPU and DirectML only in v1 — no CUDA).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionDevice {
    /// Prefer DirectML on Windows with CPU fallback; CPU on Linux.
    #[default]
    Auto,
    /// Force CPU execution.
    Cpu,
    /// Request DirectML acceleration on Windows.
    DirectMl,
}

/// Display mode for Live Translate output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveDisplayMode {
    /// Render translations directly over detected speech bubbles.
    #[default]
    InPlace,
    /// Render translations in a separate movable floating panel.
    FloatingPanel,
}

/// Comic reading order for bubble sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingDirection {
    /// Infer reading direction from vertical/horizontal text layout.
    #[default]
    Auto,
    /// Right-to-left (Japanese manga).
    RightToLeft,
    /// Left-to-right (manhwa / western comics).
    LeftToRight,
}

/// Visual reverse image search engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisualSearchEngine {
    #[default]
    GoogleLens,
    Yandex,
    Bing,
}

/// Application user interface language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiLanguage {
    #[default]
    Vietnamese,
    English,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
    struct DeviceWrapper {
        device: ExecutionDevice,
    }

    #[test]
    fn execution_device_supports_auto_cpu_directml_and_rejects_cuda() {
        for (raw, expected) in [
            ("auto", ExecutionDevice::Auto),
            ("cpu", ExecutionDevice::Cpu),
            ("direct_ml", ExecutionDevice::DirectMl),
        ] {
            let parsed: Result<DeviceWrapper, _> = toml::from_str(&format!("device = \"{raw}\""));
            assert_eq!(parsed.ok(), Some(DeviceWrapper { device: expected }));
        }

        let cuda_result: Result<DeviceWrapper, _> = toml::from_str("device = \"cuda\"");
        assert!(cuda_result.is_err());
    }
}
