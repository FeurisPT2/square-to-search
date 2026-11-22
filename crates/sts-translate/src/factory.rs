//! Factory builders and redacted secret container for translation and Vision AI providers.

use std::sync::Arc;

use sts_core::cache::TranslationCache;
use sts_core::config::TranslateConfig;
use tokio::sync::Mutex;

use crate::{
    CachedFallbackTranslator, CachedTranslator, DeepLTranslator, GeminiProvider,
    GoogleWebTranslator, OllamaProvider, OpenAiCompatProvider, TranslateError, Translator,
    VisionAiProvider,
};

/// Resolved BYOK API keys passed from the OS keyring into provider factories.
///
/// Implements a custom [`std::fmt::Debug`] that redacts every present key as `"[REDACTED]"` so
/// credentials never leak into logs or panic messages.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderSecrets {
    /// Google Gemini API key.
    pub gemini_api_key: Option<String>,
    /// OpenAI-compatible API key.
    pub openai_api_key: Option<String>,
    /// DeepL API key.
    pub deepl_api_key: Option<String>,
}

impl std::fmt::Debug for ProviderSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |opt: &Option<String>| -> &'static str {
            if opt.is_some() { "[REDACTED]" } else { "None" }
        };
        f.debug_struct("ProviderSecrets")
            .field("gemini_api_key", &redact(&self.gemini_api_key))
            .field("openai_api_key", &redact(&self.openai_api_key))
            .field("deepl_api_key", &redact(&self.deepl_api_key))
            .finish()
    }
}

fn require_secret<'a>(
    provider_label: &str,
    secret: Option<&'a str>,
) -> Result<&'a str, TranslateError> {
    secret
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            TranslateError::ProviderNotConfigured(format!(
                "{provider_label} API key is not saved in OS keyring"
            ))
        })
}

fn optional_custom_prompt(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn non_empty_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    }
}

/// Builds the active [`Translator`] from non-secret `config`, OS keyring `secrets`, and shared
/// LRU `cache`.
///
/// # Errors
///
/// Returns [`TranslateError::ProviderNotConfigured`] if a BYOK provider is selected without its
/// API key in `secrets`, or if `config.provider` is unrecognized.
pub fn build_translator(
    config: &TranslateConfig,
    secrets: &ProviderSecrets,
    cache: Arc<Mutex<TranslationCache>>,
) -> Result<Arc<dyn Translator>, TranslateError> {
    let provider_id = config.provider.trim().to_ascii_lowercase();
    let custom_prompt = optional_custom_prompt(&config.custom_prompt);

    match provider_id.as_str() {
        "" | "free_web" | "free-web" | "google_web" | "google-web" => {
            if config.enable_bing_fallback {
                Ok(Arc::new(
                    CachedFallbackTranslator::with_default_free_providers(cache),
                ))
            } else {
                Ok(Arc::new(CachedTranslator::new(
                    Arc::new(GoogleWebTranslator::new()),
                    cache,
                )))
            }
        }
        "gemini" => {
            let key = require_secret("Gemini", secrets.gemini_api_key.as_deref())?;
            let provider = GeminiProvider::new(key, &config.gemini_model, custom_prompt);
            Ok(Arc::new(CachedTranslator::new(Arc::new(provider), cache)))
        }
        "openai" | "openai-compatible" | "openai_compatible" => {
            let key = require_secret("OpenAI-compatible", secrets.openai_api_key.as_deref())?;
            let provider = OpenAiCompatProvider::new(
                &config.openai_base_url,
                key,
                &config.openai_model,
                custom_prompt,
            );
            Ok(Arc::new(CachedTranslator::new(Arc::new(provider), cache)))
        }
        "deepl" => {
            let key = require_secret("DeepL", secrets.deepl_api_key.as_deref())?;
            let provider = DeepLTranslator::new(key, config.deepl_free_tier);
            Ok(Arc::new(CachedTranslator::new(Arc::new(provider), cache)))
        }
        "ollama" => {
            let provider =
                OllamaProvider::new(&config.ollama_base_url, &config.ollama_model, custom_prompt);
            Ok(Arc::new(CachedTranslator::new(Arc::new(provider), cache)))
        }
        other => Err(TranslateError::ProviderNotConfigured(format!(
            "unsupported translation provider `{other}`"
        ))),
    }
}

/// Builds the optional multimodal [`VisionAiProvider`] for Ask AI from `config` and `secrets`.
///
/// Returns `Ok(None)` when Ask AI is unconfigured (`ai_provider` is `None`, empty, or `"none"`).
///
/// # Errors
///
/// Returns [`TranslateError::ProviderNotConfigured`] if `ai_provider` requires a missing API key
/// or names an unsupported Ask AI backend.
pub fn build_vision_ai_provider(
    config: &TranslateConfig,
    secrets: &ProviderSecrets,
) -> Result<Option<Arc<dyn VisionAiProvider>>, TranslateError> {
    let Some(raw_provider) = config.ai_provider.as_deref() else {
        return Ok(None);
    };
    let provider_id = raw_provider.trim().to_ascii_lowercase();
    if provider_id.is_empty() || provider_id == "none" {
        return Ok(None);
    }

    let custom_prompt = optional_custom_prompt(&config.custom_prompt);
    match provider_id.as_str() {
        "gemini" => {
            let key = require_secret("Gemini", secrets.gemini_api_key.as_deref())?;
            Ok(Some(Arc::new(GeminiProvider::new(
                key,
                &config.gemini_model,
                custom_prompt,
            ))))
        }
        "openai" | "openai-compatible" | "openai_compatible" => {
            let key = require_secret("OpenAI-compatible", secrets.openai_api_key.as_deref())?;
            Ok(Some(Arc::new(OpenAiCompatProvider::new(
                &config.openai_base_url,
                key,
                &config.openai_model,
                custom_prompt,
            ))))
        }
        "ollama" => Ok(Some(Arc::new(OllamaProvider::new(
            &config.ollama_base_url,
            &config.ollama_model,
            custom_prompt,
        )))),
        other => Err(TranslateError::ProviderNotConfigured(format!(
            "unsupported Ask AI provider `{other}`"
        ))),
    }
}

/// Returns a concise UI display label (including model name) for the configured Ask AI provider.
#[must_use]
pub fn vision_provider_display_label(config: &TranslateConfig) -> Option<String> {
    let raw_provider = config.ai_provider.as_deref()?;
    let provider_id = raw_provider.trim().to_ascii_lowercase();
    if provider_id.is_empty() || provider_id == "none" {
        return None;
    }

    match provider_id.as_str() {
        "gemini" => Some(format!(
            "Gemini ({})",
            non_empty_or(&config.gemini_model, "gemini-2.5-flash")
        )),
        "openai" | "openai-compatible" | "openai_compatible" => Some(format!(
            "OpenAI ({})",
            non_empty_or(&config.openai_model, "gpt-4o-mini")
        )),
        "ollama" => Some(format!(
            "Ollama ({})",
            non_empty_or(&config.ollama_model, "qwen2.5vl:7b")
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::TranslateRequest;

    #[test]
    fn provider_secrets_debug_redacts_all_api_keys() {
        let secrets = ProviderSecrets {
            gemini_api_key: Some("AIza-secret-gemini-key".into()),
            openai_api_key: Some("sk-secret-openai-key".into()),
            deepl_api_key: Some("dl-secret-deepl-key".into()),
        };
        let formatted = format!("{secrets:?}");
        assert!(!formatted.contains("AIza-secret-gemini-key"));
        assert!(!formatted.contains("sk-secret-openai-key"));
        assert!(!formatted.contains("dl-secret-deepl-key"));
        assert!(formatted.contains("[REDACTED]"));

        let empty = ProviderSecrets::default();
        let empty_fmt = format!("{empty:?}");
        assert!(empty_fmt.contains("None"));
        assert!(!empty_fmt.contains("[REDACTED]"));
    }

    #[test]
    fn build_translator_returns_provider_not_configured_on_missing_byok_key() {
        let cache = Arc::new(Mutex::new(TranslationCache::new(16)));
        let empty_secrets = ProviderSecrets::default();

        for provider in ["gemini", "openai", "deepl"] {
            let cfg = TranslateConfig {
                provider: provider.into(),
                ..TranslateConfig::default()
            };
            let res = build_translator(&cfg, &empty_secrets, Arc::clone(&cache));
            assert!(
                matches!(res, Err(TranslateError::ProviderNotConfigured(_))),
                "expected ProviderNotConfigured for {provider} without key"
            );
        }

        // Blank/whitespace-only key must also be rejected cleanly.
        let blank_secrets = ProviderSecrets {
            gemini_api_key: Some("   ".into()),
            openai_api_key: Some(String::new()),
            deepl_api_key: Some("\t".into()),
        };
        for provider in ["gemini", "openai", "deepl"] {
            let cfg = TranslateConfig {
                provider: provider.into(),
                ..TranslateConfig::default()
            };
            let res = build_translator(&cfg, &blank_secrets, Arc::clone(&cache));
            assert!(matches!(res, Err(TranslateError::ProviderNotConfigured(_))));
        }
    }

    #[tokio::test]
    async fn build_translator_builds_free_web_ollama_and_cached_byok_without_free_fallback() {
        let cache = Arc::new(Mutex::new(TranslationCache::new(32)));
        let empty_secrets = ProviderSecrets::default();

        let free_cfg = TranslateConfig::default();
        let free_tr = match build_translator(&free_cfg, &empty_secrets, Arc::clone(&cache)) {
            Ok(tr) => tr,
            Err(err) => panic!("expected free_web build to succeed: {err}"),
        };
        assert_eq!(free_tr.provider_name(), "cached-fallback");

        let no_bing_cfg = TranslateConfig {
            enable_bing_fallback: false,
            ..TranslateConfig::default()
        };
        let no_bing_tr = match build_translator(&no_bing_cfg, &empty_secrets, Arc::clone(&cache)) {
            Ok(tr) => tr,
            Err(err) => panic!("expected google-only free_web build to succeed: {err}"),
        };
        assert_eq!(no_bing_tr.provider_name(), "google-web");

        let ollama_cfg = TranslateConfig {
            provider: "ollama".into(),
            ..TranslateConfig::default()
        };
        let ollama_tr = match build_translator(&ollama_cfg, &empty_secrets, Arc::clone(&cache)) {
            Ok(tr) => tr,
            Err(err) => panic!("expected ollama build to succeed without API key: {err}"),
        };
        assert_eq!(ollama_tr.provider_name(), "ollama");

        let full_secrets = ProviderSecrets {
            gemini_api_key: Some("gem-key".into()),
            openai_api_key: Some("sk-valid".into()),
            deepl_api_key: Some("dl-key".into()),
        };
        let gem_tr = match build_translator(
            &TranslateConfig {
                provider: "gemini".into(),
                ..TranslateConfig::default()
            },
            &full_secrets,
            Arc::clone(&cache),
        ) {
            Ok(tr) => tr,
            Err(err) => panic!("expected gemini build to succeed: {err}"),
        };
        assert_eq!(gem_tr.provider_name(), "gemini");

        let deepl_tr = match build_translator(
            &TranslateConfig {
                provider: "deepl".into(),
                ..TranslateConfig::default()
            },
            &full_secrets,
            Arc::clone(&cache),
        ) {
            Ok(tr) => tr,
            Err(err) => panic!("expected deepl build to succeed: {err}"),
        };
        assert_eq!(deepl_tr.provider_name(), "deepl");

        // OpenAI BYOK built via factory uses CachedTranslator (LRU hit on repeat, no free fallback).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": { "role": "assistant", "content": "Xin chào" }
                }]
            })))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
            .expect(1)
            .mount(&server)
            .await;

        let openai_cfg = TranslateConfig {
            provider: "openai".into(),
            openai_base_url: format!("{}/v1", server.uri()),
            ..TranslateConfig::default()
        };
        let byok = match build_translator(&openai_cfg, &full_secrets, Arc::clone(&cache)) {
            Ok(tr) => tr,
            Err(err) => panic!("expected openai build to succeed: {err}"),
        };
        assert_eq!(byok.provider_name(), "openai");

        let req1 = TranslateRequest::new("en", "vi", vec!["Hello".into()]);
        let first = match byok.translate(&req1).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected first BYOK translate to succeed: {err}"),
        };
        assert_eq!(first.translations, vec!["Xin chào"]);

        let repeat = match byok.translate(&req1).await {
            Ok(resp) => resp,
            Err(err) => panic!("expected cached BYOK repeat to succeed: {err}"),
        };
        assert_eq!(repeat.provider_name, "cache");
        assert_eq!(repeat.translations, vec!["Xin chào"]);

        // Cache miss hitting 401 returns AuthFailed directly (no silent free-web fallback).
        let req2 = TranslateRequest::new("en", "vi", vec!["Uncached".into()]);
        let err = byok.translate(&req2).await;
        assert!(matches!(err, Err(TranslateError::AuthFailed(_))));
    }

    #[test]
    fn build_vision_ai_provider_and_display_label_cover_all_variants() {
        let empty_secrets = ProviderSecrets::default();

        let none_cfg = TranslateConfig::default();
        assert!(matches!(
            build_vision_ai_provider(&none_cfg, &empty_secrets),
            Ok(None)
        ));
        assert_eq!(vision_provider_display_label(&none_cfg), None);

        let explicit_none = TranslateConfig {
            ai_provider: Some("none".into()),
            ..TranslateConfig::default()
        };
        assert!(matches!(
            build_vision_ai_provider(&explicit_none, &empty_secrets),
            Ok(None)
        ));
        assert_eq!(vision_provider_display_label(&explicit_none), None);

        let gem_cfg = TranslateConfig {
            ai_provider: Some("gemini".into()),
            ..TranslateConfig::default()
        };
        assert!(matches!(
            build_vision_ai_provider(&gem_cfg, &empty_secrets),
            Err(TranslateError::ProviderNotConfigured(_))
        ));
        assert_eq!(
            vision_provider_display_label(&gem_cfg).as_deref(),
            Some("Gemini (gemini-2.5-flash)")
        );

        let all_secrets = ProviderSecrets {
            gemini_api_key: Some("gem-key".into()),
            openai_api_key: Some("oa-key".into()),
            deepl_api_key: Some("dl-key".into()),
        };
        let gem_provider = match build_vision_ai_provider(&gem_cfg, &all_secrets) {
            Ok(Some(p)) => p,
            Ok(None) => panic!("expected Some(GeminiProvider), got None"),
            Err(err) => panic!("expected Some(GeminiProvider), got Err({err})"),
        };
        assert_eq!(gem_provider.provider_name(), "gemini");

        let oa_cfg = TranslateConfig {
            ai_provider: Some("openai".into()),
            ..TranslateConfig::default()
        };
        assert_eq!(
            vision_provider_display_label(&oa_cfg).as_deref(),
            Some("OpenAI (gpt-4o-mini)")
        );
        let oa_provider = match build_vision_ai_provider(&oa_cfg, &all_secrets) {
            Ok(Some(p)) => p,
            Ok(None) => panic!("expected Some(OpenAiCompatProvider), got None"),
            Err(err) => panic!("expected Some(OpenAiCompatProvider), got Err({err})"),
        };
        assert_eq!(oa_provider.provider_name(), "openai");

        let ollama_cfg = TranslateConfig {
            ai_provider: Some("ollama".into()),
            ..TranslateConfig::default()
        };
        assert_eq!(
            vision_provider_display_label(&ollama_cfg).as_deref(),
            Some("Ollama (qwen2.5vl:7b)")
        );
        let ollama_provider = match build_vision_ai_provider(&ollama_cfg, &empty_secrets) {
            Ok(Some(p)) => p,
            Ok(None) => panic!("expected Some(OllamaProvider), got None"),
            Err(err) => panic!("expected Some(OllamaProvider) without key, got Err({err})"),
        };
        assert_eq!(ollama_provider.provider_name(), "ollama");
    }
}
