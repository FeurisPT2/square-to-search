//! OpenAI-compatible BYOK translator and multimodal SSE streaming Vision AI provider.

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

/// Default base URL for the OpenAI API.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

const DEFAULT_MODEL: &str = "gpt-4o-mini";
const PROVIDER_NAME: &str = "openai";

/// OpenAI-compatible provider implementing [`Translator`] and SSE-streaming [`VisionAiProvider`].
#[derive(Clone)]
pub struct OpenAiCompatProvider {
    base_url: String,
    api_key: String,
    model: String,
    custom_prompt: Option<String>,
    client: reqwest::Client,
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("custom_prompt", &self.custom_prompt.is_some())
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatProvider {
    /// Creates a new [`OpenAiCompatProvider`] normalizing trailing `/` on `base_url`.
    #[must_use]
    pub fn new(
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

    /// Creates a new [`OpenAiCompatProvider`] with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        custom_prompt: Option<String>,
        client: reqwest::Client,
    ) -> Self {
        let model_raw = model.into();
        let model_trimmed = model_raw.trim();
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

    fn chat_completions_url(&self) -> Result<Url, TranslateError> {
        Url::parse(&format!("{}/chat/completions", self.base_url)).map_err(|err| {
            TranslateError::InvalidResponse(format!("invalid OpenAI base URL: {err}"))
        })
    }

    fn build_request_body(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
        stream: bool,
    ) -> Value {
        let user_content = if let Some(png_bytes) = image_png {
            let data_url = format!("data:image/png;base64,{}", STANDARD.encode(png_bytes));
            json!([
                { "type": "text", "text": prompts.user },
                { "type": "image_url", "image_url": { "url": data_url } }
            ])
        } else {
            Value::String(prompts.user.clone())
        };

        json!({
            "model": self.model,
            "stream": stream,
            "messages": [
                { "role": "system", "content": prompts.system },
                { "role": "user", "content": user_content }
            ]
        })
    }

    async fn complete_chat(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
    ) -> Result<String, TranslateError> {
        let key = require_non_empty("OpenAI API key", &self.api_key)?;
        let url = self.chat_completions_url()?;
        let body = self.build_request_body(prompts, image_png, false);
        let value =
            post_json_for_value("OpenAI", self.client.post(url).bearer_auth(key).json(&body))
                .await?;
        extract_openai_message_content(&value)
    }

    async fn stream_chat(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
        delta_tx: &UnboundedSender<String>,
    ) -> Result<String, TranslateError> {
        let key = require_non_empty("OpenAI API key", &self.api_key)?;
        let url = self.chat_completions_url()?;
        let body = self.build_request_body(prompts, image_png, true);
        let response =
            post_json_checked("OpenAI", self.client.post(url).bearer_auth(key).json(&body)).await?;

        stream_sse_json("OpenAI", response, delta_tx, extract_openai_delta_content).await
    }
}

fn check_openai_error(value: &Value) -> Result<(), TranslateError> {
    if let Some(err_msg) = value.pointer("/error/message").and_then(Value::as_str) {
        return Err(TranslateError::InvalidResponse(format!(
            "OpenAI API error: {err_msg}"
        )));
    }
    Ok(())
}

fn extract_openai_message_content(value: &Value) -> Result<String, TranslateError> {
    check_openai_error(value)?;
    match value.pointer("/choices/0/message/content") {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(parts)) => {
            let mut joined = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    joined.push_str(text);
                }
            }
            if joined.is_empty() {
                Err(TranslateError::InvalidResponse(
                    "OpenAI response choices[0].message.content array contained no text".into(),
                ))
            } else {
                Ok(joined)
            }
        }
        _ => Err(TranslateError::InvalidResponse(
            "OpenAI response missing choices[0].message.content".into(),
        )),
    }
}

fn extract_openai_delta_content(value: &Value) -> Result<Option<String>, TranslateError> {
    check_openai_error(value)?;
    Ok(value
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

impl Translator for OpenAiCompatProvider {
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
                |prompts| async move { self.complete_chat(&prompts, None).await },
            )
            .await
        })
    }
}

impl VisionAiProvider for OpenAiCompatProvider {
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
                .complete_chat(&prompts, request.image_png.as_deref())
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
                .stream_chat(&prompts, request.image_png.as_deref(), &delta_tx)
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn openai_trims_base_url_and_sends_bearer_auth_on_translate() {
        let server = MockServer::start().await;
        let response_json = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Xin chào\n|||STS_SEP|||\nThế giới"
                }
            }]
        });

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test-openai-key"))
            .and(body_string_contains("\"stream\":false"))
            .and(body_string_contains("Giữ văn phong manga"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
            .expect(1)
            .mount(&server)
            .await;

        let padded_base = format!("  {}/v1///  ", server.uri());
        let provider = OpenAiCompatProvider::new(
            padded_base,
            "sk-test-openai-key",
            "gpt-4o-mini",
            Some("Giữ văn phong manga".into()),
        );
        assert_eq!(provider.base_url(), format!("{}/v1", server.uri()));

        let req = TranslateRequest::new("en", "vi", vec!["Hello".into(), "World".into()]);
        let res = match provider.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected OpenAI translate to succeed: {err}"),
        };
        assert_eq!(res.provider_name, "openai");
        assert_eq!(res.translations, vec!["Xin chào", "Thế giới"]);
    }

    #[tokio::test]
    async fn openai_ask_non_streaming_returns_complete_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains("\"stream\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{ "message": { "content": "Tóm tắt ngắn gọn." } }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            format!("{}/v1", server.uri()),
            "sk-key",
            "gpt-4o-mini",
            None,
        );
        let req = VisionAiRequest {
            image_png: None,
            ocr_text: Some("Text".into()),
            prompt: "Tóm tắt".into(),
        };
        let res = match provider.ask(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected OpenAI ask to succeed: {err}"),
        };
        assert_eq!(res.answer, "Tóm tắt ngắn gọn.");
    }

    #[tokio::test]
    async fn openai_ask_stream_sends_image_data_url_parses_deltas_and_stops_on_done() {
        let server = MockServer::start().await;
        let png = vec![0x89, 0x50, 0x4E, 0x47, 0x01, 0x02];
        let expected_data_url = format!("data:image/png;base64,{}", STANDARD.encode(&png));

        let sse_body = concat!(
            ": keepalive\n\n",
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Tiếng Việt — \"}}]}\n\n",
            "data: {\"choices\":[]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Truyện tranh\"}}]}\n\n",
            "data: [DONE]\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"TRAILING_IGNORED\"}}]}\n\n"
        );

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-stream-key"))
            .and(body_string_contains("\"stream\":true"))
            .and(body_string_contains(&expected_data_url))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            format!("{}/v1/", server.uri()),
            "sk-stream-key",
            "gpt-4o-mini",
            None,
        );
        let req = VisionAiRequest {
            image_png: Some(png),
            ocr_text: None,
            prompt: "Tóm tắt cảnh này".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let res = match provider.ask_stream(&req, tx).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected OpenAI ask_stream to succeed: {err}"),
        };

        assert_eq!(res.provider_name, "openai");
        assert_eq!(res.answer, "Tiếng Việt — Truyện tranh");

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        assert_eq!(chunks, vec!["Tiếng Việt — ", "Truyện tranh"]);
    }

    #[tokio::test]
    async fn openai_ask_stream_handles_tcp_chunks_splitting_multibyte_vietnamese_utf8() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(err) => panic!("failed to bind TCP listener: {err}"),
        };
        let addr = match listener.local_addr() {
            Ok(a) => a,
            Err(err) => panic!("failed to read local addr: {err}"),
        };

        let server_task = tokio::spawn(async move {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(err) => panic!("accept failed: {err}"),
            };
            let mut req_buf = [0u8; 4096];
            let _ = socket.read(&mut req_buf).await;

            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
            let _ = socket.write_all(headers.as_bytes()).await;

            let sse_payload = b"data: {\"choices\":[{\"delta\":{\"content\":\"Ti\xe1\xba\xbfng Vi\xe1\xbb\x87t \xe2\x80\x94 Truy\xe1\xbb\x87n tranh\"}}]}\n\ndata: [DONE]\n\n";
            // Send 3-byte slices deliberately cutting inside multi-byte UTF-8 sequences!
            for slice in sse_payload.chunks(3) {
                let frame = format!("{:X}\r\n", slice.len());
                let _ = socket.write_all(frame.as_bytes()).await;
                let _ = socket.write_all(slice).await;
                let _ = socket.write_all(b"\r\n").await;
                let _ = socket.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            let _ = socket.flush().await;
        });

        let provider = OpenAiCompatProvider::new(
            format!("http://{addr}/v1"),
            "sk-tcp-test",
            "gpt-4o-mini",
            None,
        );
        let req = VisionAiRequest {
            image_png: None,
            ocr_text: None,
            prompt: "Test UTF-8".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let res = match provider.ask_stream(&req, tx).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected chunked TCP SSE stream to succeed: {err}"),
        };
        let _ = server_task.await;

        assert_eq!(res.answer, "Tiếng Việt — Truyện tranh");
        assert!(!res.answer.contains('\u{FFFD}'));
        assert_eq!(
            rx.recv().await.as_deref(),
            Some("Tiếng Việt — Truyện tranh")
        );
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn openai_ask_stream_rejects_corrupt_json_chunk() {
        let server = MockServer::start().await;
        let corrupt_sse =
            "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: {corrupt-json\n\n";

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(corrupt_sse, "text/event-stream"))
            .mount(&server)
            .await;

        let provider = OpenAiCompatProvider::new(
            format!("{}/v1", server.uri()),
            "sk-key",
            "gpt-4o-mini",
            None,
        );
        let req = VisionAiRequest {
            image_png: None,
            ocr_text: None,
            prompt: "Test".into(),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let res = provider.ask_stream(&req, tx).await;
        assert!(matches!(res, Err(TranslateError::InvalidResponse(_))));
    }

    #[tokio::test]
    async fn openai_maps_401_403_to_auth_failed_and_429_to_rate_limited() {
        for (status, is_auth) in [(401, true), (403, true), (429, false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string("denied"))
                .mount(&server)
                .await;

            let provider =
                OpenAiCompatProvider::new(format!("{}/v1", server.uri()), "sk-k", "gpt-4o", None);
            let req = TranslateRequest::new("en", "vi", vec!["Hi".into()]);
            let err = provider.translate(&req).await;
            if is_auth {
                assert!(matches!(err, Err(TranslateError::AuthFailed(_))));
            } else {
                assert!(matches!(err, Err(TranslateError::RateLimited(_))));
            }
        }
    }

    #[test]
    fn openai_debug_redacts_api_key() {
        let provider =
            OpenAiCompatProvider::new(DEFAULT_BASE_URL, "sk-super-secret", "gpt-4o", None);
        let dbg = format!("{provider:?}");
        assert!(!dbg.contains("sk-super-secret"));
        assert!(dbg.contains("[REDACTED]"));
    }
}
