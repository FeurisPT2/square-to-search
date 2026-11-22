//! Free Bing Translator web endpoint (`/translator` token + `/ttranslatev3`) fallback translator.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::RwLock;
use url::{Url, form_urlencoded};

use crate::batch::{pack_batch, unpack_batch};
use crate::{
    BoxFuture, TranslateError, TranslateRequest, TranslateResponse, Translator,
    build_default_http_client, looks_like_captcha_or_html, summarize_body, validate_languages,
};

/// Default base URL for Bing Translator's public web endpoint.
pub const DEFAULT_BASE_URL: &str = "https://www.bing.com";

const DEFAULT_TOKEN_TTL_MS: u64 = 3_600_000;
const TOKEN_REFRESH_MARGIN_MS: u64 = 30_000;
const MIN_TOKEN_TTL_MS: u64 = 5_000;

#[derive(Debug, Clone)]
struct BingAuthToken {
    ig: String,
    key: String,
    token: String,
    expires_at: Instant,
}

enum BingPostOutcome {
    Translated(String),
    TokenExpired,
}

/// Translator backed by Bing Translator's free web endpoint (`/ttranslatev3`).
#[derive(Debug, Clone)]
pub struct BingWebTranslator {
    base_url: String,
    client: reqwest::Client,
    auth: Arc<RwLock<Option<BingAuthToken>>>,
}

impl Default for BingWebTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl BingWebTranslator {
    /// Creates a new [`BingWebTranslator`] targeting [`DEFAULT_BASE_URL`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Creates a new [`BingWebTranslator`] targeting a custom `base_url`.
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self::with_client(base_url, build_default_http_client())
    }

    /// Creates a new [`BingWebTranslator`] with a caller-supplied [`reqwest::Client`].
    #[must_use]
    pub fn with_client(base_url: impl Into<String>, client: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            client,
            auth: Arc::new(RwLock::new(None)),
        }
    }

    /// Returns the configured base URL without trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn get_or_fetch_token(&self) -> Result<Option<BingAuthToken>, TranslateError> {
        {
            let guard = self.auth.read().await;
            if let Some(cached) = guard.as_ref()
                && Instant::now() < cached.expires_at
            {
                return Ok(Some(cached.clone()));
            }
        }

        let endpoint = Url::parse(&format!("{}/translator", self.base_url)).map_err(|err| {
            TranslateError::InvalidResponse(format!("invalid Bing base URL: {err}"))
        })?;
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
                "Bing /translator returned HTTP 429".into(),
            ));
        }

        // Allow local/test endpoints that only stub `/ttranslatev3` without `/translator`.
        if status == StatusCode::NOT_FOUND {
            tracing::debug!("Bing /translator returned 404; proceeding directly to /ttranslatev3");
            return Ok(None);
        }

        if !status.is_success() {
            if looks_like_captcha_or_html(content_type.as_deref(), &body) {
                return Err(TranslateError::RateLimited(format!(
                    "Bing /translator returned HTTP {status} CAPTCHA/interstitial"
                )));
            }
            return Err(TranslateError::InvalidResponse(format!(
                "Bing /translator returned HTTP {status}: {}",
                summarize_body(&body)
            )));
        }

        if let Some(parsed) = parse_bing_tokens(&body) {
            let mut guard = self.auth.write().await;
            *guard = Some(parsed.clone());
            return Ok(Some(parsed));
        }

        if body.to_ascii_lowercase().contains("captcha") {
            return Err(TranslateError::RateLimited(
                "Bing /translator returned CAPTCHA challenge".into(),
            ));
        }

        Err(TranslateError::InvalidResponse(
            "unable to extract Bing translator abuse-prevention token".into(),
        ))
    }

    async fn invalidate_token(&self) {
        let mut guard = self.auth.write().await;
        *guard = None;
    }

    async fn post_translate(
        &self,
        payload: &str,
        from_lang: &str,
        to_lang: &str,
        auth: Option<&BingAuthToken>,
    ) -> Result<BingPostOutcome, TranslateError> {
        let mut endpoint =
            Url::parse(&format!("{}/ttranslatev3", self.base_url)).map_err(|err| {
                TranslateError::InvalidResponse(format!("invalid Bing base URL: {err}"))
            })?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("isVertical", "1");
            if let Some(tok) = auth {
                query.append_pair("IG", &tok.ig);
                query.append_pair("IID", "translator.5028");
            }
        }

        let form_body = {
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            serializer
                .append_pair("fromLang", from_lang)
                .append_pair("text", payload)
                .append_pair("to", to_lang);
            if let Some(tok) = auth {
                serializer
                    .append_pair("token", &tok.token)
                    .append_pair("key", &tok.key);
            }
            serializer.finish()
        };

        let response = self
            .client
            .post(endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ORIGIN, &self.base_url)
            .header(
                reqwest::header::REFERER,
                format!("{}/translator", self.base_url),
            )
            .body(form_body)
            .send()
            .await?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.text().await?;

        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(TranslateError::RateLimited(
                "Bing /ttranslatev3 returned HTTP 429".into(),
            ));
        }

        if !status.is_success() {
            if looks_like_captcha_or_html(content_type.as_deref(), &body) {
                return Err(TranslateError::RateLimited(format!(
                    "Bing /ttranslatev3 returned HTTP {status} HTML/CAPTCHA interstitial"
                )));
            }
            return Err(TranslateError::InvalidResponse(format!(
                "Bing /ttranslatev3 returned HTTP {status}: {}",
                summarize_body(&body)
            )));
        }

        let json: Value = match serde_json::from_str(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                if looks_like_captcha_or_html(content_type.as_deref(), &body) {
                    return Err(TranslateError::RateLimited(
                        "Bing /ttranslatev3 returned HTML/CAPTCHA page instead of JSON".into(),
                    ));
                }
                return Err(TranslateError::InvalidResponse(format!(
                    "Bing /ttranslatev3 returned malformed JSON ({err}): {}",
                    summarize_body(&body)
                )));
            }
        };

        if let Some(obj) = json.as_object() {
            let show_captcha = obj
                .get("ShowCaptcha")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let status_code = obj.get("statusCode").and_then(Value::as_u64);
            if show_captcha || status_code == Some(429) {
                return Err(TranslateError::RateLimited(
                    "Bing /ttranslatev3 requested CAPTCHA or rate limited".into(),
                ));
            }
            if status_code == Some(205) || status_code == Some(401) {
                return Ok(BingPostOutcome::TokenExpired);
            }
            return Err(TranslateError::InvalidResponse(format!(
                "Bing /ttranslatev3 returned error payload: {}",
                summarize_body(&body)
            )));
        }

        let translated = json
            .as_array()
            .and_then(|root| root.first())
            .and_then(|entry| entry.get("translations"))
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                TranslateError::InvalidResponse(
                    "Bing /ttranslatev3 response missing translations[0].text".into(),
                )
            })?;

        Ok(BingPostOutcome::Translated(translated.to_owned()))
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
        let from_lang = map_bing_lang(request.source_lang.trim(), true);
        let to_lang = map_bing_lang(request.target_lang.trim(), false);

        let token = self.get_or_fetch_token().await?;
        let translated = match self
            .post_translate(&packed.payload, from_lang, to_lang, token.as_ref())
            .await?
        {
            BingPostOutcome::Translated(text) => text,
            BingPostOutcome::TokenExpired => {
                self.invalidate_token().await;
                let refreshed = self.get_or_fetch_token().await?;
                match self
                    .post_translate(&packed.payload, from_lang, to_lang, refreshed.as_ref())
                    .await?
                {
                    BingPostOutcome::Translated(text) => text,
                    BingPostOutcome::TokenExpired => {
                        return Err(TranslateError::RateLimited(
                            "Bing /ttranslatev3 rejected refreshed abuse-prevention token".into(),
                        ));
                    }
                }
            }
        };

        let translations = unpack_batch(&translated, &packed.delimiter, request.texts.len())?;

        Ok(TranslateResponse {
            translations,
            provider_name: self.provider_name().to_owned(),
        })
    }
}

fn map_bing_lang(lang: &str, is_source: bool) -> &str {
    if is_source && lang.eq_ignore_ascii_case("auto") {
        return "auto-detect";
    }
    if lang.eq_ignore_ascii_case("zh-cn") {
        return "zh-Hans";
    }
    if lang.eq_ignore_ascii_case("zh-tw") {
        return "zh-Hant";
    }
    lang
}

fn parse_bing_tokens(html: &str) -> Option<BingAuthToken> {
    let ig = extract_ig(html)?;
    let (key, token, ttl_ms) = extract_helper_params(html)?;
    let effective_ttl = ttl_ms
        .saturating_sub(TOKEN_REFRESH_MARGIN_MS)
        .max(MIN_TOKEN_TTL_MS);

    Some(BingAuthToken {
        ig,
        key,
        token,
        expires_at: Instant::now() + Duration::from_millis(effective_ttl),
    })
}

fn extract_ig(html: &str) -> Option<String> {
    for needle in ["IG:\"", "IG: \""] {
        if let Some(pos) = html.find(needle) {
            let rest = &html[pos + needle.len()..];
            if let Some(end) = rest.find('"') {
                let candidate = rest[..end].trim();
                if !candidate.is_empty() {
                    return Some(candidate.to_owned());
                }
            }
        }
    }
    None
}

fn extract_helper_params(html: &str) -> Option<(String, String, u64)> {
    for marker in ["params_AbusePreventionHelper", "params_RichTranslateHelper"] {
        if let Some(marker_pos) = html.find(marker) {
            let after_marker = &html[marker_pos + marker.len()..];
            let open = after_marker.find('[')?;
            let after_open = &after_marker[open..];
            let close = after_open.find(']')?;
            let array_slice = &after_open[..=close];

            let parsed: Value = serde_json::from_str(array_slice).ok()?;
            let arr = parsed.as_array()?;
            let key = match arr.first()? {
                Value::Number(num) => num.to_string(),
                Value::String(s) if !s.is_empty() => s.clone(),
                _ => return None,
            };
            let token = arr.get(1)?.as_str()?.trim();
            if token.is_empty() {
                return None;
            }
            let ttl_ms = arr
                .get(2)
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TOKEN_TTL_MS);
            return Some((key, token.to_owned(), ttl_ms));
        }
    }
    None
}

impl Translator for BingWebTranslator {
    fn provider_name(&self) -> &'static str {
        "bing-web"
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
    use wiremock::matchers::{body_string_contains, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const SAMPLE_TRANSLATOR_HTML: &str = r#"<!DOCTYPE html>
<html>
<head><script>
var _G={IG:"ABC123DEF456",EventID:"test"};
var params_AbusePreventionHelper = [1730000000000,"tok_secret_value",3600000];
</script></head>
<body></body>
</html>"#;

    #[tokio::test]
    async fn bing_web_fetches_token_reuses_it_and_translates_batch() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/translator"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(SAMPLE_TRANSLATOR_HTML),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .and(query_param("IG", "ABC123DEF456"))
            .and(body_string_contains("token=tok_secret_value"))
            .and(body_string_contains("key=1730000000000"))
            .and(body_string_contains("fromLang=auto-detect"))
            .and(body_string_contains("to=vi"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "detectedLanguage": { "language": "en", "score": 1.0 },
                    "translations": [
                        { "text": "Xin chào\n|||STS_SEP|||\nThế giới", "to": "vi" }
                    ]
                }
            ])))
            .expect(2)
            .mount(&server)
            .await;

        let translator = BingWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("auto", "vi", vec!["Hello".into(), "World".into()]);

        let first = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected first Bing call to succeed, got error: {err}"),
        };
        assert_eq!(first.provider_name, "bing-web");
        assert_eq!(first.translations, vec!["Xin chào", "Thế giới"]);

        // Second call must reuse the cached token (`GET /translator` expectation == 1).
        let second = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected second Bing call to succeed, got error: {err}"),
        };
        assert_eq!(second.translations, vec!["Xin chào", "Thế giới"]);
    }

    #[tokio::test]
    async fn bing_web_maps_http_429_and_show_captcha_to_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let translator = BingWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("en", "vi", vec!["World".into()]);
        assert!(matches!(
            translator.translate(&req).await,
            Err(TranslateError::RateLimited(_))
        ));

        let captcha_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ShowCaptcha": true, "statusCode": 429 })),
            )
            .mount(&captcha_server)
            .await;

        let captcha_translator = BingWebTranslator::with_base_url(captcha_server.uri());
        assert!(matches!(
            captcha_translator.translate(&req).await,
            Err(TranslateError::RateLimited(_))
        ));
    }

    #[tokio::test]
    async fn bing_web_refreshes_expired_token_on_status_205() {
        let server = MockServer::start().await;

        let fresh_html = r#"<html><script>
var _G={IG:"FRESH_IG_999"};
var params_AbusePreventionHelper = [1730000009999,"tok_fresh",3600000];
</script></html>"#;

        Mock::given(method("GET"))
            .and(path("/translator"))
            .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_TRANSLATOR_HTML))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/translator"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fresh_html))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .and(query_param("IG", "ABC123DEF456"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "statusCode": 205 })),
            )
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .and(query_param("IG", "FRESH_IG_999"))
            .and(body_string_contains("token=tok_fresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "translations": [{ "text": "Thành công", "to": "vi" }]
                }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let translator = BingWebTranslator::with_base_url(server.uri());
        let req = TranslateRequest::new("zh-CN", "vi", vec!["成功".into()]);
        let res = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected token refresh retry to succeed, got error: {err}"),
        };
        assert_eq!(res.translations, vec!["Thành công"]);
    }

    #[test]
    fn bing_lang_mapping_normalizes_auto_and_chinese_variants() {
        assert_eq!(map_bing_lang("auto", true), "auto-detect");
        assert_eq!(map_bing_lang("zh-CN", true), "zh-Hans");
        assert_eq!(map_bing_lang("zh-TW", false), "zh-Hant");
        assert_eq!(map_bing_lang("ja", true), "ja");
    }
}
