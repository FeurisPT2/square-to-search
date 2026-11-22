//! Google Gemini BYOK translator and multimodal streaming Vision AI provider.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::{
    BoxFuture, PromptMessages, TranslateError, TranslateRequest, TranslateResponse, Translator,
    VisionAiProvider, VisionAiRequest, VisionAiResponse, build_ai_http_client,
    build_vision_prompts, normalize_base_url, post_json_checked, post_json_for_value,
    require_non_empty, stream_sse_json, translate_with_llm_batch,
};

/// Default base URL for the Google Gemini REST API.
pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

const DEFAULT_MODEL: &str = "gemini-2.5-flash";
const PROVIDER_NAME: &str = "gemini";

/// Google Gemini provider implementing both [`Translator`] and streaming [`VisionAiProvider`].
#[derive(Clone)]
pub struct GeminiProvider {
    base_url: String,
    api_key: String,
    model: String,
    custom_prompt: Option<String>,
    client: reqwest::Client,
}

impl std::fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("custom_prompt", &self.custom_prompt.is_some())
            .finish_non_exhaustive()
    }
}

impl GeminiProvider {
    /// Creates a new [`GeminiProvider`] targeting [`DEFAULT_BASE_URL`].
    #[must_use]
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        custom_prompt: Option<String>,
    ) -> Self {
        Self::with_base_url(DEFAULT_BASE_URL, api_key, model, custom_prompt)
    }

    /// Creates a new [`GeminiProvider`] targeting a custom `base_url` (e.g. for `wiremock` tests).
    #[must_use]
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        custom_prompt: Option<String>,
    ) -> Self {
        Self::with_client(
            base_url,
            api_key,
            model,
            custom_prompt,
            build_ai_http_client(),
        )
    }

    /// Creates a new [`GeminiProvider`] with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        custom_prompt: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        let model_raw = model.into();
        let model_trimmed = model_raw.trim().trim_start_matches("models/");
        let normalized_model = if model_trimmed.is_empty() {
            DEFAULT_MODEL.to_owned()
        } else {
            model_trimmed.to_owned()
        };

        Self {
            base_url: normalize_base_url(&base_url.into(), DEFAULT_BASE_URL),
            api_key: api_key.into().trim().to_owned(),
            model: normalized_model,
            custom_prompt,
            client,
        }
    }

    /// Returns the normalized base URL.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn endpoint(&self, action: &str, sse: bool) -> Result<Url, TranslateError> {
        let mut url = Url::parse(&format!(
            "{}/models/{}:{}",
            self.base_url, self.model, action
        ))
        .map_err(|err| TranslateError::InvalidResponse(format!("invalid Gemini URL: {err}")))?;
        if sse {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
        Ok(url)
    }

    fn build_request_body(prompts: &PromptMessages, image_png: Option<&[u8]>) -> Value {
        let mut parts = vec![json!({ "text": prompts.user })];
        if let Some(png_bytes) = image_png {
            parts.push(json!({
                "inline_data": {
                    "mime_type": "image/png",
                    "data": STANDARD.encode(png_bytes),
                }
            }));
        }
        json!({
            "system_instruction": {
                "parts": [{ "text": prompts.system }]
            },
            "contents": [{
                "role": "user",
                "parts": parts
            }]
        })
    }

    async fn generate_content(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
    ) -> Result<String, TranslateError> {
        let key = require_non_empty("Gemini API key", &self.api_key)?;
        let url = self.endpoint("generateContent", false)?;
        let body = Self::build_request_body(prompts, image_png);
        let value = post_json_for_value(
            "Gemini",
            self.client
                .post(url)
                .header("x-goog-api-key", key)
                .json(&body),
        )
        .await?;

        extract_gemini_text(&value)?.ok_or_else(|| {
            TranslateError::InvalidResponse(
                "Gemini response missing candidates[0].content.parts text".into(),
            )
        })
    }

    async fn stream_content(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
        delta_tx: &UnboundedSender<String>,
    ) -> Result<String, TranslateError> {
        let key = require_non_empty("Gemini API key", &self.api_key)?;
        let url = self.endpoint("streamGenerateContent", true)?;
        let body = Self::build_request_body(prompts, image_png);
        let response = post_json_checked(
            "Gemini",
            self.client
                .post(url)
                .header("x-goog-api-key", key)
                .json(&body),
        )
        .await?;

        stream_sse_json("Gemini", response, delta_tx, extract_gemini_text).await
    }
}

fn extract_gemini_text(value: &Value) -> Result<Option<String>, TranslateError> {
    if let Some(err_msg) = value.pointer("/error/message").and_then(Value::as_str) {
        return Err(TranslateError::InvalidResponse(format!(
            "Gemini API error: {err_msg}"
        )));
    }

    let Some(parts) = value
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    else {
        if let Some(reason) = value
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
        {
            return Err(TranslateError::InvalidResponse(format!(
                "Gemini blocked prompt: {reason}"
            )));
        }
        return Ok(None);
    };

    let mut text = String::new();
    for part in parts {
        if let Some(piece) = part.get("text").and_then(Value::as_str) {
            text.push_str(piece);
        }
    }
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

impl Translator for GeminiProvider {
    fn provider_name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(async move {
            translate_with_llm_batch(
                PROVIDER_NAME,
                request,
                self.custom_prompt.as_deref(),
                |prompts| async move { self.generate_content(&prompts, None).await },
            )
            .await
        })
    }
}

impl VisionAiProvider for GeminiProvider {
    fn provider_name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn ask<'a>(
        &'a self,
        request: &'a VisionAiRequest,
    ) -> BoxFuture<'a, Result<VisionAiResponse, TranslateError>> {
        Box::pin(async move {
            let prompts = build_vision_prompts(request, self.custom_prompt.as_deref())?;
            let answer = self
                .generate_content(&prompts, request.image_png.as_deref())
                .await?;
            Ok(VisionAiResponse {
                answer,
                provider_name: PROVIDER_NAME.to_owned(),
            })
        })
    }

    fn ask_stream<'a>(
        &'a self,
        request: &'a VisionAiRequest,
        delta_tx: UnboundedSender<String>,
    ) -> BoxFuture<'a, Result<VisionAiResponse, TranslateError>> {
        Box::pin(async move {
            let prompts = build_vision_prompts(request, self.custom_prompt.as_deref())?;
            let answer = self
                .stream_content(&prompts, request.image_png.as_deref(), &delta_tx)
                .await?;
            Ok(VisionAiResponse {
                answer,
                provider_name: PROVIDER_NAME.to_owned(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn gemini_sends_api_key_in_header_never_query_param_and_translates_batch() {
        let server = MockServer::start().await;
        let response_json = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "text": "Chào buổi sáng\n|||STS_SEP|||\nCảm ơn"
                    }]
                }
            }]
        });

        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-flash:generateContent"))
            .and(header("x-goog-api-key", "AIza-test-secret"))
            .and(body_string_contains("Giữ xưng hô tự nhiên"))
            .and(body_string_contains("|||STS_SEP|||"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
            .expect(1)
            .mount(&server)
            .await;

        let provider = GeminiProvider::with_base_url(
            server.uri(),
            "AIza-test-secret",
            "gemini-2.5-flash",
            Some("Giữ xưng hô tự nhiên, không dịch tên riêng".into()),
        );
        let req = TranslateRequest::new("ja", "vi", vec!["おはよう".into(), "ありがとう".into()]);
        let res = match provider.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Gemini translate to succeed: {err}"),
        };

        assert_eq!(res.provider_name, "gemini");
        assert_eq!(res.translations, vec!["Chào buổi sáng", "Cảm ơn"]);

        let received = server.received_requests().await.unwrap_or_default();
        assert_eq!(received.len(), 1);
        assert!(received[0].url.query().is_none());
        assert!(!received[0].url.as_str().contains("AIza-test-secret"));
    }

    #[tokio::test]
    async fn gemini_ask_non_streaming_uses_generate_content_with_inline_data() {
        let server = MockServer::start().await;
        let png = vec![0x89, 0x50, 0x4E, 0x47];
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-flash:generateContent"))
            .and(header("x-goog-api-key", "AIza-ask-key"))
            .and(body_string_contains("inline_data"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "candidates": [{
                    "content": { "parts": [{ "text": "Đây là khung thoại manga." }] }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider =
            GeminiProvider::with_base_url(server.uri(), "AIza-ask-key", "gemini-2.5-flash", None);
        let req = VisionAiRequest {
            image_png: Some(png),
            ocr_text: None,
            prompt: "Giải thích".into(),
        };
        let res = match provider.ask(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Gemini ask to succeed: {err}"),
        };
        assert_eq!(res.provider_name, "gemini");
        assert_eq!(res.answer, "Đây là khung thoại manga.");
    }

    #[tokio::test]
    async fn gemini_ask_stream_sends_inline_data_png_and_streams_sse_vietnamese() {
        let server = MockServer::start().await;
        let png_bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        let encoded_png = STANDARD.encode(&png_bytes);

        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Tiếng Việt — \"}]}}]}\n\n",
            ": keepalive comment\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Truyện tranh\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n"
        );

        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-flash:streamGenerateContent"))
            .and(query_param("alt", "sse"))
            .and(header("x-goog-api-key", "AIza-stream-secret"))
            .and(body_string_contains("inline_data"))
            .and(body_string_contains("image/png"))
            .and(body_string_contains(&encoded_png))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = GeminiProvider::with_base_url(
            server.uri(),
            "AIza-stream-secret",
            "gemini-2.5-flash",
            None,
        );
        let req = VisionAiRequest {
            image_png: Some(png_bytes),
            ocr_text: Some("漫画".into()),
            prompt: "Giải thích khung thoại này".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let res = match provider.ask_stream(&req, tx).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Gemini ask_stream to succeed: {err}"),
        };

        assert_eq!(res.provider_name, "gemini");
        assert_eq!(res.answer, "Tiếng Việt — Truyện tranh");

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        assert_eq!(chunks, vec!["Tiếng Việt — ", "Truyện tranh"]);

        let received = server.received_requests().await.unwrap_or_default();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].url.query(), Some("alt=sse"));
        assert!(!received[0].url.as_str().contains("AIza-stream-secret"));
    }

    #[tokio::test]
    async fn gemini_maps_401_403_to_auth_failed_and_429_to_rate_limited() {
        for (status, is_auth) in [(401, true), (403, true), (429, false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string("error"))
                .mount(&server)
                .await;

            let provider =
                GeminiProvider::with_base_url(server.uri(), "key", "gemini-2.5-flash", None);
            let req = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
            let err = provider.translate(&req).await;
            if is_auth {
                assert!(
                    matches!(err, Err(TranslateError::AuthFailed(_))),
                    "expected AuthFailed for HTTP {status}, got {err:?}"
                );
            } else {
                assert!(
                    matches!(err, Err(TranslateError::RateLimited(_))),
                    "expected RateLimited for HTTP {status}, got {err:?}"
                );
            }

            let ai_req = VisionAiRequest {
                image_png: None,
                ocr_text: None,
                prompt: "Hello".into(),
            };
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let stream_err = provider.ask_stream(&ai_req, tx).await;
            if is_auth {
                assert!(matches!(stream_err, Err(TranslateError::AuthFailed(_))));
            } else {
                assert!(matches!(stream_err, Err(TranslateError::RateLimited(_))));
            }
        }
    }

    #[test]
    fn gemini_debug_redacts_api_key() {
        let provider = GeminiProvider::new("AIza-super-secret", "gemini-2.5-flash", None);
        let dbg = format!("{provider:?}");
        assert!(!dbg.contains("AIza-super-secret"));
        assert!(dbg.contains("[REDACTED]"));
    }
}
