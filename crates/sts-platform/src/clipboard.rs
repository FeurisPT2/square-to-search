//! OS clipboard service abstraction with lazy `arboard` initialization.

use std::borrow::Cow;
use std::fmt;
use std::sync::Mutex;

use tracing::warn;

use crate::PlatformError;

/// Abstraction over OS clipboard text and RGBA image writes.
pub trait ClipboardService: Send + Sync {
    /// Writes UTF-8 text to the OS clipboard.
    fn set_text(&self, text: &str) -> Result<(), PlatformError>;

    /// Writes a tightly packed RGBA8 image (`width * height * 4` bytes) to the OS clipboard.
    fn set_image_rgba(&self, width: usize, height: usize, rgba: &[u8])
    -> Result<(), PlatformError>;
}

/// Production [`ClipboardService`] backed by `arboard`, lazily initializing the OS handle on
/// first use so construction never panics in headless test environments.
pub struct ArboardClipboard {
    inner: Mutex<Option<arboard::Clipboard>>,
}

impl ArboardClipboard {
    /// Creates a new lazily initialized clipboard service without contacting the OS clipboard yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    fn with_clipboard<F>(&self, op: F) -> Result<(), PlatformError>
    where
        F: FnOnce(&mut arboard::Clipboard) -> Result<(), arboard::Error>,
    {
        let mut guard = self.inner.lock().map_err(|_| {
            PlatformError::ClipboardFailed(String::from("clipboard mutex poisoned"))
        })?;

        if guard.is_none() {
            let clipboard = arboard::Clipboard::new().map_err(|err| {
                warn!("failed to initialize OS clipboard: {err}");
                PlatformError::ClipboardFailed(err.to_string())
            })?;
            *guard = Some(clipboard);
        }

        let Some(clipboard) = guard.as_mut() else {
            return Err(PlatformError::ClipboardFailed(String::from(
                "clipboard handle unavailable after initialization",
            )));
        };

        op(clipboard).map_err(|err| {
            warn!("clipboard write failed: {err}");
            PlatformError::ClipboardFailed(err.to_string())
        })
    }
}

impl Default for ArboardClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ArboardClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArboardClipboard").finish_non_exhaustive()
    }
}

impl ClipboardService for ArboardClipboard {
    fn set_text(&self, text: &str) -> Result<(), PlatformError> {
        self.with_clipboard(|clipboard| clipboard.set_text(text))
    }

    fn set_image_rgba(
        &self,
        width: usize,
        height: usize,
        rgba: &[u8],
    ) -> Result<(), PlatformError> {
        if width == 0 || height == 0 {
            return Err(PlatformError::ClipboardFailed(String::from(
                "clipboard image width and height must be non-zero",
            )));
        }

        let expected_len = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| {
                PlatformError::ClipboardFailed(String::from("clipboard image dimensions overflow"))
            })?;

        if rgba.len() != expected_len {
            return Err(PlatformError::ClipboardFailed(format!(
                "invalid RGBA buffer length for {width}x{height} image: expected {expected_len}, got {}",
                rgba.len()
            )));
        }

        let image = arboard::ImageData {
            width,
            height,
            bytes: Cow::Borrowed(rgba),
        };
        self.with_clipboard(|clipboard| clipboard.set_image(image))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arboard_clipboard_constructs_lazily_and_rejects_invalid_image_dimensions() {
        let clipboard = ArboardClipboard::new();
        let bad_len = clipboard.set_image_rgba(2, 2, &[0u8; 4]);
        assert!(matches!(bad_len, Err(PlatformError::ClipboardFailed(_))));

        let zero_dim = clipboard.set_image_rgba(0, 2, &[]);
        assert!(matches!(zero_dim, Err(PlatformError::ClipboardFailed(_))));
    }
}
