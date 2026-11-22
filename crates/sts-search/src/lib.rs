//! Visual reverse image search launcher for `square-to-search`.
//!
//! Encodes cropped RGBA screenshots to PNG in memory, uploads directly to Google Lens, Yandex,
//! or Bing without third-party image hosts, opens the resulting search URL in the default OS
//! browser, and cleanly falls back to copying the cropped image to the clipboard + opening the
//! engine's landing page when direct upload fails.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use reqwest::header::LOCATION;
use reqwest::multipart::{Form, Part};
use reqwest::redirect::Policy;
use serde_json::Value;
use sts_core::config::VisualSearchConfig;
use sts_core::types::{CaptureFrame, VisualSearchEngine};
use sts_platform::PlatformError;
use sts_platform::clipboard::ClipboardService;
use url::Url;

/// Default HTTP timeout applied to direct visual search uploads (15 seconds).
pub const DEFAULT_UPLOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Default base URL for Google Lens direct uploads.
pub const DEFAULT_GOOGLE_LENS_BASE_URL: &str = "https://lens.google.com";
/// Default browser landing URL opened when Google Lens falls back to clipboard paste.
pub const DEFAULT_GOOGLE_LENS_FALLBACK_URL: &str = "https://lens.google.com";

/// Default base URL for Yandex reverse image search uploads.
pub const DEFAULT_YANDEX_BASE_URL: &str = "https://yandex.com";
/// Default browser landing URL opened when Yandex falls back to clipboard paste.
pub const DEFAULT_YANDEX_FALLBACK_URL: &str = "https://yandex.com/images/";

/// Default base URL for Bing Visual Search uploads.
pub const DEFAULT_BING_BASE_URL: &str = "https://www.bing.com";
/// Default browser landing URL opened when Bing falls back to clipboard paste.
pub const DEFAULT_BING_FALLBACK_URL: &str = "https://www.bing.com/visualsearch";

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Errors produced by visual reverse image search operations.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// Input capture frame had zero dimensions or mismatched RGBA byte length.
    #[error("invalid capture frame: {0}")]
    InvalidFrame(String),

    /// Failed to encode the RGBA capture frame into PNG bytes.
    #[error("failed to encode capture frame as PNG: {0}")]
    EncodeFailed(String),

    /// Failed to initialize the HTTP client used for visual search uploads.
    #[error("failed to build visual search HTTP client: {0}")]
    HttpClientInit(String),

    /// Direct image upload failed and clipboard fallback was disabled.
    #[error("visual search upload failed: {0}")]
    UploadFailed(String),

    /// Writing the cropped image to the OS clipboard during fallback failed.
    #[error("clipboard fallback failed: {0}")]
    ClipboardFailed(#[from] PlatformError),

    /// Opening a result or fallback URL in the OS browser failed.
    #[error("failed to open URL `{url}` in browser: {reason}")]
    BrowserOpenFailed {
        /// Target URL that failed to open.
        url: String,
        /// Underlying failure details.
        reason: String,
    },
}

/// Abstraction over launching a URL in the user's default web browser.
pub trait BrowserOpener: Send + Sync {
    /// Opens `url` in the system browser.
    fn open_url(&self, url: &str) -> Result<(), SearchError>;
}

/// Production [`BrowserOpener`] backed by [`open::that_detached`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemBrowserOpener;

impl SystemBrowserOpener {
    /// Creates a new [`SystemBrowserOpener`].
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl BrowserOpener for SystemBrowserOpener {
    fn open_url(&self, url: &str) -> Result<(), SearchError> {
        let validated =
            validate_http_url(url).map_err(|reason| SearchError::BrowserOpenFailed {
                url: url.to_owned(),
                reason,
            })?;
        open::that_detached(validated.as_str()).map_err(|err| SearchError::BrowserOpenFailed {
            url: url.to_owned(),
            reason: err.to_string(),
        })
    }
}

/// Outcome of a completed visual search request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualSearchOutcome {
    /// Direct upload succeeded and the result URL was opened in the browser.
    OpenedResultUrl {
        /// Visual search engine used.
        engine: VisualSearchEngine,
        /// Final result URL opened in the browser.
        url: String,
    },
    /// Direct upload failed, so the image crop was copied to the clipboard and the engine's
    /// landing page was opened in the browser for manual paste (`Ctrl+V`).
    FallbackClipboard {
        /// Visual search engine attempted.
        engine: VisualSearchEngine,
        /// Landing page URL opened in the browser.
        fallback_url: String,
        /// Reason why direct upload fell back.
        reason: String,
    },
}

/// Configurable upload base URLs and browser fallback landing pages for all supported engines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualSearchEndpoints {
    /// Base URL for Google Lens uploads (default: `https://lens.google.com`).
    pub google_lens_base_url: String,
    /// Browser fallback landing URL for Google Lens (default: `https://lens.google.com`).
    pub google_lens_fallback_url: String,
    /// Base URL for Yandex uploads (default: `https://yandex.com`).
    pub yandex_base_url: String,
    /// Browser fallback landing URL for Yandex (default: `https://yandex.com/images/`).
    pub yandex_fallback_url: String,
    /// Base URL for Bing Visual Search uploads (default: `https://www.bing.com`).
    pub bing_base_url: String,
    /// Browser fallback landing URL for Bing (default: `https://www.bing.com/visualsearch`).
    pub bing_fallback_url: String,
}

impl Default for VisualSearchEndpoints {
    fn default() -> Self {
        Self {
            google_lens_base_url: DEFAULT_GOOGLE_LENS_BASE_URL.to_owned(),
            google_lens_fallback_url: DEFAULT_GOOGLE_LENS_FALLBACK_URL.to_owned(),
            yandex_base_url: DEFAULT_YANDEX_BASE_URL.to_owned(),
            yandex_fallback_url: DEFAULT_YANDEX_FALLBACK_URL.to_owned(),
            bing_base_url: DEFAULT_BING_BASE_URL.to_owned(),
            bing_fallback_url: DEFAULT_BING_FALLBACK_URL.to_owned(),
        }
    }
}

impl VisualSearchEndpoints {
    /// Overrides the Google Lens upload base URL while preserving the canonical fallback page.
    #[must_use]
    pub fn with_google_lens_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.google_lens_base_url = base_url.into();
        self
    }

    /// Overrides the Yandex upload base URL while preserving the canonical fallback page.
    #[must_use]
    pub fn with_yandex_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.yandex_base_url = base_url.into();
        self
    }

    /// Overrides the Bing Visual Search upload base URL while preserving the canonical fallback page.
    #[must_use]
    pub fn with_bing_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.bing_base_url = base_url.into();
        self
    }

    /// Returns the configured upload base URL for `engine`.
    #[must_use]
    pub fn base_url(&self, engine: VisualSearchEngine) -> &str {
        match engine {
            VisualSearchEngine::GoogleLens => &self.google_lens_base_url,
            VisualSearchEngine::Yandex => &self.yandex_base_url,
            VisualSearchEngine::Bing => &self.bing_base_url,
        }
    }

    /// Returns the configured browser fallback landing URL for `engine`.
    #[must_use]
    pub fn fallback_url(&self, engine: VisualSearchEngine) -> &str {
        match engine {
            VisualSearchEngine::GoogleLens => &self.google_lens_fallback_url,
            VisualSearchEngine::Yandex => &self.yandex_fallback_url,
            VisualSearchEngine::Bing => &self.bing_fallback_url,
        }
    }
}

/// Encodes an RGBA8 [`CaptureFrame`] into an in-memory PNG byte buffer.
///
/// # Errors
///
/// Returns [`SearchError::InvalidFrame`] when frame dimensions are zero or do not match the
/// RGBA buffer length, or [`SearchError::EncodeFailed`] if PNG encoding fails.
pub fn encode_frame_to_png(frame: &CaptureFrame) -> Result<Vec<u8>, SearchError> {
    validate_frame(frame)?;
    encode_validated_rgba_to_png(frame.width, frame.height, &frame.rgba)
}

/// Service that executes direct reverse image search uploads with clipboard fallback.
#[derive(Clone)]
pub struct VisualSearchService {
    browser: Arc<dyn BrowserOpener>,
    clipboard: Arc<dyn ClipboardService>,
    endpoints: VisualSearchEndpoints,
    client: reqwest::Client,
    timeout: Option<Duration>,
}

impl fmt::Debug for VisualSearchService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VisualSearchService")
            .field("endpoints", &self.endpoints)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl VisualSearchService {
    /// Creates a new [`VisualSearchService`] with default production endpoints and a 15-second
    /// non-redirecting HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::HttpClientInit`] if the underlying HTTP client fails to initialize.
    pub fn new(
        browser: Arc<dyn BrowserOpener>,
        clipboard: Arc<dyn ClipboardService>,
    ) -> Result<Self, SearchError> {
        Self::with_endpoints(browser, clipboard, VisualSearchEndpoints::default())
    }

    /// Creates a new [`VisualSearchService`] with custom [`VisualSearchEndpoints`] and the
    /// default 15-second timeout.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::HttpClientInit`] if the underlying HTTP client fails to initialize.
    pub fn with_endpoints(
        browser: Arc<dyn BrowserOpener>,
        clipboard: Arc<dyn ClipboardService>,
        endpoints: VisualSearchEndpoints,
    ) -> Result<Self, SearchError> {
        Self::with_endpoints_and_timeout(browser, clipboard, endpoints, DEFAULT_UPLOAD_TIMEOUT)
    }

    /// Creates a new [`VisualSearchService`] with custom endpoints and upload timeout.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::HttpClientInit`] if the underlying HTTP client fails to initialize.
    pub fn with_endpoints_and_timeout(
        browser: Arc<dyn BrowserOpener>,
        clipboard: Arc<dyn ClipboardService>,
        endpoints: VisualSearchEndpoints,
        timeout: Duration,
    ) -> Result<Self, SearchError> {
        let client = build_http_client(timeout)?;
        Ok(Self {
            browser,
            clipboard,
            endpoints,
            client,
            timeout: Some(timeout),
        })
    }

    /// Creates a new [`VisualSearchService`] with a caller-supplied [`reqwest::Client`].
    ///
    /// Callers supplying a custom client should configure [`reqwest::redirect::Policy::none()`]
    /// so redirect `Location` headers are captured directly without downloading full result HTML.
    #[must_use]
    pub fn with_client(
        browser: Arc<dyn BrowserOpener>,
        clipboard: Arc<dyn ClipboardService>,
        endpoints: VisualSearchEndpoints,
        client: reqwest::Client,
    ) -> Self {
        Self {
            browser,
            clipboard,
            endpoints,
            client,
            timeout: None,
        }
    }

    /// Returns the configured [`VisualSearchEndpoints`].
    #[must_use]
    pub const fn endpoints(&self) -> &VisualSearchEndpoints {
        &self.endpoints
    }

    /// Returns the underlying [`BrowserOpener`] handle.
    #[must_use]
    pub const fn browser(&self) -> &Arc<dyn BrowserOpener> {
        &self.browser
    }

    /// Runs visual reverse image search for `frame` using `config`.
    ///
    /// # Errors
    ///
    /// Propagates [`SearchError`] on invalid frames, PNG encoding failure, clipboard/browser
    /// errors, or upload failures when `config.enable_clipboard_fallback` is `false`.
    pub async fn search_with_config(
        &self,
        frame: &CaptureFrame,
        config: &VisualSearchConfig,
    ) -> Result<VisualSearchOutcome, SearchError> {
        self.search(frame, config.engine, config.enable_clipboard_fallback)
            .await
    }

    /// Runs visual reverse image search for `frame` on `engine`.
    ///
    /// On upload failure (HTTP error, timeout, missing redirect, or unparseable response):
    /// - When `enable_clipboard_fallback` is `true`, copies `frame.rgba` to the OS clipboard,
    ///   opens the engine's landing page, and returns [`VisualSearchOutcome::FallbackClipboard`].
    /// - When `enable_clipboard_fallback` is `false`, returns [`SearchError::UploadFailed`].
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::InvalidFrame`], [`SearchError::EncodeFailed`],
    /// [`SearchError::UploadFailed`], [`SearchError::ClipboardFailed`], or
    /// [`SearchError::BrowserOpenFailed`].
    pub async fn search(
        &self,
        frame: &CaptureFrame,
        engine: VisualSearchEngine,
        enable_clipboard_fallback: bool,
    ) -> Result<VisualSearchOutcome, SearchError> {
        let (width, height) = validate_frame(frame)?;
        let png_bytes = encode_validated_rgba_to_png(frame.width, frame.height, &frame.rgba)?;

        let upload_fut = self.upload_image(engine, frame.width, frame.height, png_bytes);
        let upload_result = match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, upload_fut).await {
                Ok(res) => res,
                Err(_) => Err(format!(
                    "{engine:?} upload timed out after {}ms",
                    limit.as_millis()
                )),
            },
            None => upload_fut.await,
        };

        match upload_result {
            Ok(url) => {
                tracing::debug!(?engine, %url, "opening visual search result URL");
                self.browser.open_url(&url)?;
                Ok(VisualSearchOutcome::OpenedResultUrl { engine, url })
            }
            Err(reason) => {
                if !enable_clipboard_fallback {
                    tracing::warn!(
                        ?engine,
                        %reason,
                        "visual search upload failed and clipboard fallback is disabled"
                    );
                    return Err(SearchError::UploadFailed(reason));
                }

                tracing::warn!(
                    ?engine,
                    %reason,
                    "visual search upload failed; falling back to clipboard and landing page"
                );
                self.clipboard.set_image_rgba(width, height, &frame.rgba)?;
                let fallback_url = self.endpoints.fallback_url(engine).to_owned();
                self.browser.open_url(&fallback_url)?;
                Ok(VisualSearchOutcome::FallbackClipboard {
                    engine,
                    fallback_url,
                    reason,
                })
            }
        }
    }

    async fn upload_image(
        &self,
        engine: VisualSearchEngine,
        width: u32,
        height: u32,
        png_bytes: Vec<u8>,
    ) -> Result<String, String> {
        match engine {
            VisualSearchEngine::GoogleLens => {
                self.upload_google_lens(width, height, png_bytes).await
            }
            VisualSearchEngine::Yandex => self.upload_yandex(png_bytes).await,
            VisualSearchEngine::Bing => self.upload_bing(png_bytes).await,
        }
    }

    async fn upload_google_lens(
        &self,
        width: u32,
        height: u32,
        png_bytes: Vec<u8>,
    ) -> Result<String, String> {
        let base_url = self.endpoints.google_lens_base_url.trim_end_matches('/');
        let mut endpoint = Url::parse(&format!("{base_url}/v3/upload"))
            .map_err(|err| format!("invalid Google Lens endpoint URL: {err}"))?;
        let epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        endpoint.query_pairs_mut().append_pair("stcs", &epoch_ms);

        let image_part = build_png_part(png_bytes)?;
        let form = Form::new()
            .part("encoded_image", image_part)
            .text("processed_image_dimensions", format!("{width},{height}"));

        let response = self
            .client
            .post(endpoint)
            .multipart(form)
            .send()
            .await
            .map_err(|err| format!("Google Lens upload request failed: {err}"))?;

        let status = response.status();
        if status.is_redirection() {
            extract_redirect_location(base_url, &response)
        } else {
            Err(format!(
                "Google Lens upload returned HTTP {status} instead of a redirect"
            ))
        }
    }

    async fn upload_yandex(&self, png_bytes: Vec<u8>) -> Result<String, String> {
        let base_url = self.endpoints.yandex_base_url.trim_end_matches('/');
        let mut endpoint = Url::parse(&format!("{base_url}/images/search"))
            .map_err(|err| format!("invalid Yandex endpoint URL: {err}"))?;
        endpoint
            .query_pairs_mut()
            .append_pair("rpt", "imageview")
            .append_pair("format", "json")
            .append_pair(
                "request",
                r#"{"blocks":[{"block":"b-page_type_search-by-image__link"}]}"#,
            );

        let image_part = build_png_part(png_bytes)?;
        let form = Form::new().part("upfile", image_part);

        let response = self
            .client
            .post(endpoint)
            .multipart(form)
            .send()
            .await
            .map_err(|err| format!("Yandex upload request failed: {err}"))?;

        let status = response.status();
        if status.is_redirection() {
            return extract_redirect_location(base_url, &response);
        }
        if !status.is_success() {
            return Err(format!("Yandex upload returned HTTP {status}"));
        }

        let body = response
            .text()
            .await
            .map_err(|err| format!("failed to read Yandex response body: {err}"))?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|err| format!("Yandex response was not valid JSON: {err}"))?;
        parse_yandex_json_url(base_url, &json)
    }

    async fn upload_bing(&self, png_bytes: Vec<u8>) -> Result<String, String> {
        let base_url = self.endpoints.bing_base_url.trim_end_matches('/');
        let mut endpoint = Url::parse(&format!("{base_url}/images/search"))
            .map_err(|err| format!("invalid Bing endpoint URL: {err}"))?;
        endpoint
            .query_pairs_mut()
            .append_pair("view", "detailv2")
            .append_pair("iss", "sbiupload")
            .append_pair("FORM", "SBIHMP");

        let image_part = build_png_part(png_bytes)?;
        let form = Form::new().part("file", image_part);

        let response = self
            .client
            .post(endpoint)
            .multipart(form)
            .send()
            .await
            .map_err(|err| format!("Bing upload request failed: {err}"))?;

        let status = response.status();
        if status.is_redirection() {
            return extract_redirect_location(base_url, &response);
        }
        if !status.is_success() {
            return Err(format!("Bing upload returned HTTP {status}"));
        }

        let body = response
            .text()
            .await
            .map_err(|err| format!("failed to read Bing response body: {err}"))?;
        let json: Value = serde_json::from_str(&body)
            .map_err(|err| format!("Bing response was neither redirect nor JSON: {err}"))?;
        parse_bing_json_url(base_url, &json)
    }
}

fn build_http_client(timeout: Duration) -> Result<reqwest::Client, SearchError> {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(timeout)
        .user_agent(DEFAULT_USER_AGENT)
        .build()
        .map_err(|err| SearchError::HttpClientInit(err.to_string()))
}

fn validate_frame(frame: &CaptureFrame) -> Result<(usize, usize), SearchError> {
    if frame.width == 0 || frame.height == 0 {
        return Err(SearchError::InvalidFrame(format!(
            "frame dimensions must be non-zero, got {}x{}",
            frame.width, frame.height
        )));
    }
    let width = usize::try_from(frame.width).map_err(|_| {
        SearchError::InvalidFrame(format!("frame width {} exceeds usize", frame.width))
    })?;
    let height = usize::try_from(frame.height).map_err(|_| {
        SearchError::InvalidFrame(format!("frame height {} exceeds usize", frame.height))
    })?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            SearchError::InvalidFrame(format!(
                "frame dimensions {}x{} overflow RGBA buffer length",
                frame.width, frame.height
            ))
        })?;
    if frame.rgba.len() != expected {
        return Err(SearchError::InvalidFrame(format!(
            "invalid RGBA buffer length for {}x{}: expected {expected} bytes, got {}",
            frame.width,
            frame.height,
            frame.rgba.len()
        )));
    }
    Ok((width, height))
}

fn encode_validated_rgba_to_png(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, SearchError> {
    let mut png_bytes = Vec::new();
    let encoder = PngEncoder::new(&mut png_bytes);
    encoder
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|err| SearchError::EncodeFailed(err.to_string()))?;
    Ok(png_bytes)
}

fn build_png_part(png_bytes: Vec<u8>) -> Result<Part, String> {
    Part::bytes(png_bytes)
        .file_name("image.png")
        .mime_str("image/png")
        .map_err(|err| format!("failed to construct PNG multipart payload: {err}"))
}

fn validate_http_url(raw: &str) -> Result<Url, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("URL must not be empty".to_owned());
    }
    let parsed = Url::parse(trimmed).map_err(|err| format!("invalid URL `{trimmed}`: {err}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "unsupported URL scheme `{}` (only http/https allowed)",
            parsed.scheme()
        ));
    }
    if !parsed.has_host() {
        return Err(format!("URL `{trimmed}` is missing a host"));
    }
    Ok(parsed)
}

fn resolve_redirect_url(base_url: &str, location: &str) -> Result<String, String> {
    let trimmed = location.trim();
    if trimmed.is_empty() {
        return Err("redirect Location header was empty".to_owned());
    }

    if let Ok(parsed) = Url::parse(trimmed) {
        return validate_http_url(parsed.as_str()).map(String::from);
    }

    let base = validate_http_url(base_url)?;
    let joined = base
        .join(trimmed)
        .map_err(|err| format!("failed to resolve redirect `{trimmed}`: {err}"))?;
    validate_http_url(joined.as_str()).map(String::from)
}

fn extract_redirect_location(
    base_url: &str,
    response: &reqwest::Response,
) -> Result<String, String> {
    let header_value = response
        .headers()
        .get(LOCATION)
        .ok_or_else(|| "redirect response missing Location header".to_owned())?;
    let location = header_value
        .to_str()
        .map_err(|err| format!("redirect Location header is not valid UTF-8: {err}"))?;
    resolve_redirect_url(base_url, location)
}

fn parse_yandex_json_url(base_url: &str, json: &Value) -> Result<String, String> {
    let candidate = json
        .get("url")
        .and_then(Value::as_str)
        .or_else(|| {
            json.get("blocks")
                .and_then(Value::as_array)
                .and_then(|blocks| {
                    blocks.iter().find_map(|block| {
                        block
                            .get("params")
                            .and_then(|params| params.get("url"))
                            .and_then(Value::as_str)
                    })
                })
        })
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(raw) = candidate {
        if Url::parse(raw).is_ok() || raw.starts_with('/') {
            return resolve_redirect_url(base_url, raw);
        }
        let query = raw.trim_start_matches('?');
        if !query.is_empty()
            && query.contains('=')
            && !query.chars().any(char::is_whitespace)
            && !query.contains("://")
        {
            let combined = format!("{}/images/search?{query}", base_url.trim_end_matches('/'));
            return validate_http_url(&combined).map(String::from);
        }
        return Err(format!("unrecognized Yandex result URL payload `{raw}`"));
    }

    if let Some(cbir_id) = json
        .get("cbirId")
        .or_else(|| json.get("cbir_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut url =
            validate_http_url(&format!("{}/images/search", base_url.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .append_pair("rpt", "imageview")
            .append_pair("cbir_id", cbir_id);
        return Ok(url.into());
    }

    Err("Yandex JSON response did not contain a result URL".to_owned())
}

fn parse_bing_json_url(base_url: &str, json: &Value) -> Result<String, String> {
    if let Some(raw) = json
        .get("webSearchUrl")
        .or_else(|| json.get("url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return resolve_redirect_url(base_url, raw);
    }

    if let Some(token) = json
        .get("imageInsightsToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let mut url =
            validate_http_url(&format!("{}/images/search", base_url.trim_end_matches('/')))?;
        url.query_pairs_mut()
            .append_pair("view", "detailv2")
            .append_pair("iss", "sbi")
            .append_pair("insightsToken", token);
        return Ok(url.into());
    }

    Err("Bing JSON response did not contain a result URL or insights token".to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RecordedClipboardImage {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    }

    #[derive(Debug, Default)]
    struct FakeClipboard {
        texts: Mutex<Vec<String>>,
        images: Mutex<Vec<RecordedClipboardImage>>,
        fail_image_message: Option<String>,
    }

    impl FakeClipboard {
        fn new() -> Self {
            Self::default()
        }

        fn failing_image(message: impl Into<String>) -> Self {
            Self {
                texts: Mutex::new(Vec::new()),
                images: Mutex::new(Vec::new()),
                fail_image_message: Some(message.into()),
            }
        }

        fn image_writes(&self) -> Vec<RecordedClipboardImage> {
            match self.images.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

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
            if let Some(msg) = &self.fail_image_message {
                return Err(PlatformError::ClipboardFailed(msg.clone()));
            }
            let record = RecordedClipboardImage {
                width,
                height,
                rgba: rgba.to_vec(),
            };
            match self.images.lock() {
                Ok(mut guard) => guard.push(record),
                Err(poisoned) => poisoned.into_inner().push(record),
            }
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct FakeBrowser {
        opened_urls: Mutex<Vec<String>>,
        fail_message: Option<String>,
    }

    impl FakeBrowser {
        fn new() -> Self {
            Self::default()
        }

        fn failing(message: impl Into<String>) -> Self {
            Self {
                opened_urls: Mutex::new(Vec::new()),
                fail_message: Some(message.into()),
            }
        }

        fn opened_urls(&self) -> Vec<String> {
            match self.opened_urls.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }
    }

    impl BrowserOpener for FakeBrowser {
        fn open_url(&self, url: &str) -> Result<(), SearchError> {
            if let Some(msg) = &self.fail_message {
                return Err(SearchError::BrowserOpenFailed {
                    url: url.to_owned(),
                    reason: msg.clone(),
                });
            }
            match self.opened_urls.lock() {
                Ok(mut guard) => guard.push(url.to_owned()),
                Err(poisoned) => poisoned.into_inner().push(url.to_owned()),
            }
            Ok(())
        }
    }

    fn sample_frame() -> CaptureFrame {
        // Synthetic 3x2 RGBA image with distinct pixels so encoding and clipboard checks are exact.
        let mut rgba = Vec::with_capacity(3 * 2 * 4);
        for y in 0..2u8 {
            for x in 0..3u8 {
                rgba.extend_from_slice(&[x * 80, y * 120, 42, 255]);
            }
        }
        CaptureFrame {
            width: 3,
            height: 2,
            scale_factor: 1.5,
            rgba,
        }
    }

    fn request_contains_png_and_field(req: &Request, field_name: &str) -> bool {
        const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
        let has_png = req
            .body
            .windows(PNG_MAGIC.len())
            .any(|window| window == PNG_MAGIC);
        let field_marker = format!("name=\"{field_name}\"");
        let has_field = req
            .body
            .windows(field_marker.len())
            .any(|window| window == field_marker.as_bytes());
        has_png && has_field
    }

    #[test]
    fn service_and_browser_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VisualSearchService>();
        assert_send_sync::<SystemBrowserOpener>();
        assert_send_sync::<SearchError>();
    }

    #[test]
    fn encode_frame_to_png_produces_valid_decodable_png() -> TestResult {
        let frame = sample_frame();
        let png_bytes = encode_frame_to_png(&frame)?;
        assert!(png_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));

        let decoded =
            image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)?.to_rgba8();
        assert_eq!(decoded.width(), frame.width);
        assert_eq!(decoded.height(), frame.height);
        assert_eq!(decoded.as_raw(), &frame.rgba);
        Ok(())
    }

    #[test]
    fn system_browser_opener_rejects_unsafe_url_schemes() {
        let opener = SystemBrowserOpener::new();
        for unsafe_url in [
            "",
            "   ",
            "file:///C:/Windows/System32/cmd.exe",
            "javascript:alert(1)",
            "ftp://example.com/file.png",
        ] {
            let res = opener.open_url(unsafe_url);
            assert!(
                matches!(res, Err(SearchError::BrowserOpenFailed { .. })),
                "expected BrowserOpenFailed for `{unsafe_url}`, got {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn google_lens_direct_upload_opens_redirect_url_without_touching_clipboard() -> TestResult
    {
        let server = MockServer::start().await;
        let expected_url = "https://lens.google.com/search?ep=ccm&p=abc123";

        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(move |req: &Request| {
                assert!(
                    request_contains_png_and_field(req, "encoded_image"),
                    "expected multipart encoded_image PNG payload"
                );
                ResponseTemplate::new(302).insert_header("Location", expected_url)
            })
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::GoogleLens, true)
            .await?;

        assert_eq!(
            outcome,
            VisualSearchOutcome::OpenedResultUrl {
                engine: VisualSearchEngine::GoogleLens,
                url: expected_url.to_owned(),
            }
        );
        assert_eq!(browser.opened_urls(), vec![expected_url.to_owned()]);
        assert!(clipboard.image_writes().is_empty());
        assert!(clipboard.text_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn google_lens_resolves_relative_location_redirect_against_base_url() -> TestResult {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(
                ResponseTemplate::new(303).insert_header("Location", "/search?ep=ccm&p=rel999"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let expected_url = format!("{}/search?ep=ccm&p=rel999", server.uri());
        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::GoogleLens, true)
            .await?;

        assert_eq!(
            outcome,
            VisualSearchOutcome::OpenedResultUrl {
                engine: VisualSearchEngine::GoogleLens,
                url: expected_url.clone(),
            }
        );
        assert_eq!(browser.opened_urls(), vec![expected_url]);
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn google_lens_failure_copies_crop_to_clipboard_and_opens_fallback_page() -> TestResult {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .expect(1)
            .mount(&server)
            .await;

        let frame = sample_frame();
        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&frame, VisualSearchEngine::GoogleLens, true)
            .await?;

        match outcome {
            VisualSearchOutcome::FallbackClipboard {
                engine,
                fallback_url,
                reason,
            } => {
                assert_eq!(engine, VisualSearchEngine::GoogleLens);
                assert_eq!(fallback_url, "https://lens.google.com");
                assert!(reason.contains("503"), "expected 503 in reason: {reason}");
            }
            other => return Err(format!("expected FallbackClipboard, got {other:?}").into()),
        }

        assert_eq!(browser.opened_urls(), vec!["https://lens.google.com"]);
        assert_eq!(
            clipboard.image_writes(),
            vec![RecordedClipboardImage {
                width: frame.width as usize,
                height: frame.height as usize,
                rgba: frame.rgba,
            }]
        );
        Ok(())
    }

    #[tokio::test]
    async fn google_lens_timeout_falls_back_to_clipboard_and_opens_landing_page() -> TestResult {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(
                ResponseTemplate::new(302)
                    .set_delay(Duration::from_millis(500))
                    .insert_header("Location", "https://lens.google.com/search?p=late"),
            )
            .mount(&server)
            .await;

        let frame = sample_frame();
        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints_and_timeout(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
            Duration::from_millis(100),
        )?;

        let outcome = service
            .search(&frame, VisualSearchEngine::GoogleLens, true)
            .await?;

        match outcome {
            VisualSearchOutcome::FallbackClipboard {
                engine,
                fallback_url,
                reason,
            } => {
                assert_eq!(engine, VisualSearchEngine::GoogleLens);
                assert_eq!(fallback_url, "https://lens.google.com");
                assert!(!reason.is_empty());
            }
            other => return Err(format!("expected FallbackClipboard, got {other:?}").into()),
        }

        assert_eq!(browser.opened_urls(), vec!["https://lens.google.com"]);
        assert_eq!(clipboard.image_writes().len(), 1);
        assert_eq!(clipboard.image_writes()[0].rgba, frame.rgba);
        Ok(())
    }

    #[tokio::test]
    async fn google_lens_non_redirect_and_missing_location_fall_back_to_clipboard() -> TestResult {
        for response in [
            ResponseTemplate::new(200).set_body_string("<html>not a redirect</html>"),
            ResponseTemplate::new(302),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v3/upload"))
                .respond_with(response)
                .expect(1)
                .mount(&server)
                .await;

            let frame = sample_frame();
            let browser = Arc::new(FakeBrowser::new());
            let clipboard = Arc::new(FakeClipboard::new());
            let endpoints =
                VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
            let service = VisualSearchService::with_endpoints(
                Arc::clone(&browser) as Arc<dyn BrowserOpener>,
                Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
                endpoints,
            )?;

            let outcome = service
                .search(&frame, VisualSearchEngine::GoogleLens, true)
                .await?;

            assert!(matches!(
                outcome,
                VisualSearchOutcome::FallbackClipboard {
                    engine: VisualSearchEngine::GoogleLens,
                    ref fallback_url,
                    ..
                } if fallback_url == "https://lens.google.com"
            ));
            assert_eq!(browser.opened_urls(), vec!["https://lens.google.com"]);
            assert_eq!(clipboard.image_writes().len(), 1);
            assert_eq!(clipboard.image_writes()[0].rgba, frame.rgba);
        }
        Ok(())
    }

    #[tokio::test]
    async fn google_lens_unsafe_redirect_scheme_falls_back_to_clipboard() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "file:///C:/Windows/System32/cmd.exe"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::GoogleLens, true)
            .await?;

        assert!(matches!(
            outcome,
            VisualSearchOutcome::FallbackClipboard {
                engine: VisualSearchEngine::GoogleLens,
                ref fallback_url,
                ..
            } if fallback_url == "https://lens.google.com"
        ));
        assert_eq!(browser.opened_urls(), vec!["https://lens.google.com"]);
        assert_eq!(clipboard.image_writes().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn returns_error_when_upload_fails_and_clipboard_fallback_disabled() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let res = service
            .search(&sample_frame(), VisualSearchEngine::GoogleLens, false)
            .await;

        assert!(matches!(res, Err(SearchError::UploadFailed(_))));
        assert!(browser.opened_urls().is_empty());
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn yandex_direct_upload_opens_json_result_url_without_touching_clipboard() -> TestResult {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "blocks": [
                {
                    "params": {
                        "url": "cbir_id=12345%2Fabc&rpt=imageview"
                    }
                }
            ]
        });

        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(move |req: &Request| {
                assert!(
                    request_contains_png_and_field(req, "upfile"),
                    "expected multipart upfile PNG payload"
                );
                ResponseTemplate::new(200).set_body_json(body.clone())
            })
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_yandex_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let expected_url = format!(
            "{}/images/search?cbir_id=12345%2Fabc&rpt=imageview",
            server.uri()
        );
        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::Yandex, true)
            .await?;

        assert_eq!(
            outcome,
            VisualSearchOutcome::OpenedResultUrl {
                engine: VisualSearchEngine::Yandex,
                url: expected_url.clone(),
            }
        );
        assert_eq!(browser.opened_urls(), vec![expected_url]);
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn yandex_unsafe_json_url_falls_back_to_clipboard() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "url": "javascript:alert(1)" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_yandex_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::Yandex, true)
            .await?;

        assert!(matches!(
            outcome,
            VisualSearchOutcome::FallbackClipboard {
                engine: VisualSearchEngine::Yandex,
                ref fallback_url,
                ..
            } if fallback_url == "https://yandex.com/images/"
        ));
        assert_eq!(browser.opened_urls(), vec!["https://yandex.com/images/"]);
        assert_eq!(clipboard.image_writes().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn yandex_failure_falls_back_to_clipboard_and_opens_yandex_images() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let frame = sample_frame();
        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_yandex_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&frame, VisualSearchEngine::Yandex, true)
            .await?;

        assert!(matches!(
            outcome,
            VisualSearchOutcome::FallbackClipboard {
                engine: VisualSearchEngine::Yandex,
                ref fallback_url,
                ..
            } if fallback_url == "https://yandex.com/images/"
        ));
        assert_eq!(browser.opened_urls(), vec!["https://yandex.com/images/"]);
        assert_eq!(clipboard.image_writes().len(), 1);
        assert_eq!(clipboard.image_writes()[0].rgba, frame.rgba);
        Ok(())
    }

    #[tokio::test]
    async fn bing_direct_upload_opens_redirect_url_without_touching_clipboard() -> TestResult {
        let server = MockServer::start().await;
        let expected_url = "https://www.bing.com/images/search?view=detailv2&iss=sbi&idpbck=xyz";

        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(move |req: &Request| {
                assert!(
                    request_contains_png_and_field(req, "file"),
                    "expected multipart file PNG payload"
                );
                ResponseTemplate::new(302).insert_header("Location", expected_url)
            })
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_bing_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::Bing, true)
            .await?;

        assert_eq!(
            outcome,
            VisualSearchOutcome::OpenedResultUrl {
                engine: VisualSearchEngine::Bing,
                url: expected_url.to_owned(),
            }
        );
        assert_eq!(browser.opened_urls(), vec![expected_url.to_owned()]);
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bing_direct_upload_accepts_json_insights_response() -> TestResult {
        let server = MockServer::start().await;
        let expected_url = "https://www.bing.com/images/search?view=detailv2&iss=sbi&q=img";

        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "webSearchUrl": expected_url })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_bing_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::Bing, true)
            .await?;

        assert_eq!(
            outcome,
            VisualSearchOutcome::OpenedResultUrl {
                engine: VisualSearchEngine::Bing,
                url: expected_url.to_owned(),
            }
        );
        assert_eq!(browser.opened_urls(), vec![expected_url.to_owned()]);
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bing_direct_upload_builds_url_from_image_insights_token() -> TestResult {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "imageInsightsToken": "ccid_tok+123" })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_bing_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let expected_url = format!(
            "{}/images/search?view=detailv2&iss=sbi&insightsToken=ccid_tok%2B123",
            server.uri()
        );
        let outcome = service
            .search(&sample_frame(), VisualSearchEngine::Bing, true)
            .await?;

        assert_eq!(
            outcome,
            VisualSearchOutcome::OpenedResultUrl {
                engine: VisualSearchEngine::Bing,
                url: expected_url.clone(),
            }
        );
        assert_eq!(browser.opened_urls(), vec![expected_url]);
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn bing_failure_falls_back_to_clipboard_and_opens_bing_visualsearch() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let frame = sample_frame();
        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_bing_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let outcome = service
            .search(&frame, VisualSearchEngine::Bing, true)
            .await?;

        assert!(matches!(
            outcome,
            VisualSearchOutcome::FallbackClipboard {
                engine: VisualSearchEngine::Bing,
                ref fallback_url,
                ..
            } if fallback_url == "https://www.bing.com/visualsearch"
        ));
        assert_eq!(
            browser.opened_urls(),
            vec!["https://www.bing.com/visualsearch"]
        );
        assert_eq!(clipboard.image_writes().len(), 1);
        assert_eq!(clipboard.image_writes()[0].rgba, frame.rgba);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_invalid_capture_frame_without_network_or_clipboard_calls() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(ResponseTemplate::new(302))
            .expect(0)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        for bad_frame in [
            CaptureFrame {
                width: 0,
                height: 10,
                scale_factor: 1.0,
                rgba: Vec::new(),
            },
            CaptureFrame {
                width: 2,
                height: 2,
                scale_factor: 1.0,
                rgba: vec![0; 8],
            },
        ] {
            let res = service
                .search(&bad_frame, VisualSearchEngine::GoogleLens, true)
                .await;
            assert!(matches!(res, Err(SearchError::InvalidFrame(_))));
        }

        assert!(browser.opened_urls().is_empty());
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn propagates_browser_error_on_direct_success_without_triggering_fallback() -> TestResult
    {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "https://lens.google.com/search?p=ok"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::failing("no default browser"));
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let res = service
            .search(&sample_frame(), VisualSearchEngine::GoogleLens, true)
            .await;

        assert!(matches!(res, Err(SearchError::BrowserOpenFailed { .. })));
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn propagates_clipboard_error_during_fallback_without_opening_browser() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v3/upload"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::failing_image("clipboard unavailable"));
        let endpoints = VisualSearchEndpoints::default().with_google_lens_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let res = service
            .search(&sample_frame(), VisualSearchEngine::GoogleLens, true)
            .await;

        assert!(matches!(res, Err(SearchError::ClipboardFailed(_))));
        assert!(browser.opened_urls().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn search_with_config_delegates_engine_and_fallback_flag() -> TestResult {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/images/search"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;

        let browser = Arc::new(FakeBrowser::new());
        let clipboard = Arc::new(FakeClipboard::new());
        let endpoints = VisualSearchEndpoints::default().with_yandex_base_url(server.uri());
        let service = VisualSearchService::with_endpoints(
            Arc::clone(&browser) as Arc<dyn BrowserOpener>,
            Arc::clone(&clipboard) as Arc<dyn ClipboardService>,
            endpoints,
        )?;

        let config = VisualSearchConfig {
            engine: VisualSearchEngine::Yandex,
            enable_clipboard_fallback: false,
        };
        let res = service.search_with_config(&sample_frame(), &config).await;

        assert!(matches!(res, Err(SearchError::UploadFailed(_))));
        assert!(browser.opened_urls().is_empty());
        assert!(clipboard.image_writes().is_empty());
        Ok(())
    }
}
