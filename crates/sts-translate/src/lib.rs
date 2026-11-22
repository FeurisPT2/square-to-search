//! Translation providers, collision-resistant batch packing, LRU-cached services,
//! BYOK / local LLM providers, and multimodal streaming Vision AI for `square-to-search`.

pub mod batch;
pub mod bing_web;
pub mod deepl;
pub mod factory;
pub mod gemini;
pub mod google_web;
pub mod ollama;
pub mod openai;
pub mod service;

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

pub use batch::{PackedBatch, pack_batch, unpack_batch};
pub use bing_web::BingWebTranslator;
pub use deepl::DeepLTranslator;
pub use factory::{
    ProviderSecrets, build_translator, build_vision_ai_provider, vision_provider_display_label,
};
pub use gemini::GeminiProvider;
pub use google_web::GoogleWebTranslator;
pub use ollama::OllamaProvider;
pub use openai::OpenAiCompatProvider;
pub use service::{CachedFallbackTranslator, CachedTranslator};

const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_AI_HTTP_TIMEOUT: Duration = Duration::from_secs(90);
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Heap-allocated `Send` future used by object-safe async traits in `sts-translate`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Errors returned by translation and vision AI providers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TranslateError {
    /// Upstream provider rejected the request with HTTP 429, a CAPTCHA page, or quota exhaustion.
    #[error("translation provider rate limited: {0}")]
    RateLimited(String),

    /// Network, DNS, TLS, or timeout failure while communicating with a provider.
    #[error("translation network error: {0}")]
    Network(String),

    /// Provider returned an unexpected HTTP status, malformed payload, or mismatched batch count.
    #[error("invalid translation provider response: {0}")]
    InvalidResponse(String),

    /// Source or target language code is missing or unsupported by the provider.
    #[error("unsupported translation language: {0}")]
    UnsupportedLanguage(String),

    /// Upstream provider rejected credentials or authorization (HTTP 401/403).
    #[error("authentication failed for translation/AI provider: {0}")]
    AuthFailed(String),

    /// Required provider configuration or API key is missing.
    #[error("translation/AI provider is not configured: {0}")]
    ProviderNotConfigured(String),
}

impl From<reqwest::Error> for TranslateError {
    fn from(err: reqwest::Error) -> Self {
        Self::Network(err.to_string())
    }
}

/// Ordered batch translation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslateRequest {
    /// Source language code (e.g. `"auto"`, `"ja"`, `"ko"`, `"zh-CN"`, `"en"`).
    pub source_lang: String,
    /// Target language code (e.g. `"vi"`, `"en"`).
    pub target_lang: String,
    /// Ordered list of text segments to translate.
    pub texts: Vec<String>,
}

impl TranslateRequest {
    /// Creates a new [`TranslateRequest`].
    #[must_use]
    pub fn new(
        source_lang: impl Into<String>,
        target_lang: impl Into<String>,
        texts: Vec<String>,
    ) -> Self {
        Self {
            source_lang: source_lang.into(),
            target_lang: target_lang.into(),
            texts,
        }
    }
}

/// Ordered batch translation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslateResponse {
    /// Translated text segments matching the input order and length.
    pub translations: Vec<String>,
    /// Identifier of the provider that served the response.
    pub provider_name: String,
}

/// Asynchronous batch text translation provider.
///
/// Uses [`BoxFuture`] so providers can be stored dynamically as `Arc<dyn Translator>`.
pub trait Translator: Send + Sync {
    /// Stable human-readable identifier for this translator (e.g. `"google-web"`, `"bing-web"`).
    fn provider_name(&self) -> &'static str;

    /// Translates every text segment in `request`, preserving exact order.
    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>>;
}

/// Request payload for multimodal vision AI queries (Milestone 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionAiRequest {
    /// Optional PNG-encoded image crop of the selected region.
    pub image_png: Option<Vec<u8>>,
    /// Optional OCR text extracted from the region.
    pub ocr_text: Option<String>,
    /// User question or quick-action instruction.
    pub prompt: String,
}

/// Response returned by a [`VisionAiProvider`] (Milestone 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionAiResponse {
    /// Answer text produced by the AI provider.
    pub answer: String,
    /// Identifier of the AI provider / model.
    pub provider_name: String,
}

/// Multimodal AI provider trait for Milestone 3 (Gemini / OpenAI-compatible / Ollama).
///
/// Uses [`BoxFuture`] so providers can be stored dynamically as `Arc<dyn VisionAiProvider>`.
pub trait VisionAiProvider: Send + Sync {
    /// Stable human-readable identifier for this AI provider.
    fn provider_name(&self) -> &'static str;

    /// Answers a multimodal prompt about an image crop and/or OCR text.
    fn ask<'a>(
        &'a self,
        request: &'a VisionAiRequest,
    ) -> BoxFuture<'a, Result<VisionAiResponse, TranslateError>>;

    /// Streams incremental answer chunks over `delta_tx` and resolves with the complete response.
    fn ask_stream<'a>(
        &'a self,
        request: &'a VisionAiRequest,
        delta_tx: UnboundedSender<String>,
    ) -> BoxFuture<'a, Result<VisionAiResponse, TranslateError>> {
        Box::pin(async move {
            let response = self.ask(request).await?;
            if !response.answer.is_empty() {
                let _ = delta_tx.send(response.answer.clone());
            }
            Ok(response)
        })
    }
}

/// Incremental UTF-8-safe line splitter for chunked HTTP streaming responses (SSE / NDJSON).
#[derive(Debug, Default)]
pub(crate) struct LineStreamDecoder {
    pending: Vec<u8>,
}

impl LineStreamDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) -> Vec<String> {
        if bytes.is_empty() {
            return Vec::new();
        }
        self.pending.extend_from_slice(bytes);
        let mut lines = Vec::new();
        let mut start = 0;

        // Splitting only at `\n` (0x0A) is UTF-8-safe because 0x0A never appears inside multi-byte UTF-8 codepoints.
        while let Some(rel_nl) = self.pending[start..].iter().position(|&b| b == b'\n') {
            let nl_pos = start + rel_nl;
            let raw_line = &self.pending[start..nl_pos];
            let trimmed_cr = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            lines.push(String::from_utf8_lossy(trimmed_cr).into_owned());
            start = nl_pos + 1;
        }

        if start > 0 {
            self.pending.drain(..start);
        }
        lines
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let tail = std::mem::take(&mut self.pending);
        let trimmed_cr = tail.strip_suffix(b"\r").unwrap_or(&tail);
        if trimmed_cr.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(trimmed_cr).into_owned())
        }
    }
}

pub(crate) fn map_http_status_error(
    provider: &str,
    status: StatusCode,
    body: &str,
) -> TranslateError {
    let summary = summarize_body(body);
    let detail = if summary.is_empty() {
        format!("{provider} returned HTTP {status}")
    } else {
        format!("{provider} returned HTTP {status}: {summary}")
    };

    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        TranslateError::AuthFailed(detail)
    } else if status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 456 {
        TranslateError::RateLimited(detail)
    } else {
        TranslateError::InvalidResponse(detail)
    }
}

pub(crate) async fn post_json_checked(
    provider: &str,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, TranslateError> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(map_http_status_error(provider, status, &body))
}

pub(crate) async fn post_json_for_value(
    provider: &str,
    request: reqwest::RequestBuilder,
) -> Result<Value, TranslateError> {
    let response = post_json_checked(provider, request).await?;
    let body = response.text().await?;
    serde_json::from_str(&body).map_err(|err| {
        TranslateError::InvalidResponse(format!(
            "{provider} returned malformed JSON ({err}): {}",
            summarize_body(&body)
        ))
    })
}

pub(crate) async fn read_line_stream<F>(
    mut response: reqwest::Response,
    mut on_line: F,
) -> Result<(), TranslateError>
where
    F: FnMut(&str) -> Result<bool, TranslateError>,
{
    let mut decoder = LineStreamDecoder::new();
    while let Some(chunk) = response.chunk().await? {
        for line in decoder.push_bytes(&chunk) {
            if !on_line(&line)? {
                return Ok(());
            }
        }
    }
    if let Some(tail) = decoder.finish() {
        let _ = on_line(&tail)?;
    }
    Ok(())
}

pub(crate) async fn stream_sse_json<F>(
    provider: &str,
    response: reqwest::Response,
    delta_tx: &UnboundedSender<String>,
    mut extract_delta: F,
) -> Result<String, TranslateError>
where
    F: FnMut(&Value) -> Result<Option<String>, TranslateError>,
{
    let mut full_answer = String::new();
    read_line_stream(response, |line| {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(':') {
            return Ok(true);
        }
        let Some(payload) = trimmed.strip_prefix("data:") else {
            return Ok(true);
        };
        let data = payload.trim();
        if data.is_empty() {
            return Ok(true);
        }
        if data == "[DONE]" {
            return Ok(false);
        }

        let value: Value = serde_json::from_str(data).map_err(|err| {
            TranslateError::InvalidResponse(format!(
                "{provider} returned malformed SSE JSON chunk ({err}): {}",
                summarize_body(data)
            ))
        })?;

        if let Some(delta) = extract_delta(&value)?
            && !delta.is_empty()
        {
            full_answer.push_str(&delta);
            let _ = delta_tx.send(delta);
        }
        Ok(true)
    })
    .await?;

    Ok(full_answer)
}

pub(crate) struct PromptMessages {
    pub(crate) system: String,
    pub(crate) user: String,
}

pub(crate) fn build_translation_prompts(
    source_lang: &str,
    target_lang: &str,
    packed: &PackedBatch,
    segment_count: usize,
    custom_prompt: Option<&str>,
) -> PromptMessages {
    let source_desc = if source_lang.trim().eq_ignore_ascii_case("auto") {
        "the detected source language".to_owned()
    } else {
        format!("`{}`", source_lang.trim())
    };
    let mut system = format!(
        "You are a precise translator for comics, manga, and screen text. \
         Translate from {source_desc} into `{}`.",
        target_lang.trim()
    );
    if segment_count > 1 {
        system.push_str(&format!(
            " There are exactly {segment_count} segments separated by `{}`. \
             Preserve the exact delimiter `{}` between translated segments and return \
             ONLY the translated segments with no numbering or commentary.",
            packed.delimiter, packed.delimiter
        ));
    } else {
        system.push_str(
            " Return ONLY the translated text with no extra commentary or quotation marks.",
        );
    }

    if let Some(extra) = custom_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\nAdditional instructions:\n");
        system.push_str(extra);
    }

    PromptMessages {
        system,
        user: packed.payload.clone(),
    }
}

pub(crate) fn build_vision_prompts(
    request: &VisionAiRequest,
    custom_prompt: Option<&str>,
) -> Result<PromptMessages, TranslateError> {
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(TranslateError::InvalidResponse(
            "Ask AI prompt cannot be empty".into(),
        ));
    }

    let mut system = String::from(
        "You are a concise, helpful multimodal assistant for comics and screen captures. \
         Answer clearly in the user's language.",
    );
    if let Some(extra) = custom_prompt.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\nAdditional instructions:\n");
        system.push_str(extra);
    }

    let mut user = prompt.to_owned();
    if let Some(ocr) = request
        .ocr_text
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        user.push_str("\n\nOCR text:\n");
        user.push_str(ocr);
    }

    Ok(PromptMessages { system, user })
}

pub(crate) async fn translate_with_llm_batch<F, Fut>(
    provider_name: &'static str,
    request: &TranslateRequest,
    custom_prompt: Option<&str>,
    generate: F,
) -> Result<TranslateResponse, TranslateError>
where
    F: FnOnce(PromptMessages) -> Fut,
    Fut: Future<Output = Result<String, TranslateError>>,
{
    validate_languages(&request.source_lang, &request.target_lang)?;
    if request.texts.is_empty() {
        return Ok(TranslateResponse {
            translations: Vec::new(),
            provider_name: provider_name.to_owned(),
        });
    }

    let refs: Vec<&str> = request.texts.iter().map(String::as_str).collect();
    let packed = pack_batch(&refs);
    let prompts = build_translation_prompts(
        &request.source_lang,
        &request.target_lang,
        &packed,
        request.texts.len(),
        custom_prompt,
    );
    let raw = generate(prompts).await?;
    let translations = unpack_batch(&raw, &packed.delimiter, request.texts.len())?;

    Ok(TranslateResponse {
        translations,
        provider_name: provider_name.to_owned(),
    })
}

pub(crate) fn normalize_base_url(base_url: &str, default_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        default_url.to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub(crate) fn require_non_empty<'a>(
    label: &str,
    value: &'a str,
) -> Result<&'a str, TranslateError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        Err(TranslateError::ProviderNotConfigured(format!(
            "{label} is not configured"
        )))
    } else {
        Ok(trimmed)
    }
}

pub(crate) fn validate_languages(
    source_lang: &str,
    target_lang: &str,
) -> Result<(), TranslateError> {
    let source = source_lang.trim();
    if source.is_empty() {
        return Err(TranslateError::UnsupportedLanguage(
            "source language cannot be empty".into(),
        ));
    }

    let target = target_lang.trim();
    if target.is_empty() {
        return Err(TranslateError::UnsupportedLanguage(
            "target language cannot be empty".into(),
        ));
    }
    if target.eq_ignore_ascii_case("auto") || target.eq_ignore_ascii_case("auto-detect") {
        return Err(TranslateError::UnsupportedLanguage(format!(
            "target language cannot be `{target}`"
        )));
    }

    Ok(())
}

pub(crate) fn build_default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(DEFAULT_HTTP_TIMEOUT)
        .user_agent(DEFAULT_USER_AGENT)
        .build()
        .unwrap_or_default()
}

pub(crate) fn build_ai_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(DEFAULT_HTTP_TIMEOUT)
        .timeout(DEFAULT_AI_HTTP_TIMEOUT)
        .user_agent(DEFAULT_USER_AGENT)
        .build()
        .unwrap_or_default()
}

pub(crate) fn looks_like_captcha_or_html(content_type: Option<&str>, body: &str) -> bool {
    let is_html_ct = content_type.is_some_and(|ct| ct.to_ascii_lowercase().contains("text/html"));
    let trimmed = body.trim_start();
    let prefix_lower: String = trimmed
        .chars()
        .take(2048)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    let is_html_body = trimmed.starts_with('<')
        && (prefix_lower.contains("<!doctype") || prefix_lower.contains("<html"));
    let mentions_captcha = prefix_lower.contains("captcha")
        || prefix_lower.contains("unusual traffic")
        || prefix_lower.contains("g-recaptcha")
        || prefix_lower.contains("/sorry/");

    is_html_ct || is_html_body || mentions_captcha
}

pub(crate) fn summarize_body(body: &str) -> String {
    const MAX_CHARS: usize = 160;
    let trimmed = body.trim();
    let mut chars = trimmed.chars();
    let snippet: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{snippet}...")
    } else {
        snippet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_stream_decoder_preserves_multibyte_vietnamese_across_arbitrary_byte_splits() {
        let input = "Tiếng Việt — Truyện tranh\nDòng hai: Đọc truyện\r\nDòng cuối không newline";
        let raw = input.as_bytes();

        for chunk_size in [1usize, 2, 3, 5, 7, 13] {
            let mut decoder = LineStreamDecoder::new();
            let mut lines = Vec::new();
            for chunk in raw.chunks(chunk_size) {
                lines.extend(decoder.push_bytes(chunk));
            }
            if let Some(tail) = decoder.finish() {
                lines.push(tail);
            }
            assert_eq!(
                lines,
                vec![
                    "Tiếng Việt — Truyện tranh",
                    "Dòng hai: Đọc truyện",
                    "Dòng cuối không newline",
                ],
                "failed for chunk_size={chunk_size}"
            );
            for line in &lines {
                assert!(
                    !line.contains('\u{FFFD}'),
                    "found replacement char in {line}"
                );
            }
        }

        // Verify explicit split right inside multi-byte UTF-8 character `'ế'` and `'—'`.
        let split_src = "Tiếng Việt — Truyện tranh\n";
        let bytes = split_src.as_bytes();
        let split_at_e = split_src.find("ếng").map_or(3, |idx| idx + 1);
        let split_at_dash = split_src.find('—').map_or(10, |idx| idx + 2);
        let mut decoder = LineStreamDecoder::new();
        let mut lines = decoder.push_bytes(&bytes[..split_at_e]);
        assert!(lines.is_empty());
        lines.extend(decoder.push_bytes(&bytes[split_at_e..split_at_dash]));
        assert!(lines.is_empty());
        lines.extend(decoder.push_bytes(&bytes[split_at_dash..]));
        assert_eq!(lines, vec!["Tiếng Việt — Truyện tranh"]);
        assert_eq!(decoder.finish(), None);
    }

    #[test]
    fn map_http_status_error_maps_auth_rate_limit_and_unexpected_statuses() {
        assert!(matches!(
            map_http_status_error("Gemini", StatusCode::UNAUTHORIZED, "bad key"),
            TranslateError::AuthFailed(_)
        ));
        assert!(matches!(
            map_http_status_error("OpenAI", StatusCode::FORBIDDEN, "forbidden"),
            TranslateError::AuthFailed(_)
        ));
        assert!(matches!(
            map_http_status_error("Gemini", StatusCode::TOO_MANY_REQUESTS, "quota"),
            TranslateError::RateLimited(_)
        ));
        if let Ok(code_456) = StatusCode::from_u16(456) {
            assert!(matches!(
                map_http_status_error("DeepL", code_456, "quota exceeded"),
                TranslateError::RateLimited(_)
            ));
        }
        assert!(matches!(
            map_http_status_error("Ollama", StatusCode::INTERNAL_SERVER_ERROR, "oops"),
            TranslateError::InvalidResponse(_)
        ));
    }

    #[tokio::test]
    async fn vision_ai_provider_default_ask_stream_emits_full_answer() {
        struct StubVision;
        impl VisionAiProvider for StubVision {
            fn provider_name(&self) -> &'static str {
                "stub-vision"
            }

            fn ask<'a>(
                &'a self,
                request: &'a VisionAiRequest,
            ) -> BoxFuture<'a, Result<VisionAiResponse, TranslateError>> {
                Box::pin(async move {
                    Ok(VisionAiResponse {
                        answer: format!("Answer: {}", request.prompt),
                        provider_name: "stub-vision".into(),
                    })
                })
            }
        }

        let provider = StubVision;
        let req = VisionAiRequest {
            image_png: None,
            ocr_text: None,
            prompt: "Giải thích".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let res = match provider.ask_stream(&req, tx).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected default ask_stream to succeed: {err}"),
        };
        assert_eq!(res.answer, "Answer: Giải thích");
        assert_eq!(rx.recv().await.as_deref(), Some("Answer: Giải thích"));
        assert!(rx.recv().await.is_none());
    }
}
