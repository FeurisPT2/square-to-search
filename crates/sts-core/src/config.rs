//! Persistent TOML configuration types and resilient [`ConfigStore`].

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::types::{
    ExecutionDevice, LiveDisplayMode, OcrLanguagePack, OcrModelTier, ReadingDirection, UiLanguage,
    VisualSearchEngine,
};

static TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Service identifier used when storing BYOK API keys in the OS keyring.
pub const KEYRING_SERVICE_NAME: &str = "square-to-search";

/// Logical BYOK API key slots persisted exclusively in the OS keyring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretSlot {
    /// Google Gemini API key slot.
    GeminiApiKey,
    /// OpenAI-compatible API key slot.
    OpenAiApiKey,
    /// DeepL API key slot.
    DeepLApiKey,
}

impl SecretSlot {
    /// Returns all supported BYOK secret slots in UI display order.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::GeminiApiKey, Self::OpenAiApiKey, Self::DeepLApiKey]
    }

    /// Returns the credential account key stored inside the OS keyring.
    #[must_use]
    pub const fn account_key(self) -> &'static str {
        match self {
            Self::GeminiApiKey => "gemini_api_key",
            Self::OpenAiApiKey => "openai_api_key",
            Self::DeepLApiKey => "deepl_api_key",
        }
    }

    /// Returns the compact UI/command identifier (`"gemini"`, `"openai"`, `"deepl"`).
    #[must_use]
    pub const fn slot_id(self) -> &'static str {
        match self {
            Self::GeminiApiKey => "gemini",
            Self::OpenAiApiKey => "openai",
            Self::DeepLApiKey => "deepl",
        }
    }

    /// Parses a UI/command slot identifier into a [`SecretSlot`].
    #[must_use]
    pub fn from_slot_id(id: &str) -> Option<Self> {
        let trimmed = id.trim();
        if trimmed.eq_ignore_ascii_case("gemini") {
            Some(Self::GeminiApiKey)
        } else if trimmed.eq_ignore_ascii_case("openai") {
            Some(Self::OpenAiApiKey)
        } else if trimmed.eq_ignore_ascii_case("deepl") {
            Some(Self::DeepLApiKey)
        } else {
            None
        }
    }

    /// Returns the human-readable provider label for this slot.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::GeminiApiKey => "Google Gemini",
            Self::OpenAiApiKey => "OpenAI-compatible",
            Self::DeepLApiKey => "DeepL",
        }
    }
}

fn backup_path_for(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".bak");
    PathBuf::from(os)
}

fn temp_path_for(path: &Path) -> PathBuf {
    let seq = TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut os = path.as_os_str().to_os_string();
    os.push(format!(".{}.{seq}.tmp", std::process::id()));
    PathBuf::from(os)
}

/// Top-level application configuration persisted as `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub ui_language: UiLanguage,
    pub autostart: bool,
    pub hotkeys: HotkeyConfig,
    pub live: LiveTranslateConfig,
    pub ocr: OcrConfig,
    pub translate: TranslateConfig,
    pub search: VisualSearchConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            ui_language: UiLanguage::Vietnamese,
            autostart: false,
            hotkeys: HotkeyConfig::default(),
            live: LiveTranslateConfig::default(),
            ocr: OcrConfig::default(),
            translate: TranslateConfig::default(),
            search: VisualSearchConfig::default(),
        }
    }
}

/// Global hotkey bindings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotkeyConfig {
    pub square_to_search: String,
    pub toggle_live_translate: String,
    pub toggle_live_visibility: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            square_to_search: "Ctrl+Shift+S".to_owned(),
            toggle_live_translate: "Ctrl+Shift+T".to_owned(),
            toggle_live_visibility: "Ctrl+Shift+H".to_owned(),
        }
    }
}

/// Live Translate settings for continuous comic/manga reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveTranslateConfig {
    pub display_mode: LiveDisplayMode,
    pub source_lang: String,
    pub target_lang: String,
    pub reading_direction: ReadingDirection,
    pub debounce_ms: u64,
    pub overlay_opacity: f32,
    pub overlay_font_size: f32,
}

impl Default for LiveTranslateConfig {
    fn default() -> Self {
        Self {
            display_mode: LiveDisplayMode::InPlace,
            source_lang: "auto".to_owned(),
            target_lang: "vi".to_owned(),
            reading_direction: ReadingDirection::Auto,
            debounce_ms: 350,
            overlay_opacity: 0.88,
            overlay_font_size: 16.0,
        }
    }
}

/// Local PP-OCRv5 engine and model pack preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OcrConfig {
    pub live_pack: OcrLanguagePack,
    pub snip_pack: OcrLanguagePack,
    pub tier: OcrModelTier,
    pub device: ExecutionDevice,
    pub idle_unload_secs: u64,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            live_pack: OcrLanguagePack::CjkEn,
            snip_pack: OcrLanguagePack::CjkEn,
            tier: OcrModelTier::Mobile,
            device: ExecutionDevice::Auto,
            idle_unload_secs: 300,
        }
    }
}

fn default_gemini_model() -> String {
    "gemini-2.5-flash".to_owned()
}

fn default_openai_base_url() -> String {
    "https://api.openai.com/v1".to_owned()
}

fn default_openai_model() -> String {
    "gpt-4o-mini".to_owned()
}

fn default_ollama_model() -> String {
    "qwen2.5vl:7b".to_owned()
}

const fn default_deepl_free_tier() -> bool {
    true
}

/// Translation and optional BYOK / Ollama AI settings (non-secret fields only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TranslateConfig {
    pub source_lang: String,
    pub target_lang: String,
    pub enable_bing_fallback: bool,
    pub cache_capacity: usize,
    pub provider: String,
    pub ai_provider: Option<String>,
    #[serde(default = "default_gemini_model")]
    pub gemini_model: String,
    #[serde(default = "default_openai_base_url")]
    pub openai_base_url: String,
    #[serde(default = "default_openai_model")]
    pub openai_model: String,
    pub ollama_base_url: String,
    #[serde(default = "default_ollama_model")]
    pub ollama_model: String,
    #[serde(default = "default_deepl_free_tier")]
    pub deepl_free_tier: bool,
    pub custom_prompt: String,
}

impl Default for TranslateConfig {
    fn default() -> Self {
        Self {
            source_lang: "auto".to_owned(),
            target_lang: "vi".to_owned(),
            enable_bing_fallback: true,
            cache_capacity: 1024,
            provider: "free_web".to_owned(),
            ai_provider: None,
            gemini_model: default_gemini_model(),
            openai_base_url: default_openai_base_url(),
            openai_model: default_openai_model(),
            ollama_base_url: "http://localhost:11434".to_owned(),
            ollama_model: default_ollama_model(),
            deepl_free_tier: default_deepl_free_tier(),
            custom_prompt: String::new(),
        }
    }
}

/// Visual reverse image search settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisualSearchConfig {
    pub engine: VisualSearchEngine,
    pub enable_clipboard_fallback: bool,
}

impl Default for VisualSearchConfig {
    fn default() -> Self {
        Self {
            engine: VisualSearchEngine::GoogleLens,
            enable_clipboard_fallback: true,
        }
    }
}

/// Result of loading configuration from [`ConfigStore`].
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigLoadOutcome {
    /// Loaded cleanly from an existing TOML file (with missing fields backfilled from defaults).
    Loaded(AppConfig),
    /// Configuration file did not exist yet; returned default values.
    Default(AppConfig),
    /// Configuration file was corrupt or invalid; backed up to `backup_path` and recovered with
    /// default values.
    RecoveredFromCorrupt {
        backup_path: PathBuf,
        config: AppConfig,
    },
}

impl ConfigLoadOutcome {
    /// Borrows the loaded or recovered [`AppConfig`].
    #[must_use]
    pub const fn config(&self) -> &AppConfig {
        match self {
            Self::Loaded(cfg)
            | Self::Default(cfg)
            | Self::RecoveredFromCorrupt { config: cfg, .. } => cfg,
        }
    }

    /// Consumes the outcome and returns the [`AppConfig`].
    #[must_use]
    pub fn into_config(self) -> AppConfig {
        match self {
            Self::Loaded(cfg)
            | Self::Default(cfg)
            | Self::RecoveredFromCorrupt { config: cfg, .. } => cfg,
        }
    }
}

/// Manages loading, corrupt-file backup recovery, and atomic TOML writes for `config.toml`.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Creates a store targeting an explicit file path.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Returns the managed configuration file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Resolves the standard per-user OS configuration file path (`.../config.toml`).
    pub fn default_os_path() -> Result<PathBuf, CoreError> {
        let dirs = ProjectDirs::from("dev", "square-to-search", "square-to-search")
            .ok_or(CoreError::ConfigDirUnavailable)?;
        Ok(dirs.config_dir().join("config.toml"))
    }

    /// Loads `config.toml`, backfilling missing fields from defaults, or backs up corrupt TOML to
    /// `config.toml.bak` and recovers cleanly with defaults.
    pub fn load_or_default(&self) -> Result<ConfigLoadOutcome, CoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ConfigLoadOutcome::Default(AppConfig::default()));
            }
            Err(source) => {
                return Err(CoreError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };

        let parsed = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|raw| toml::from_str::<AppConfig>(raw).ok());

        if let Some(config) = parsed {
            return Ok(ConfigLoadOutcome::Loaded(config));
        }

        let backup_path = backup_path_for(&self.path);
        fs::rename(&self.path, &backup_path).map_err(|source| CoreError::Io {
            path: backup_path.clone(),
            source,
        })?;
        let config = AppConfig::default();
        self.save(&config)?;

        Ok(ConfigLoadOutcome::RecoveredFromCorrupt {
            backup_path,
            config,
        })
    }

    /// Atomically persists `config` as pretty TOML by writing a sibling temp file and renaming.
    pub fn save(&self, config: &AppConfig) -> Result<(), CoreError> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|source| CoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let serialized = toml::to_string_pretty(config)?;
        let tmp_path = temp_path_for(&self.path);

        let write_result = (|| -> std::io::Result<()> {
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(source) = write_result {
            let _ = fs::remove_file(&tmp_path);
            return Err(CoreError::Io {
                path: tmp_path,
                source,
            });
        }

        if let Err(source) = fs::rename(&tmp_path, &self.path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(CoreError::Io {
                path: self.path.clone(),
                source,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn default_config_matches_spec_values() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.ui_language, UiLanguage::Vietnamese);
        assert!(!cfg.autostart);
        assert_eq!(cfg.hotkeys.square_to_search, "Ctrl+Shift+S");
        assert_eq!(cfg.hotkeys.toggle_live_translate, "Ctrl+Shift+T");
        assert_eq!(cfg.hotkeys.toggle_live_visibility, "Ctrl+Shift+H");
        assert_eq!(cfg.live.display_mode, LiveDisplayMode::InPlace);
        assert_eq!(cfg.live.reading_direction, ReadingDirection::Auto);
        assert_eq!(cfg.live.target_lang, "vi");
        assert_eq!(cfg.live.debounce_ms, 350);
        assert_eq!(cfg.ocr.live_pack, OcrLanguagePack::CjkEn);
        assert_eq!(cfg.ocr.snip_pack, OcrLanguagePack::CjkEn);
        assert_eq!(cfg.ocr.idle_unload_secs, 300);
        assert_eq!(cfg.ocr.tier, OcrModelTier::Mobile);
        assert_eq!(cfg.ocr.device, ExecutionDevice::Auto);
        assert_eq!(cfg.translate.target_lang, "vi");
        assert!(cfg.translate.enable_bing_fallback);
        assert_eq!(cfg.translate.ai_provider, None);
        assert_eq!(cfg.search.engine, VisualSearchEngine::GoogleLens);
        assert!(cfg.search.enable_clipboard_fallback);
    }

    #[test]
    fn config_store_returns_default_when_file_missing() -> TestResult {
        let dir = TempDir::new()?;
        let store = ConfigStore::new(dir.path().join("config.toml"));

        let outcome = store.load_or_default()?;
        assert_eq!(outcome, ConfigLoadOutcome::Default(AppConfig::default()));
        Ok(())
    }

    #[test]
    fn config_store_backfills_missing_fields_from_partial_toml() -> TestResult {
        let dir = TempDir::new()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[live]\ndebounce_ms = 500\n")?;

        let store = ConfigStore::new(path);
        let outcome = store.load_or_default()?;

        let mut expected = AppConfig::default();
        expected.live.debounce_ms = 500;
        assert_eq!(outcome, ConfigLoadOutcome::Loaded(expected));
        Ok(())
    }

    #[test]
    fn config_store_backs_up_corrupt_toml_and_recovers_default() -> TestResult {
        let dir = TempDir::new()?;
        let path = dir.path().join("config.toml");
        let corrupt = "not = [valid toml";
        fs::write(&path, corrupt)?;

        let store = ConfigStore::new(path);
        let outcome = store.load_or_default()?;

        match outcome {
            ConfigLoadOutcome::RecoveredFromCorrupt {
                backup_path,
                config,
            } => {
                assert!(backup_path.ends_with("config.toml.bak"));
                assert!(backup_path.exists());
                assert_eq!(fs::read_to_string(&backup_path)?, corrupt);
                assert_eq!(config, AppConfig::default());
            }
            other => {
                return Err(format!("expected RecoveredFromCorrupt, got {other:?}").into());
            }
        }

        // Subsequent boot loads cleanly from the recovered default config.toml.
        let reloaded = store.load_or_default()?;
        assert_eq!(reloaded.config(), &AppConfig::default());
        assert_eq!(reloaded.into_config(), AppConfig::default());
        Ok(())
    }

    #[test]
    fn config_store_backs_up_non_utf8_corrupt_bytes_and_overwrites_stale_backup() -> TestResult {
        let dir = TempDir::new()?;
        let path = dir.path().join("config.toml");
        let existing_bak = dir.path().join("config.toml.bak");
        fs::write(&existing_bak, "stale backup")?;
        let binary_garbage = [0xff_u8, 0xfe, 0x00, 0x80];
        fs::write(&path, binary_garbage)?;

        let store = ConfigStore::new(path);
        let outcome = store.load_or_default()?;

        match outcome {
            ConfigLoadOutcome::RecoveredFromCorrupt {
                backup_path,
                config,
            } => {
                assert_eq!(backup_path, existing_bak);
                assert_eq!(fs::read(&backup_path)?, binary_garbage);
                assert_eq!(config, AppConfig::default());
            }
            other => {
                return Err(format!("expected RecoveredFromCorrupt, got {other:?}").into());
            }
        }
        Ok(())
    }

    #[test]
    fn config_store_recovers_when_legacy_or_unsupported_cuda_device_present() -> TestResult {
        let dir = TempDir::new()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ocr]\ndevice = \"cuda\"\n")?;

        let store = ConfigStore::new(path);
        let outcome = store.load_or_default()?;

        match outcome {
            ConfigLoadOutcome::RecoveredFromCorrupt {
                backup_path,
                config,
            } => {
                assert!(backup_path.ends_with("config.toml.bak"));
                assert!(backup_path.exists());
                assert_eq!(config.ocr.device, ExecutionDevice::Auto);
            }
            other => {
                return Err(format!("expected RecoveredFromCorrupt, got {other:?}").into());
            }
        }
        Ok(())
    }

    #[test]
    fn config_store_save_roundtrips_and_overwrites_atomically() -> TestResult {
        let dir = TempDir::new()?;
        let nested_path = dir.path().join("nested").join("config.toml");
        let store = ConfigStore::new(nested_path);

        let mut cfg_v1 = AppConfig::default();
        cfg_v1.live.debounce_ms = 420;
        cfg_v1.ocr.tier = OcrModelTier::Server;
        store.save(&cfg_v1)?;
        assert_eq!(store.load_or_default()?, ConfigLoadOutcome::Loaded(cfg_v1));

        let cfg_v2 = AppConfig {
            ui_language: UiLanguage::English,
            search: VisualSearchConfig {
                engine: VisualSearchEngine::Yandex,
                enable_clipboard_fallback: false,
            },
            ..AppConfig::default()
        };
        store.save(&cfg_v2)?;
        assert_eq!(store.load_or_default()?, ConfigLoadOutcome::Loaded(cfg_v2));

        let leftover_tmps: Vec<_> = fs::read_dir(dir.path().join("nested"))?
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftover_tmps.is_empty());
        Ok(())
    }

    #[test]
    fn config_store_default_os_path_ends_with_config_toml() -> TestResult {
        let path = ConfigStore::default_os_path()?;
        assert!(path.ends_with("config.toml"));
        Ok(())
    }

    #[test]
    fn secret_slot_ids_roundtrip_and_cover_all_byok_slots() {
        assert_eq!(KEYRING_SERVICE_NAME, "square-to-search");

        let slots = SecretSlot::all();
        assert_eq!(
            slots,
            [
                SecretSlot::GeminiApiKey,
                SecretSlot::OpenAiApiKey,
                SecretSlot::DeepLApiKey,
            ]
        );

        let mut account_keys = std::collections::HashSet::new();
        let mut slot_ids = std::collections::HashSet::new();
        for slot in slots {
            let slot_id = slot.slot_id();
            let account_key = slot.account_key();
            assert!(!slot_id.is_empty());
            assert!(!account_key.is_empty());
            assert!(!slot.display_name().is_empty());
            assert_eq!(SecretSlot::from_slot_id(slot_id), Some(slot));
            assert!(slot_ids.insert(slot_id));
            assert!(account_keys.insert(account_key));
        }

        assert_eq!(
            SecretSlot::from_slot_id("gemini"),
            Some(SecretSlot::GeminiApiKey)
        );
        assert_eq!(
            SecretSlot::from_slot_id("openai"),
            Some(SecretSlot::OpenAiApiKey)
        );
        assert_eq!(
            SecretSlot::from_slot_id("deepl"),
            Some(SecretSlot::DeepLApiKey)
        );
        assert_eq!(SecretSlot::from_slot_id("unknown"), None);
        assert_eq!(SecretSlot::from_slot_id(""), None);
    }

    #[test]
    fn translate_config_serializes_no_secret_fields_and_backfills_m3_defaults() -> TestResult {
        let default_cfg = AppConfig::default();
        let serialized = toml::to_string_pretty(&default_cfg)?;
        assert!(
            !serialized.to_ascii_lowercase().contains("api_key"),
            "serialized config.toml must never contain any api_key field:\n{serialized}"
        );

        let dir = TempDir::new()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[translate]\nprovider = \"gemini\"\n")?;

        let store = ConfigStore::new(path);
        let outcome = store.load_or_default()?;
        let ConfigLoadOutcome::Loaded(loaded) = outcome else {
            return Err(format!("expected Loaded outcome, got {outcome:?}").into());
        };

        assert_eq!(loaded.translate.provider, "gemini");
        assert_eq!(loaded.translate.gemini_model, "gemini-2.5-flash");
        assert_eq!(
            loaded.translate.openai_base_url,
            "https://api.openai.com/v1"
        );
        assert_eq!(loaded.translate.openai_model, "gpt-4o-mini");
        assert_eq!(loaded.translate.ollama_model, "qwen2.5vl:7b");
        assert!(loaded.translate.deepl_free_tier);
        assert_eq!(loaded.translate.source_lang, "auto");
        assert_eq!(loaded.translate.target_lang, "vi");
        assert!(loaded.translate.enable_bing_fallback);
        assert_eq!(loaded.translate.cache_capacity, 1024);
        assert_eq!(loaded.translate.ai_provider, None);
        assert_eq!(loaded.translate.ollama_base_url, "http://localhost:11434");
        assert_eq!(loaded.translate.custom_prompt, "");
        Ok(())
    }
}
