//! Core domain types, HiDPI geometry math, TOML configuration store, LRU translation cache,
//! live frame-diff/debounce tracking, and speech-bubble clustering for `square-to-search`.

pub mod bubbles;
pub mod cache;
pub mod config;
pub mod geometry;
pub mod live;
pub mod types;

use std::path::PathBuf;

pub use bubbles::{
    ClusterConfig, cluster_ocr_lines, cluster_ocr_lines_with_config, compute_fitted_font_size,
    contrasting_text_color, sample_bubble_background,
};
pub use cache::TranslationCache;
pub use config::{
    AppConfig, ConfigLoadOutcome, ConfigStore, HotkeyConfig, KEYRING_SERVICE_NAME,
    LiveTranslateConfig, OcrConfig, SecretSlot, TranslateConfig, VisualSearchConfig,
};
pub use geometry::{
    LogicalRect, MonitorGeometry, PhysicalRect, logical_to_physical_crop, normalize_drag_rect,
    physical_to_logical_rect,
};
pub use live::{
    DEFAULT_MOTION_THRESHOLD, DEFAULT_SAMPLE_INTERVAL_MS, DebounceDecision, FrameSignature,
    LiveDebounceTracker,
};
pub use types::{
    CaptureFrame, ExecutionDevice, LiveDisplayMode, OcrLanguagePack, OcrLine, OcrModelTier,
    ReadingDirection, TextBlock, TranslatedBlock, UiLanguage, VisualSearchEngine,
};

/// Errors produced by `sts-core` operations.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Operating system standard configuration directory could not be resolved.
    #[error("unable to determine OS configuration directory")]
    ConfigDirUnavailable,

    /// Filesystem I/O failure associated with a specific path.
    #[error("I/O error at `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failure serializing [`AppConfig`] to TOML.
    #[error("failed to serialize configuration to TOML: {0}")]
    SerializeConfig(#[from] toml::ser::Error),
}
