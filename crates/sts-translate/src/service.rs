//! LRU-cached translation service with automatic primary-to-fallback failover.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use sts_core::cache::TranslationCache;
use tokio::sync::Mutex;

use crate::{
    BingWebTranslator, BoxFuture, GoogleWebTranslator, TranslateError, TranslateRequest,
    TranslateResponse, Translator, validate_languages,
};

/// Translation service that checks an in-memory LRU [`TranslationCache`], batches only cache
/// misses into a single request, falls back from `primary` to `fallback` on recoverable upstream
/// failures, populates the cache, and reassembles translations in the original request order.
pub struct CachedFallbackTranslator {
    primary: Arc<dyn Translator>,
    fallback: Arc<dyn Translator>,
    cache: Arc<Mutex<TranslationCache>>,
}

impl std::fmt::Debug for CachedFallbackTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedFallbackTranslator")
            .field("primary", &self.primary.provider_name())
            .field("fallback", &self.fallback.provider_name())
            .finish_non_exhaustive()
    }
}

impl CachedFallbackTranslator {
    /// Creates a new [`CachedFallbackTranslator`] with explicit primary/fallback providers
    /// and a shared [`TranslationCache`].
    #[must_use]
    pub fn new(
        primary: Arc<dyn Translator>,
        fallback: Arc<dyn Translator>,
        cache: Arc<Mutex<TranslationCache>>,
    ) -> Self {
        Self {
            primary,
            fallback,
            cache,
        }
    }

    /// Creates a [`CachedFallbackTranslator`] wired to the default free Google → Bing web stack.
    #[must_use]
    pub fn with_default_free_providers(cache: Arc<Mutex<TranslationCache>>) -> Self {
        Self::new(
            Arc::new(GoogleWebTranslator::new()),
            Arc::new(BingWebTranslator::new()),
            cache,
        )
    }

    /// Returns a reference to the underlying shared [`TranslationCache`].
    #[must_use]
    pub fn cache(&self) -> &Arc<Mutex<TranslationCache>> {
        &self.cache
    }

    /// Convenience helper to translate a slice of strings for `(source_lang, target_lang)`.
    ///
    /// # Errors
    ///
    /// Propagates [`TranslateError`] if languages are invalid or both providers fail.
    pub async fn translate_batch<S: AsRef<str>>(
        &self,
        source_lang: &str,
        target_lang: &str,
        texts: &[S],
    ) -> Result<TranslateResponse, TranslateError> {
        let request = build_batch_request(source_lang, target_lang, texts);
        self.translate_request(&request).await
    }

    async fn translate_request(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateResponse, TranslateError> {
        translate_with_cache(&self.cache, request, |miss_request| async move {
            self.translate_with_fallback(&miss_request).await
        })
        .await
    }

    async fn translate_with_fallback(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateResponse, TranslateError> {
        match call_checked(self.primary.as_ref(), request).await {
            Ok(resp) => Ok(resp),
            Err(primary_err) if should_fallback(&primary_err) => {
                tracing::warn!(
                    primary = self.primary.provider_name(),
                    fallback = self.fallback.provider_name(),
                    error = %primary_err,
                    "primary translator failed; falling back to secondary provider"
                );
                match call_checked(self.fallback.as_ref(), request).await {
                    Ok(resp) => Ok(resp),
                    Err(fallback_err) => {
                        tracing::warn!(
                            primary = self.primary.provider_name(),
                            fallback = self.fallback.provider_name(),
                            primary_error = %primary_err,
                            fallback_error = %fallback_err,
                            "fallback translator also failed"
                        );
                        Err(combine_errors(primary_err, fallback_err))
                    }
                }
            }
            Err(err) => Err(err),
        }
    }
}

fn build_batch_request<S: AsRef<str>>(
    source_lang: &str,
    target_lang: &str,
    texts: &[S],
) -> TranslateRequest {
    TranslateRequest {
        source_lang: source_lang.to_owned(),
        target_lang: target_lang.to_owned(),
        texts: texts.iter().map(|s| s.as_ref().to_owned()).collect(),
    }
}

async fn translate_with_cache<F, Fut>(
    cache: &Arc<Mutex<TranslationCache>>,
    request: &TranslateRequest,
    fetch_misses: F,
) -> Result<TranslateResponse, TranslateError>
where
    F: FnOnce(TranslateRequest) -> Fut,
    Fut: Future<Output = Result<TranslateResponse, TranslateError>>,
{
    validate_languages(&request.source_lang, &request.target_lang)?;

    if request.texts.is_empty() {
        return Ok(TranslateResponse {
            translations: Vec::new(),
            provider_name: "cache".into(),
        });
    }

    let mut resolved: Vec<Option<String>> = vec![None; request.texts.len()];
    let mut miss_texts: Vec<String> = Vec::new();
    let mut miss_index_groups: Vec<Vec<usize>> = Vec::new();
    let mut miss_lookup: HashMap<&str, usize> = HashMap::new();

    // Scope the cache lock strictly around synchronous lookups so it is never held across `.await`.
    {
        let mut guard = cache.lock().await;
        for (idx, text) in request.texts.iter().enumerate() {
            if text.trim().is_empty() {
                resolved[idx] = Some(text.clone());
                continue;
            }
            if let Some(cached) = guard.get(&request.source_lang, &request.target_lang, text) {
                resolved[idx] = Some(cached.to_owned());
            } else if let Some(&group_idx) = miss_lookup.get(text.as_str()) {
                miss_index_groups[group_idx].push(idx);
            } else {
                let group_idx = miss_texts.len();
                miss_lookup.insert(text.as_str(), group_idx);
                miss_texts.push(text.clone());
                miss_index_groups.push(vec![idx]);
            }
        }
    }

    if miss_texts.is_empty() {
        let translations = collect_resolved(resolved)?;
        return Ok(TranslateResponse {
            translations,
            provider_name: "cache".into(),
        });
    }

    let miss_request = TranslateRequest {
        source_lang: request.source_lang.clone(),
        target_lang: request.target_lang.clone(),
        texts: miss_texts,
    };

    let upstream = fetch_misses(miss_request.clone()).await?;
    if upstream.translations.len() != miss_request.texts.len() {
        return Err(TranslateError::InvalidResponse(format!(
            "provider `{}` returned {} translations for {} inputs",
            upstream.provider_name,
            upstream.translations.len(),
            miss_request.texts.len()
        )));
    }

    // Populate cache under a short-lived lock (dropped immediately).
    {
        let mut guard = cache.lock().await;
        for (source_text, translated_text) in miss_request.texts.iter().zip(&upstream.translations)
        {
            guard.insert(
                &request.source_lang,
                &request.target_lang,
                source_text,
                translated_text.clone(),
            );
        }
    }

    for (indices, translated_text) in miss_index_groups.into_iter().zip(upstream.translations) {
        if let Some((&last_idx, earlier_indices)) = indices.split_last() {
            for &idx in earlier_indices {
                resolved[idx] = Some(translated_text.clone());
            }
            resolved[last_idx] = Some(translated_text);
        }
    }

    let translations = collect_resolved(resolved)?;
    Ok(TranslateResponse {
        translations,
        provider_name: upstream.provider_name,
    })
}

async fn call_checked(
    provider: &dyn Translator,
    request: &TranslateRequest,
) -> Result<TranslateResponse, TranslateError> {
    let response = provider.translate(request).await?;
    if response.translations.len() != request.texts.len() {
        return Err(TranslateError::InvalidResponse(format!(
            "provider `{}` returned {} translations for {} inputs",
            provider.provider_name(),
            response.translations.len(),
            request.texts.len()
        )));
    }
    Ok(response)
}

const fn should_fallback(err: &TranslateError) -> bool {
    matches!(
        err,
        TranslateError::RateLimited(_)
            | TranslateError::Network(_)
            | TranslateError::InvalidResponse(_)
    )
}

fn combine_errors(primary: TranslateError, fallback: TranslateError) -> TranslateError {
    match (primary, fallback) {
        (TranslateError::RateLimited(p), TranslateError::RateLimited(f)) => {
            TranslateError::RateLimited(format!(
                "all translation providers rate limited (primary: {p}; fallback: {f})"
            ))
        }
        (TranslateError::RateLimited(p), other) => TranslateError::RateLimited(format!(
            "primary rate limited ({p}) and fallback failed ({other})"
        )),
        (other, TranslateError::RateLimited(f)) => TranslateError::RateLimited(format!(
            "primary failed ({other}) and fallback rate limited ({f})"
        )),
        (_, fallback_err) => fallback_err,
    }
}

fn collect_resolved(resolved: Vec<Option<String>>) -> Result<Vec<String>, TranslateError> {
    resolved
        .into_iter()
        .map(|slot| {
            slot.ok_or_else(|| {
                TranslateError::InvalidResponse(
                    "missing translated segment while reassembling cached batch".into(),
                )
            })
        })
        .collect()
}

impl Translator for CachedFallbackTranslator {
    fn provider_name(&self) -> &'static str {
        "cached-fallback"
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(self.translate_request(request))
    }
}

/// LRU-cached single-provider translator for BYOK / Ollama backends (never silently falls back
/// to free web endpoints on upstream auth/rate-limit errors).
pub struct CachedTranslator {
    provider: Arc<dyn Translator>,
    cache: Arc<Mutex<TranslationCache>>,
}

impl std::fmt::Debug for CachedTranslator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedTranslator")
            .field("provider", &self.provider.provider_name())
            .finish_non_exhaustive()
    }
}

impl CachedTranslator {
    /// Creates a new [`CachedTranslator`] wrapping `provider` and a shared [`TranslationCache`].
    #[must_use]
    pub fn new(provider: Arc<dyn Translator>, cache: Arc<Mutex<TranslationCache>>) -> Self {
        Self { provider, cache }
    }

    /// Returns a reference to the underlying shared [`TranslationCache`].
    #[must_use]
    pub fn cache(&self) -> &Arc<Mutex<TranslationCache>> {
        &self.cache
    }

    /// Convenience helper to translate a slice of strings for `(source_lang, target_lang)`.
    ///
    /// # Errors
    ///
    /// Propagates [`TranslateError`] if languages are invalid or the upstream provider fails.
    pub async fn translate_batch<S: AsRef<str>>(
        &self,
        source_lang: &str,
        target_lang: &str,
        texts: &[S],
    ) -> Result<TranslateResponse, TranslateError> {
        let request = build_batch_request(source_lang, target_lang, texts);
        self.translate_request(&request).await
    }

    async fn translate_request(
        &self,
        request: &TranslateRequest,
    ) -> Result<TranslateResponse, TranslateError> {
        translate_with_cache(&self.cache, request, |miss_request| async move {
            call_checked(self.provider.as_ref(), &miss_request).await
        })
        .await
    }
}

impl Translator for CachedTranslator {
    fn provider_name(&self) -> &'static str {
        self.provider.provider_name()
    }

    fn translate<'a>(
        &'a self,
        request: &'a TranslateRequest,
    ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
        Box::pin(self.translate_request(request))
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[tokio::test]
    async fn cached_fallback_uses_cache_for_repeats_and_falls_back_to_bing_on_google_429() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        // First call: Google returns 200 JSON for "Hello" -> "Xin chào".
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .and(query_param("q", "Hello"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                [["Xin chào", "Hello", null, null, 10]],
                null,
                "en"
            ])))
            .expect(1)
            .mount(&google_server)
            .await;

        // Second distinct text: Google returns HTTP 429.
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .and(query_param("q", "World"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .expect(1)
            .mount(&google_server)
            .await;

        // Bing fallback handles "World" -> "Thế giới".
        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "detectedLanguage": { "language": "en", "score": 1.0 },
                    "translations": [{ "text": "Thế giới", "to": "vi" }]
                }
            ])))
            .expect(1)
            .mount(&bing_server)
            .await;

        let cache = Arc::new(Mutex::new(TranslationCache::new(64)));
        let service = CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            cache,
        );

        let first = match service.translate_batch("en", "vi", &["Hello"]).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected first Google call to succeed, got error: {err}"),
        };
        assert_eq!(first.translations, vec!["Xin chào"]);
        assert_eq!(first.provider_name, "google-web");

        // Repeat request for "Hello" must hit LRU cache (Google mock expects exactly 1 request).
        let cached = match service.translate_batch("en", "vi", &["Hello"]).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected cache hit to succeed, got error: {err}"),
        };
        assert_eq!(cached.translations, vec!["Xin chào"]);
        assert_eq!(cached.provider_name, "cache");

        // "World" gets 429 from Google and seamlessly falls back to Bing.
        let fallback = match service.translate_batch("en", "vi", &["World"]).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Bing fallback to succeed, got error: {err}"),
        };
        assert_eq!(fallback.translations, vec!["Thế giới"]);
        assert_eq!(fallback.provider_name, "bing-web");
    }

    #[tokio::test]
    async fn cached_fallback_falls_back_to_bing_on_google_html_captcha() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string("<html><body>CAPTCHA unusual traffic</body></html>"),
            )
            .expect(1)
            .mount(&google_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "translations": [{ "text": "Chào buổi sáng", "to": "vi" }]
                }
            ])))
            .expect(1)
            .mount(&bing_server)
            .await;

        let service = CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            Arc::new(Mutex::new(TranslationCache::new(32))),
        );

        let res = match service.translate_batch("ja", "vi", &["おはよう"]).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected fallback after Google CAPTCHA, got error: {err}"),
        };
        assert_eq!(res.translations, vec!["Chào buổi sáng"]);
        assert_eq!(res.provider_name, "bing-web");
    }

    #[tokio::test]
    async fn cached_fallback_batches_only_misses_and_reassembles_original_order() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        // Pre-populate cache with "One" and "Three".
        let cache = Arc::new(Mutex::new(TranslationCache::new(64)));
        {
            let mut guard = cache.lock().await;
            guard.insert("en", "vi", "One", "Một".into());
            guard.insert("en", "vi", "Three", "Ba".into());
        }

        // Only misses ["Two", "Four"] (deduplicated!) should hit Google.
        let expected_query = "Two\n|||STS_SEP|||\nFour";
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .and(query_param("q", expected_query))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                [["Hai\n|||STS_SEP|||\nBốn", expected_query, null, null, 10]],
                null,
                "en"
            ])))
            .expect(1)
            .mount(&google_server)
            .await;

        let service = CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            Arc::clone(&cache),
        );

        let res = match service
            .translate_batch("en", "vi", &["One", "Two", "Three", "Four", "Two"])
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected partial cache batch to succeed, got error: {err}"),
        };
        assert_eq!(res.translations, vec!["Một", "Hai", "Ba", "Bốn", "Hai"]);

        // Subsequent call with all items must now be 100% cached.
        let second = match service
            .translate_batch("en", "vi", &["Four", "Two", "One"])
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected second call to be cached, got error: {err}"),
        };
        assert_eq!(second.translations, vec!["Bốn", "Hai", "Một"]);
        assert_eq!(second.provider_name, "cache");
    }

    #[tokio::test]
    async fn returns_rate_limited_when_both_google_and_bing_return_429() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&google_server)
            .await;

        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&bing_server)
            .await;

        let service = CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            Arc::new(Mutex::new(TranslationCache::new(16))),
        );

        let res = service.translate_batch("en", "vi", &["Blocked"]).await;
        assert!(matches!(res, Err(TranslateError::RateLimited(_))));
    }

    #[tokio::test]
    async fn cached_fallback_works_behind_arc_dyn_translator() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                [["Xin chào", "Hello", null, null, 10]],
                null,
                "en"
            ])))
            .mount(&google_server)
            .await;

        let translator: Arc<dyn Translator> = Arc::new(CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            Arc::new(Mutex::new(TranslationCache::new(8))),
        ));

        let req = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
        let res = match translator.translate(&req).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Arc<dyn Translator> call to succeed, got error: {err}"),
        };
        assert_eq!(res.translations, vec!["Xin chào"]);
    }

    #[tokio::test]
    async fn cached_fallback_falls_back_to_bing_when_google_drops_batch_delimiter() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        // Google returns 200 but drops the batch separator -> unpack_batch returns InvalidResponse.
        Mock::given(method("GET"))
            .and(path("/translate_a/single"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                [["Xin chào thế giới", "Hello World", null, null, 10]],
                null,
                "en"
            ])))
            .expect(1)
            .mount(&google_server)
            .await;

        // Bing preserves the collision-avoided delimiter and exact bubble order.
        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "translations": [{
                        "text": "Dòng 1 |||STS_SEP||| bên trong\n|||STS_SEP_1|||\nDòng 2",
                        "to": "vi"
                    }]
                }
            ])))
            .expect(1)
            .mount(&bing_server)
            .await;

        let service = CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            Arc::new(Mutex::new(TranslationCache::new(32))),
        );

        let inputs = ["Line 1 |||STS_SEP||| inside", "Line 2"];
        let res = match service.translate_batch("en", "vi", &inputs).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected Bing fallback after delimiter drop, got error: {err}"),
        };
        assert_eq!(res.provider_name, "bing-web");
        assert_eq!(
            res.translations,
            vec!["Dòng 1 |||STS_SEP||| bên trong", "Dòng 2"]
        );

        // Verify valid fallback results were cached and scrambled Google text was NOT cached.
        let cached = match service.translate_batch("en", "vi", &inputs).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected cached lookup to succeed, got error: {err}"),
        };
        assert_eq!(cached.provider_name, "cache");
        assert_eq!(
            cached.translations,
            vec!["Dòng 1 |||STS_SEP||| bên trong", "Dòng 2"]
        );
    }

    #[tokio::test]
    async fn cached_fallback_does_not_call_fallback_on_unsupported_language() {
        let google_server = MockServer::start().await;
        let bing_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/ttranslatev3"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&bing_server)
            .await;

        let service = CachedFallbackTranslator::new(
            Arc::new(GoogleWebTranslator::with_base_url(google_server.uri())),
            Arc::new(BingWebTranslator::with_base_url(bing_server.uri())),
            Arc::new(Mutex::new(TranslationCache::new(8))),
        );

        let res = service.translate_batch("en", "", &["Hello"]).await;
        assert!(matches!(res, Err(TranslateError::UnsupportedLanguage(_))));
    }

    #[tokio::test]
    async fn cached_translator_hits_lru_cache_and_never_falls_back_on_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingFailOnMarkerTranslator {
            calls: Arc<AtomicUsize>,
        }

        impl Translator for CountingFailOnMarkerTranslator {
            fn provider_name(&self) -> &'static str {
                "counting-byok"
            }

            fn translate<'a>(
                &'a self,
                request: &'a TranslateRequest,
            ) -> BoxFuture<'a, Result<TranslateResponse, TranslateError>> {
                Box::pin(async move {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    if request.texts.iter().any(|t| t == "__AUTH_FAIL__") {
                        return Err(TranslateError::AuthFailed("bad key".into()));
                    }
                    Ok(TranslateResponse {
                        translations: request.texts.iter().map(|t| format!("VI:{t}")).collect(),
                        provider_name: "counting-byok".into(),
                    })
                })
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(CountingFailOnMarkerTranslator {
            calls: Arc::clone(&calls),
        });
        let cached = CachedTranslator::new(inner, Arc::new(Mutex::new(TranslationCache::new(64))));

        let first = match cached
            .translate_batch("en", "vi", &["Hello", "World"])
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected first BYOK batch to succeed: {err}"),
        };
        assert_eq!(first.provider_name, "counting-byok");
        assert_eq!(first.translations, vec!["VI:Hello", "VI:World"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let second = match cached
            .translate_batch("en", "vi", &["World", "Hello"])
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected cache hit to succeed: {err}"),
        };
        assert_eq!(second.provider_name, "cache");
        assert_eq!(second.translations, vec!["VI:World", "VI:Hello"]);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let partial = match cached
            .translate_batch("en", "vi", &["Hello", "Manga", "  "])
            .await
        {
            Ok(resp) => resp,
            Err(err) => panic!("expected partial cache miss to succeed: {err}"),
        };
        assert_eq!(partial.provider_name, "counting-byok");
        assert_eq!(partial.translations, vec!["VI:Hello", "VI:Manga", "  "]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let err = cached.translate_batch("en", "vi", &["__AUTH_FAIL__"]).await;
        assert!(matches!(err, Err(TranslateError::AuthFailed(_))));
    }
}
