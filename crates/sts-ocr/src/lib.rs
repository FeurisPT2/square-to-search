//! Local PP-OCRv5 ONNX engine, pinned model manifest, SHA-256 atomic downloader,
//! offline model import, and idle-unloading session manager.

pub mod downloader;
pub mod engine;
pub mod manifest;

use std::path::PathBuf;

use sts_core::types::{OcrLanguagePack, OcrModelTier};

pub use downloader::{DownloadProgress, ModelDownloader};
pub use engine::{
    DEFAULT_IDLE_TIMEOUT, IdleManagedOcrEngine, OarOcrEngine, OarOcrSession, OcrEngine, OcrSession,
    OcrSessionFactory,
};
pub use manifest::{
    DET_MODEL_FILENAME, DICT_FILENAME, ModelArtifact, ModelManifest, ModelPackSpec, ModelStore,
    PackFilePaths, PackInstallStatus, REC_MODEL_FILENAME, REQUIRED_PACK_FILES,
};

/// Errors produced by `sts-ocr` model management, downloading, and local ONNX inference.
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// Requested PP-OCRv5 language pack and tier are not installed in the local model store.
    #[error("OCR model pack {0:?} ({1:?}) is not installed")]
    ModelNotInstalled(OcrLanguagePack, OcrModelTier),

    /// Downloaded artifact SHA-256 digest did not match the expected manifest checksum.
    #[error("checksum mismatch for `{artifact}`: expected {expected}, got {actual}")]
    ChecksumMismatch {
        artifact: String,
        expected: String,
        actual: String,
    },

    /// Network, HTTP, or transport error while downloading a model artifact.
    #[error("failed to download `{artifact}`: {message}")]
    DownloadFailed { artifact: String, message: String },

    /// User-supplied offline model directory is missing required files or is invalid.
    #[error("invalid offline model directory: {0}")]
    InvalidOfflineDir(String),

    /// ONNX Runtime session initialization or inference error.
    #[error("OCR inference failed: {0}")]
    InferenceFailed(String),

    /// Filesystem I/O failure with path context.
    #[error("I/O error at `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
