//! Free Google Translate web endpoint (`translate_a/single`) translator.

use reqwest::StatusCode;
use serde_json::Value;
use url::Url;

use crate::batch::{pack_batch, unpack_batch};
use crate::{
    BoxFuture, TranslateError, TranslateRequest, TranslateResponse, Translator,
    build_default_http_client, looks_like_captcha_or_html, summarize_body, validate_languages,
};

/// Default base URL for the Google Translate public web endpoint.
pub const DEFAULT_BASE_URL: &str = "https://translate.googleapis.com";

/// Translator backed by Google Translate's free web endpoint (`/translate_a/single`).
#[derive(Debug, Clone)]
pub struct GoogleWebTranslator {
    base_url: String,
    client: reqwest::Client,
}

impl Default for GoogleWebTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl GoogleWebTranslator {
    /// Creates a new [`GoogleWebTranslator`] targeting [`DEFAULT_BASE_URL`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Creates a new [`GoogleWebTranslator`] targeting a custom `base_url`.
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_client(base_url, build_default_http_client())
    }

    /// Creates a new [`GoogleWebTranslator`] with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
        }
    }

    /// Returns the configured base URL without trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn translate_inner(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateResponse, TranslateError> {
        validate_languages(&request.source_lang, &request.target_lang)?;

        if request.texts.is_empty() {
            return Ok(TranslateResponse {
                translations: Vec::new(),
                provider_name: self.provider_name().to_owned(),
            });
        }

        let refs: Vec<&str> = request.texts.iter().map(String::as_str).collect();
        let packed = pack_batch(&refs);

        let mut endpoint =
            Url::parse(&format!("{}/translate_a/single", self.base_url)).map_err(|err| {
                TranslateError::InvalidResponse(format!("invalid Google base URL: {err}"))
            })?;
        endpoint
            .query_pairs_mut()
            .append_pair("client", "gtx")
            .append_pair("sl", request.source_lang.trim())
            .append_pair("tl", request.target_lang.trim())
            .append_pair("dt", "t")
            .append_pair("q", &packed.payload);

        let response = self.client.get(endpoint).send().await?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(TranslateError::RateLimited(
                "Google Translate returned HTTP 429".into(),
            ));
        }

        if !status.is_success() {
            if looks_like_captcha_or_html(content_type.as_deref(), &body) {
                return Err(TranslateError::RateLimited(format!(
                    "Google Translate returned HTTP {status} HTML/CAPTCHA interstitial"
                )));
            }
            return Err(TranslateError::InvalidResponse(format!(
                "Google Translate returned HTTP {status}: {}",
                summarize_body(&body)
            )));
        }

        // Parse JSON before checking HTML/CAPTCHA keywords so translated text mentioning "captcha"
        // is never misclassified as an anti-bot challenge.
        let json: Value = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                if looks_like_captcha_or_html(content_type.as_deref(), &body) {
                    return Err(TranslateError::RateLimited(
                        "Google Translate returned HTML/CAPTCHA page instead of JSON".into(),
                    ));
                }
                return Err(TranslateError::InvalidResponse(format!(
                    "Google Translate returned malformed JSON ({err}): {}",
                    summarize_body(&body)
                )));
            }
        };

        let joined = parse_google_segments(&json)?;
        let translations = unpack_batch(&joined, &packed.delimiter, request.texts.len())?;

        Ok(TranslateResponse {
            translations,
            provider_name: self.provider_name().to_owned(),
        })
    }
}

fn parse_google_segments(value: &Value) -> Result<String, TranslateError> {
    let segments = value
        .as_array()
        .and_then(|root| root.first())
        .and_then(Value::as_array)
        .ok_or_else(|| {
            TranslateError::InvalidResponse(
                "Google Translate response missing top-level segments array".into(),
            )
        })?;

    let mut joined = String::new();
    let mut extracted_any = false;
    for segment in segments {
        if let Some(piece) = segment
            .as_array()
            .and_then(|parts| parts.first())
            .and_then(Value::as_str)
        {
            joined.push_str(piece);
            extracted_any = true;
        }
    }

    if !segments.is_empty() && !extracted_any {
        return Err(TranslateError::InvalidResponse(
            "Google Translate response segments contained no translated text".into(),
        ));
    }

    Ok(joined)
}

impl Translator for GoogleWebTranslator {
    fn provider_name(&self) -> &'static str {
        "google-web"
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(self.translate_inner(request))
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn google_web_parses_multi_segment_200_json_and_preserves_order() {
        let server = MockServer::start().await;
        let body = serde_json::json!([
            [
                ["Xin chào\n", "Hello\n", null, null, 10],
                ["|||STS_SEP|||\n", "|||STS_SEP|||\n", null, null, 0],
                ["Thế giới", "World", null, null, 10]
            ],
            null,
            "en"
        ]);

        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .and(query_param("client", "gtx"))
            .and(query_param("sl", "en"))
            .and(query_param("tl", "vi"))
            .and(query_param("dt", "t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;

        let translator = GoogleWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("en", "vi", vec!["Hello".into(), "World".into()]);
        let res = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Google 200 translation to succeed, got error: {err}"),
        };

        assert_eq!(res.provider_name, "google-web");
        assert_eq!(res.translations, vec!["Xin chào", "Thế giới"]);
    }

    #[tokio::test]
    async fn google_web_maps_http_429_to_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let translator = GoogleWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
        let res = translator.translate(&req).await;
        assert!(matches!(res, Err(TranslateError::RateLimited(_))));
    }

    #[tokio::test]
    async fn google_web_maps_html_captcha_200_to_rate_limited() {
        let server = MockServer::start().await;
        let captcha_html = "<!DOCTYPE html><html><body>Our systems have detected unusual traffic. Please solve the CAPTCHA.</body></html>";
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string(captcha_html),
            )
            .mount(&server)
            .await;

        let translator = GoogleWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
        let res = translator.translate(&req).await;
        assert!(matches!(res, Err(TranslateError::RateLimited(_))));
    }

    #[tokio::test]
    async fn google_web_allows_valid_json_containing_word_captcha() {
        let server = MockServer::start().await;
        let body = serde_json::json!([
            [[
                "Vui lòng nhập captcha",
                "Please enter captcha",
                null,
                null,
                10
            ]],
            null,
            "en"
        ]);
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let translator = GoogleWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("en", "vi", vec!["Please enter captcha".into()]);
        let res = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("valid JSON mentioning captcha should succeed, got error: {err}"),
        };
        assert_eq!(res.translations, vec!["Vui lòng nhập captcha"]);
    }

    #[tokio::test]
    async fn google_web_rejects_invalid_languages_without_network_call() {
        let server = MockServer::start().await;
        let translator = GoogleWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("en", "auto", vec!["Hello".into()]);
        let res = translator.translate(&req).await;
        assert!(matches!(res, Err(TranslateError::UnsupportedLanguage(_))));
    }
}
