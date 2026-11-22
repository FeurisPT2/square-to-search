//! Local Ollama translator and NDJSON streaming Vision AI provider.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::{
    BoxFuture, PromptMessages, TranslateError, TranslateRequest, TranslateResponse, Translator,
    VisionAiProvider, VisionAiRequest, VisionAiResponse, build_ai_http_client,
    build_vision_prompts, normalize_base_url, post_json_checked, post_json_for_value,
    read_line_stream, summarize_body, translate_with_llm_batch,
};

/// Default base URL for a local Ollama daemon.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

const DEFAULT_MODEL: &str = "qwen2.5vl:7b";
const PROVIDER_NAME: &str = "ollama";

/// Local Ollama provider implementing [`Translator`] and NDJSON-streaming [`VisionAiProvider`].
#[derive(Debug, Clone)]
pub struct OllamaProvider {
    base_url: String,
    model: String,
    custom_prompt: Option<String>,
    client: reqwest::Client,
}

impl OllamaProvider {
    /// Creates a new [`OllamaProvider`] targeting `base_url`.
    #[must_use]
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        custom_prompt: Option<String>,
    ) -> Self {
        Self::with_client(base_url, model, custom_prompt, build_ai_http_client())
    }

    /// Creates a new [`OllamaProvider`] with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(
        base_url: impl Into<String>,
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

    fn chat_url(&self) -> Result<Url, TranslateError> {
        Url::parse(&format!("{}/api/chat", self.base_url)).map_err(|err| {
            TranslateError::InvalidResponse(format!("invalid Ollama base URL: {err}"))
        })
    }

    fn build_request_body(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
        stream: bool,
    ) -> Value {
        let user_msg = if let Some(png_bytes) = image_png {
            json!({
                "role": "user",
                "content": prompts.user,
                "images": [STANDARD.encode(png_bytes)]
            })
        } else {
            json!({
                "role": "user",
                "content": prompts.user
            })
        };

        json!({
            "model": self.model,
            "stream": stream,
            "messages": [
                { "role": "system", "content": prompts.system },
                user_msg
            ]
        })
    }

    async fn chat_complete(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
    ) -> Result<String, TranslateError> {
        let url = self.chat_url()?;
        let body = self.build_request_body(prompts, image_png, false);
        let value = post_json_for_value("Ollama", self.client.post(url).json(&body)).await?;
        extract_ollama_content(&value)?.ok_or_else(|| {
            TranslateError::InvalidResponse("Ollama response missing `message.content`".into())
        })
    }

    async fn chat_stream(
        &self,
        prompts: &PromptMessages,
        image_png: Option<&[u8]>,
        delta_tx: &UnboundedSender<String>,
    ) -> Result<String, TranslateError> {
        let url = self.chat_url()?;
        let body = self.build_request_body(prompts, image_png, true);
        let response = post_json_checked("Ollama", self.client.post(url).json(&body)).await?;

        let mut full_answer = String::new();
        read_line_stream(response, |line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return Ok(true);
            }
            let value: Value = serde_json::from_str(trimmed).map_err(|err| {
                TranslateError::InvalidResponse(format!(
                    "Ollama returned malformed NDJSON chunk ({err}): {}",
                    summarize_body(trimmed)
                ))
            })?;
            if let Some(chunk) = extract_ollama_content(&value)?
                && !chunk.is_empty()
            {
                full_answer.push_str(&chunk);
                let _ = delta_tx.send(chunk);
            }
            let done = value.get("done").and_then(Value::as_bool).unwrap_or(false);
            Ok(!done)
        })
        .await?;

        Ok(full_answer)
    }
}

fn extract_ollama_content(value: &Value) -> Result<Option<String>, TranslateError> {
    if let Some(err_msg) = value.get("error").and_then(Value::as_str) {
        return Err(TranslateError::InvalidResponse(format!(
            "Ollama API error: {err_msg}"
        )));
    }
    Ok(value
        .pointer("/message/content")
        .and_then(Value::as_str)
        .map(str::to_owned))
}

impl Translator for OllamaProvider {
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
                |prompts| async move { self.chat_complete(&prompts, None).await },
            )
            .await
        })
    }
}

impl VisionAiProvider for OllamaProvider {
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
                .chat_complete(&prompts, request.image_png.as_deref())
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
                .chat_stream(&prompts, request.image_png.as_deref(), &delta_tx)
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
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn ollama_translate_posts_api_chat_non_streaming_and_unpacks_batch() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_string_contains("\"stream\":false"))
            .and(body_string_contains("\"model\":\"qwen2.5vl:7b\""))
            .and(body_string_contains("|||STS_SEP|||"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "role": "assistant",
                    "content": "Chào\n|||STS_SEP|||\nTạm biệt"
                },
                "done": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(format!("{}/", server.uri()), "qwen2.5vl:7b", None);
        let req = TranslateRequest::new("ja", "vi", vec!["やあ".into(), "じゃあね".into()]);
        let res = match provider.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Ollama translate to succeed: {err}"),
        };
        assert_eq!(res.provider_name, "ollama");
        assert_eq!(res.translations, vec!["Chào", "Tạm biệt"]);
    }

    #[tokio::test]
    async fn ollama_ask_non_streaming_returns_complete_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_string_contains("\"stream\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "role": "assistant", "content": "Giải thích xong." },
                "done": true
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "qwen2.5vl:7b", None);
        let req = VisionAiRequest {
            image_png: None,
            ocr_text: None,
            prompt: "Giải thích".into(),
        };
        let res = match provider.ask(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Ollama ask to succeed: {err}"),
        };
        assert_eq!(res.answer, "Giải thích xong.");
    }

    #[tokio::test]
    async fn ollama_ask_stream_sends_base64_images_and_parses_ndjson_until_done() {
        let server = MockServer::start().await;
        let png = vec![0x89, 0x50, 0x4E, 0x47, 0x0A, 0x0B];
        let encoded = STANDARD.encode(&png);

        let ndjson = concat!(
            "{\"message\":{\"role\":\"assistant\",\"content\":\"Tiếng Việt \"},\"done\":false}\n",
            "\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"— Truyện tranh\"},\"done\":false}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"!\"},\"done\":true}\n",
            "{\"message\":{\"role\":\"assistant\",\"content\":\"IGNORED_AFTER_DONE\"},\"done\":false}\n"
        );

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_string_contains("\"stream\":true"))
            .and(body_string_contains("\"images\""))
            .and(body_string_contains(&encoded))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ndjson, "application/x-ndjson"))
            .expect(1)
            .mount(&server)
            .await;

        let provider = OllamaProvider::new(server.uri(), "qwen2.5vl:7b", None);
        let req = VisionAiRequest {
            image_png: Some(png),
            ocr_text: Some("漫画".into()),
            prompt: "Dịch tự nhiên".into(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let res = match provider.ask_stream(&req, tx).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Ollama ask_stream to succeed: {err}"),
        };

        assert_eq!(res.provider_name, "ollama");
        assert_eq!(res.answer, "Tiếng Việt — Truyện tranh!");

        let mut chunks = Vec::new();
        while let Some(chunk) = rx.recv().await {
            chunks.push(chunk);
        }
        assert_eq!(chunks, vec!["Tiếng Việt ", "— Truyện tranh", "!"]);
    }

    #[tokio::test]
    async fn ollama_maps_network_failure_cleanly() {
        let listener = match std::net::TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => panic!("failed to bind ephemeral port: {err}"),
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(err) => panic!("failed to inspect local addr: {err}"),
        };
        drop(listener);

        let provider =
            OllamaProvider::new(format!("http://127.0.0.1:{port}"), "qwen2.5vl:7b", None);
        let req = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
        let err = provider.translate(&req).await;
        assert!(matches!(err, Err(TranslateError::Network(_))));
    }
}
