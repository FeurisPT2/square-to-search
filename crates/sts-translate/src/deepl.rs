//! DeepL BYOK translator supporting Free and Pro endpoints.

use serde_json::{Map, Value};
use url::Url;

use crate::{
    BoxFuture, TranslateError, TranslateRequest, TranslateResponse, Translator,
    build_default_http_client, normalize_base_url, post_json_for_value, require_non_empty,
    validate_languages,
};

/// Default base URL for DeepL Free API plans.
pub const FREE_BASE_URL: &str = "https://api-free.deepl.com";

/// Default base URL for DeepL Pro API plans.
pub const PRO_BASE_URL: &str = "https://api.deepl.com";

/// DeepL API translator (`POST /v2/translate` with `Authorization: DeepL-Auth-Key ...`).
#[derive(Clone)]
pub struct DeepLTranslator {
    base_url: String,
    api_key: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for DeepLTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepLTranslator")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl DeepLTranslator {
    /// Creates a new [`DeepLTranslator`] using [`FREE_BASE_URL`] or [`PRO_BASE_URL`].
    #[must_use]
    pub fn new(api_key: impl Into<String>, free_tier: bool) -> Self {
        let base_url = if free_tier {
            FREE_BASE_URL
        } else {
            PRO_BASE_URL
        };
        Self::with_base_url(base_url, api_key)
    }

    /// Creates a new [`DeepLTranslator`] targeting a custom `base_url`.
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_client(base_url, api_key, build_default_http_client())
    }

    /// Creates a new [`DeepLTranslator`] with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            base_url: normalize_base_url(&base_url.into(), FREE_BASE_URL),
            api_key: api_key.into().trim().to_owned(),
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

        let key = require_non_empty("DeepL API key", &self.api_key)?;
        let url = Url::parse(&format!("{}/v2/translate", self.base_url)).map_err(|err| {
            TranslateError::InvalidResponse(format!("invalid DeepL base URL: {err}"))
        })?;

        let mut payload = Map::new();
        payload.insert(
            "text".into(),
            Value::Array(request.texts.iter().cloned().map(Value::String).collect()),
        );
        payload.insert(
            "target_lang".into(),
            Value::String(map_deepl_target_lang(&request.target_lang)),
        );
        if let Some(src) = map_deepl_source_lang(&request.source_lang) {
            payload.insert("source_lang".into(), Value::String(src));
        }

        let value = post_json_for_value(
            "DeepL",
            self.client
                .post(url)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("DeepL-Auth-Key {key}"),
                )
                .json(&Value::Object(payload)),
        )
        .await?;

        let items = value
            .get("translations")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                TranslateError::InvalidResponse(
                    "DeepL response missing `translations` array".into(),
                )
            })?;

        let translations: Vec<String> = items
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        TranslateError::InvalidResponse(
                            "DeepL response entry missing `text` field".into(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        if translations.len() != request.texts.len() {
            return Err(TranslateError::InvalidResponse(format!(
                "DeepL returned {} translations for {} inputs",
                translations.len(),
                request.texts.len()
            )));
        }

        Ok(TranslateResponse {
            translations,
            provider_name: self.provider_name().to_owned(),
        })
    }
}

fn map_deepl_source_lang(lang: &str) -> Option<String> {
    let trimmed = lang.trim();
    if trimmed.eq_ignore_ascii_case("auto") || trimmed.eq_ignore_ascii_case("auto-detect") {
        return None;
    }
    if is_chinese_code(trimmed) {
        return Some("ZH".to_owned());
    }
    if trimmed.eq_ignore_ascii_case("en-us") || trimmed.eq_ignore_ascii_case("en-gb") {
        return Some("EN".to_owned());
    }
    Some(trimmed.to_ascii_uppercase())
}

fn map_deepl_target_lang(lang: &str) -> String {
    let trimmed = lang.trim();
    if is_chinese_code(trimmed) {
        return "ZH".to_owned();
    }
    trimmed.to_ascii_uppercase()
}

fn is_chinese_code(lang: &str) -> bool {
    lang.eq_ignore_ascii_case("zh")
        || lang.eq_ignore_ascii_case("zh-cn")
        || lang.eq_ignore_ascii_case("zh-tw")
        || lang.eq_ignore_ascii_case("zh-hans")
        || lang.eq_ignore_ascii_case("zh-hant")
}

impl Translator for DeepLTranslator {
    fn provider_name(&self) -> &'static str {
        "deepl"
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
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[test]
    fn deepl_selects_free_vs_pro_urls_and_custom_base_url() {
        let free = DeepLTranslator::new("dl-key", true);
        assert_eq!(free.base_url(), FREE_BASE_URL);

        let pro = DeepLTranslator::new("dl-key", false);
        assert_eq!(pro.base_url(), PRO_BASE_URL);

        let custom = DeepLTranslator::with_base_url("  http://localhost:8080/  ", "dl-key");
        assert_eq!(custom.base_url(), "http://localhost:8080");
    }

    #[tokio::test]
    async fn deepl_sends_auth_header_and_native_text_array_preserving_order() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v2/translate"))
            .and(header("authorization", "DeepL-Auth-Key dl-secret-123"))
            .and(body_json(serde_json::json!({
                "text": ["Hello", "World"],
                "source_lang": "EN",
                "target_lang": "VI"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "translations": [
                    { "detected_source_language": "EN", "text": "Xin chào" },
                    { "detected_source_language": "EN", "text": "Thế giới" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let translator = DeepLTranslator::with_base_url(server.uri(), "dl-secret-123");
        let req = TranslateRequest::new("en", "vi", vec!["Hello".into(), "World".into()]);
        let res = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected DeepL translate to succeed: {err}"),
        };
        assert_eq!(res.provider_name, "deepl");
        assert_eq!(res.translations, vec!["Xin chào", "Thế giới"]);
    }

    #[tokio::test]
    async fn deepl_omits_source_lang_on_auto_and_normalizes_chinese() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v2/translate"))
            .and(body_json(serde_json::json!({
                "text": ["你好"],
                "target_lang": "EN-US"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "translations": [{ "text": "Hello" }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v2/translate"))
            .and(body_json(serde_json::json!({
                "text": ["世界"],
                "source_lang": "ZH",
                "target_lang": "VI"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "translations": [{ "text": "Thế giới" }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let translator = DeepLTranslator::with_base_url(server.uri(), "dl-key");
        let auto_res = match translator
            .translate(&TranslateRequest::new("auto", "en-US", vec!["你好".into()]))
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected auto DeepL translate to succeed: {err}"),
        };
        assert_eq!(auto_res.translations, vec!["Hello"]);

        let zh_res = match translator
            .translate(&TranslateRequest::new("zh-CN", "vi", vec!["世界".into()]))
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected zh-CN DeepL translate to succeed: {err}"),
        };
        assert_eq!(zh_res.translations, vec!["Thế giới"]);
    }

    #[tokio::test]
    async fn deepl_maps_403_to_auth_failed_and_429_456_to_rate_limited() {
        for (status, is_auth) in [(403, true), (429, false), (456, false)] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v2/translate"))
                .respond_with(ResponseTemplate::new(status).set_body_string("error"))
                .mount(&server)
                .await;

            let translator = DeepLTranslator::with_base_url(server.uri(), "dl-key");
            let req = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
            let res = translator.translate(&req).await;
            if is_auth {
                assert!(matches!(res, Err(TranslateError::AuthFailed(_))));
            } else {
                assert!(matches!(res, Err(TranslateError::RateLimited(_))));
            }
        }
    }

    #[test]
    fn deepl_debug_redacts_api_key() {
        let translator = DeepLTranslator::new("dl-super-secret", true);
        let dbg = format!("{translator:?}");
        assert!(!dbg.contains("dl-super-secret"));
        assert!(dbg.contains("[REDACTED]"));
    }
}
