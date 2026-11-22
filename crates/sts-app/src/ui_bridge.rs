//! Slint UI globals and window event-loop bridge for `square-to-search`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use raw_window_handle::HasWindowHandle;
use slint::winit_030::{WinitWindowAccessor, winit};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedPixelBuffer, SharedString, VecModel};
use sts_core::config::{AppConfig, SecretSlot};
use sts_core::geometry::{LogicalRect, MonitorGeometry, PhysicalRect};
use sts_core::types::{
    CaptureFrame, ExecutionDevice, LiveDisplayMode, OcrLanguagePack, OcrModelTier,
    ReadingDirection, UiLanguage, VisualSearchEngine,
};
use sts_ocr::manifest::{ModelStore, PackInstallStatus};
use sts_platform::display_env::detect_display_server;
use sts_platform::overlay::{OverlayStyle, OverlayWindowConfigurator, resolve_effective_live_mode};
use sts_search::VisualSearchOutcome;
use sts_translate::vision_provider_display_label;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::live::LiveRenderedBlock;
use crate::state::{
    AppCommand, ProviderTestTarget, UiEvent, pack_from_id, pack_to_id, tier_from_id, tier_to_id,
};
use crate::{
    AppActions, AppState, FirstRunOcrWindow, LiveControlsWindow, LiveInPlaceWindow,
    LivePanelWindow, LiveTranslatedBlockRow, ModelPackRow, SettingsWindow, SnipOverlayWindow,
};

/// Holds strong handles to all top-level Slint windows so their weak references remain upgradable
/// throughout the Slint event loop.
pub struct UiWindows {
    pub snip_overlay: SnipOverlayWindow,
    pub first_run_ocr: FirstRunOcrWindow,
    pub settings: SettingsWindow,
    pub live_inplace: LiveInPlaceWindow,
    pub live_panel: LivePanelWindow,
    pub live_controls: LiveControlsWindow,
}

#[derive(Debug, Clone)]
struct LiveUiRuntime {
    display_mode: LiveDisplayMode,
    overlay_visible: bool,
    overlay_monitor: Option<MonitorGeometry>,
    overlay_physical_rect: PhysicalRect,
}

impl Default for LiveUiRuntime {
    fn default() -> Self {
        Self {
            display_mode: LiveDisplayMode::InPlace,
            overlay_visible: true,
            overlay_monitor: None,
            overlay_physical_rect: PhysicalRect::default(),
        }
    }
}

/// Converts an RGBA8 [`CaptureFrame`] into a [`slint::Image`].
///
/// Returns `None` if dimensions are zero or the buffer length does not match `width * height * 4`.
#[must_use]
pub fn capture_frame_to_slint_image(frame: &CaptureFrame) -> Option<slint::Image> {
    if frame.width == 0 || frame.height == 0 || frame.rgba.len() != frame.expected_byte_len() {
        return None;
    }
    let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        &frame.rgba,
        frame.width,
        frame.height,
    );
    Some(slint::Image::from_rgba8(buffer))
}

/// Maps [`LiveDisplayMode`] to its Slint `AppState.live-display-mode` string identifier.
#[must_use]
pub const fn live_display_mode_to_id(mode: LiveDisplayMode) -> &'static str {
    match mode {
        LiveDisplayMode::InPlace => "in-place",
        LiveDisplayMode::FloatingPanel => "floating-panel",
    }
}

/// Converts an `[r, g, b, a]` byte array into a Slint [`Color`].
#[must_use]
pub fn rgba_to_slint_color(rgba: [u8; 4]) -> Color {
    Color::from_argb_u8(rgba[3], rgba[0], rgba[1], rgba[2])
}

/// Converts rendered Live speech-bubble blocks into Slint [`LiveTranslatedBlockRow`] records.
#[must_use]
pub fn live_blocks_to_rows(blocks: &[LiveRenderedBlock]) -> Vec<LiveTranslatedBlockRow> {
    blocks
        .iter()
        .map(|block| LiveTranslatedBlockRow {
            index: i32::try_from(block.index).unwrap_or(i32::MAX),
            x: block.logical_rect.x,
            y: block.logical_rect.y,
            width: block.logical_rect.width,
            height: block.logical_rect.height,
            original_text: SharedString::from(block.original_text.as_str()),
            translated_text: SharedString::from(block.translated_text.as_str()),
            bg_color: rgba_to_slint_color(block.bg_rgba),
            text_color: rgba_to_slint_color(block.text_rgba),
            font_size: block.font_size,
        })
        .collect()
}

/// Maps [`UiLanguage`] to its Slint `ComboBox` index (`0` = Tiếng Việt, `1` = English).
#[must_use]
pub const fn ui_language_to_index(lang: UiLanguage) -> i32 {
    match lang {
        UiLanguage::Vietnamese => 0,
        UiLanguage::English => 1,
    }
}

/// Maps a Slint `ComboBox` index back to [`UiLanguage`].
#[must_use]
pub const fn ui_language_from_index(index: i32) -> UiLanguage {
    match index {
        1 => UiLanguage::English,
        _ => UiLanguage::Vietnamese,
    }
}

/// Maps [`LiveDisplayMode`] to its Slint `ComboBox` index (`0` = In-place, `1` = Floating Panel).
#[must_use]
pub const fn live_display_mode_to_index(mode: LiveDisplayMode) -> i32 {
    match mode {
        LiveDisplayMode::InPlace => 0,
        LiveDisplayMode::FloatingPanel => 1,
    }
}

/// Maps a Slint `ComboBox` index back to [`LiveDisplayMode`].
#[must_use]
pub const fn live_display_mode_from_index(index: i32) -> LiveDisplayMode {
    match index {
        1 => LiveDisplayMode::FloatingPanel,
        _ => LiveDisplayMode::InPlace,
    }
}

/// Maps a Live Translate source language code to its Slint `ComboBox` index.
#[must_use]
pub fn live_source_lang_to_index(lang: &str) -> i32 {
    match lang.trim() {
        "ja" => 1,
        "ko" => 2,
        "zh" | "zh-CN" | "zh-TW" => 3,
        "en" => 4,
        _ => 0,
    }
}

/// Maps a Slint `ComboBox` index back to a Live Translate source language code.
#[must_use]
pub const fn live_source_lang_from_index(index: i32) -> &'static str {
    match index {
        1 => "ja",
        2 => "ko",
        3 => "zh-CN",
        4 => "en",
        _ => "auto",
    }
}

/// Maps [`ReadingDirection`] to its Slint `ComboBox` index.
#[must_use]
pub const fn reading_direction_to_index(dir: ReadingDirection) -> i32 {
    match dir {
        ReadingDirection::Auto => 0,
        ReadingDirection::RightToLeft => 1,
        ReadingDirection::LeftToRight => 2,
    }
}

/// Maps a Slint `ComboBox` index back to [`ReadingDirection`].
#[must_use]
pub const fn reading_direction_from_index(index: i32) -> ReadingDirection {
    match index {
        1 => ReadingDirection::RightToLeft,
        2 => ReadingDirection::LeftToRight,
        _ => ReadingDirection::Auto,
    }
}

/// Maps a target language code (`"vi"`, `"en"`) to its Slint `ComboBox` index.
#[must_use]
pub fn target_lang_to_index(lang: &str) -> i32 {
    if lang.trim().eq_ignore_ascii_case("en") {
        1
    } else {
        0
    }
}

/// Maps a Slint `ComboBox` index back to a target language code (`"vi"`, `"en"`).
#[must_use]
pub const fn target_lang_from_index(index: i32) -> &'static str {
    match index {
        1 => "en",
        _ => "vi",
    }
}

/// Maps [`ExecutionDevice`] to its Slint `ComboBox` index (`0` = Auto, `1` = CPU, `2` = DirectML).
/// Note: Decision 3A excludes CUDA in v1.
#[must_use]
pub const fn execution_device_to_index(device: ExecutionDevice) -> i32 {
    match device {
        ExecutionDevice::Auto => 0,
        ExecutionDevice::Cpu => 1,
        ExecutionDevice::DirectMl => 2,
    }
}

/// Maps a Slint `ComboBox` index back to [`ExecutionDevice`] (Auto / CPU / DirectML only).
#[must_use]
pub const fn execution_device_from_index(index: i32) -> ExecutionDevice {
    match index {
        1 => ExecutionDevice::Cpu,
        2 => ExecutionDevice::DirectMl,
        _ => ExecutionDevice::Auto,
    }
}

/// Maps [`OcrModelTier`] to its Slint `ComboBox` index (`0` = Mobile, `1` = Server).
#[must_use]
pub const fn ocr_tier_to_index(tier: OcrModelTier) -> i32 {
    match tier {
        OcrModelTier::Mobile => 0,
        OcrModelTier::Server => 1,
    }
}

/// Maps a Slint `ComboBox` index back to [`OcrModelTier`].
#[must_use]
pub const fn ocr_tier_from_index(index: i32) -> OcrModelTier {
    match index {
        1 => OcrModelTier::Server,
        _ => OcrModelTier::Mobile,
    }
}

/// Maps [`OcrLanguagePack`] to its Settings Square-to-Search default OCR language `ComboBox` index
/// (`0` = CJK + EN, `1` = Vietnamese / Latin, `2` = Korean).
#[must_use]
pub const fn ocr_pack_to_index(pack: OcrLanguagePack) -> i32 {
    match pack {
        OcrLanguagePack::CjkEn => 0,
        OcrLanguagePack::VietnameseLatin => 1,
        OcrLanguagePack::Korean => 2,
    }
}

/// Maps a Settings `ComboBox` index back to [`OcrLanguagePack`].
#[must_use]
pub const fn ocr_pack_from_index(index: i32) -> OcrLanguagePack {
    match index {
        1 => OcrLanguagePack::VietnameseLatin,
        2 => OcrLanguagePack::Korean,
        _ => OcrLanguagePack::CjkEn,
    }
}

/// Maps a translation provider identifier to its Slint `ComboBox` index.
#[must_use]
pub fn translate_provider_to_index(provider: &str) -> i32 {
    match provider.trim().to_ascii_lowercase().as_str() {
        "gemini" => 1,
        "openai" | "openai-compatible" | "openai_compatible" => 2,
        "deepl" => 3,
        "ollama" => 4,
        _ => 0,
    }
}

/// Maps a Slint `ComboBox` index back to a translation provider identifier.
#[must_use]
pub const fn translate_provider_from_index(index: i32) -> &'static str {
    match index {
        1 => "gemini",
        2 => "openai",
        3 => "deepl",
        4 => "ollama",
        _ => "free_web",
    }
}

/// Maps an optional Ask AI provider identifier to its Slint `ComboBox` index.
#[must_use]
pub fn ask_ai_provider_to_index(provider: Option<&str>) -> i32 {
    match provider.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("gemini") => 1,
        Some("openai" | "openai-compatible" | "openai_compatible") => 2,
        Some("ollama") => 3,
        _ => 0,
    }
}

/// Maps a Slint `ComboBox` index back to an optional Ask AI provider identifier.
#[must_use]
pub fn ask_ai_provider_from_index(index: i32) -> Option<String> {
    match index {
        1 => Some("gemini".to_owned()),
        2 => Some("openai".to_owned()),
        3 => Some("ollama".to_owned()),
        _ => None,
    }
}

/// Maps [`VisualSearchEngine`] to its Slint `ComboBox` index (`0` = Google Lens, `1` = Yandex, `2` = Bing).
#[must_use]
pub const fn visual_search_engine_to_index(engine: VisualSearchEngine) -> i32 {
    match engine {
        VisualSearchEngine::GoogleLens => 0,
        VisualSearchEngine::Yandex => 1,
        VisualSearchEngine::Bing => 2,
    }
}

/// Maps a Slint `ComboBox` index back to [`VisualSearchEngine`].
#[must_use]
pub const fn visual_search_engine_from_index(index: i32) -> VisualSearchEngine {
    match index {
        1 => VisualSearchEngine::Yandex,
        2 => VisualSearchEngine::Bing,
        _ => VisualSearchEngine::GoogleLens,
    }
}

/// Populates all Settings and shared [`AppState`] properties from `config`.
pub fn apply_config_to_app_state(
    state: &AppState<'_>,
    config: &AppConfig,
    show_wayland_notice: bool,
) {
    // Snip overlay quick OCR pack & AI status
    state.set_active_snip_ocr_pack(SharedString::from(pack_to_id(config.ocr.snip_pack)));
    let ai_configured = config
        .translate
        .ai_provider
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty() && !p.eq_ignore_ascii_case("none"));
    state.set_ai_provider_configured(ai_configured);

    // Tab 0: Chung
    state.set_ui_language_index(ui_language_to_index(config.ui_language));
    state.set_autostart_enabled(config.autostart);
    state.set_hotkey_square_to_search(SharedString::from(config.hotkeys.square_to_search.as_str()));
    state.set_hotkey_toggle_live(SharedString::from(
        config.hotkeys.toggle_live_translate.as_str(),
    ));
    state.set_hotkey_toggle_visibility(SharedString::from(
        config.hotkeys.toggle_live_visibility.as_str(),
    ));

    // Tab 1: Live Translate
    state.set_live_display_mode_index(live_display_mode_to_index(config.live.display_mode));
    state.set_show_wayland_notice(show_wayland_notice);
    state.set_live_source_lang_index(live_source_lang_to_index(&config.live.source_lang));
    state.set_live_reading_dir_index(reading_direction_to_index(config.live.reading_direction));
    state.set_live_target_lang_index(target_lang_to_index(&config.live.target_lang));
    state.set_live_debounce_ms((config.live.debounce_ms.clamp(100, 2000)) as i32);
    let opacity_pct = (config.live.overlay_opacity * 100.0)
        .round()
        .clamp(30.0, 100.0) as i32;
    state.set_live_overlay_opacity_pct(opacity_pct);
    let font_size = config.live.overlay_font_size.round().clamp(10.0, 28.0) as i32;
    state.set_live_overlay_font_size(font_size);

    // Tab 2: OCR & Models (Auto / CPU / DirectML only — NO CUDA)
    state.set_ocr_tier_index(ocr_tier_to_index(config.ocr.tier));
    state.set_ocr_snip_lang_index(ocr_pack_to_index(config.ocr.snip_pack));
    state.set_ocr_device_index(execution_device_to_index(config.ocr.device));
    let idle_minutes = (config.ocr.idle_unload_secs / 60).clamp(1, 60) as i32;
    state.set_ocr_idle_unload_minutes(idle_minutes);

    // Tab 3: Dịch & AI
    state
        .set_live_translate_provider_index(translate_provider_to_index(&config.translate.provider));
    state.set_ask_ai_provider_index(ask_ai_provider_to_index(
        config.translate.ai_provider.as_deref(),
    ));
    state.set_gemini_model(SharedString::from(config.translate.gemini_model.as_str()));
    state.set_openai_base_url(SharedString::from(
        config.translate.openai_base_url.as_str(),
    ));
    state.set_openai_model(SharedString::from(config.translate.openai_model.as_str()));
    state.set_ollama_base_url(SharedString::from(
        config.translate.ollama_base_url.as_str(),
    ));
    state.set_ollama_model(SharedString::from(config.translate.ollama_model.as_str()));
    state.set_deepl_free_tier(config.translate.deepl_free_tier);
    state.set_ai_provider_label(SharedString::from(
        vision_provider_display_label(&config.translate).unwrap_or_default(),
    ));
    let free_status = match translate_provider_to_index(&config.translate.provider) {
        1 => "Đang chọn: Gemini (BYOK) · Có dự phòng Free Web nếu thiếu key",
        2 => "Đang chọn: OpenAI-compatible (BYOK) · Có dự phòng Free Web nếu thiếu key",
        3 => "Đang chọn: DeepL (BYOK) · Có dự phòng Free Web nếu thiếu key",
        4 => "Đang chọn: Ollama Local · Không cần API key",
        _ if config.translate.enable_bing_fallback => {
            "Đang bật: Google Translate Web (dự phòng Bing Translator) · Batch + LRU Cache"
        }
        _ => "Đang bật: Google Translate Web · Batch + LRU Cache",
    };
    state.set_free_translator_status(SharedString::from(free_status));
    if !config.translate.custom_prompt.is_empty() {
        state.set_ai_custom_prompt(SharedString::from(config.translate.custom_prompt.as_str()));
    }

    // Tab 4: Tìm ảnh
    state.set_visual_search_engine_index(visual_search_engine_to_index(config.search.engine));
    state.set_visual_search_clipboard_fallback(config.search.enable_clipboard_fallback);
}

/// Clears all masked BYOK password input fields so plaintext API keys never linger in Slint state.
pub fn clear_key_inputs_on_app_state(state: &AppState<'_>) {
    state.set_gemini_key_input(SharedString::default());
    state.set_openai_key_input(SharedString::default());
    state.set_deepl_key_input(SharedString::default());
}

fn resolve_ai_configured(config: &AppConfig, has_gemini_key: bool, has_openai_key: bool) -> bool {
    match config
        .translate
        .ai_provider
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("gemini") => has_gemini_key,
        Some("openai" | "openai-compatible" | "openai_compatible") => has_openai_key,
        Some("ollama") => true,
        _ => false,
    }
}

/// Updates OS keyring key-presence badges on [`AppState`] and clears all password input fields.
pub fn apply_keyring_status_to_app_state(
    state: &AppState<'_>,
    has_gemini: bool,
    has_openai: bool,
    has_deepl: bool,
    ai_provider_configured: bool,
) {
    state.set_has_gemini_key(has_gemini);
    state.set_has_openai_key(has_openai);
    state.set_has_deepl_key(has_deepl);
    state.set_ai_provider_configured(ai_provider_configured);
    clear_key_inputs_on_app_state(state);
}

fn clear_key_input_for_slot(state: &AppState<'_>, slot: SecretSlot) {
    match slot {
        SecretSlot::GeminiApiKey => state.set_gemini_key_input(SharedString::default()),
        SecretSlot::OpenAiApiKey => state.set_openai_key_input(SharedString::default()),
        SecretSlot::DeepLApiKey => state.set_deepl_key_input(SharedString::default()),
    }
}

fn reset_live_runtime_state(state: &AppState<'_>, can_inplace: bool) {
    state.set_live_selecting_region(false);
    state.set_live_active(false);
    state.set_live_paused(false);
    state.set_live_overlay_visible(true);
    state.set_live_can_inplace(can_inplace);
    state.set_live_display_mode(SharedString::from(live_display_mode_to_id(
        if can_inplace {
            LiveDisplayMode::InPlace
        } else {
            LiveDisplayMode::FloatingPanel
        },
    )));
    state.set_live_status_text(SharedString::default());
    state.set_live_error_text(SharedString::default());
    state.set_live_rate_limited(false);
    state.set_live_blocks(ModelRc::new(VecModel::<LiveTranslatedBlockRow>::default()));
}

/// Reads all user-editable settings fields from [`AppState`] and merges them onto `base`.
#[must_use]
pub fn read_config_from_app_state(state: &AppState<'_>, base: &AppConfig) -> AppConfig {
    let mut cfg = base.clone();

    // Tab 0: Chung
    cfg.ui_language = ui_language_from_index(state.get_ui_language_index());
    cfg.autostart = state.get_autostart_enabled();
    let hk_snip = state.get_hotkey_square_to_search().trim().to_owned();
    if !hk_snip.is_empty() {
        cfg.hotkeys.square_to_search = hk_snip;
    }
    let hk_live = state.get_hotkey_toggle_live().trim().to_owned();
    if !hk_live.is_empty() {
        cfg.hotkeys.toggle_live_translate = hk_live;
    }
    let hk_vis = state.get_hotkey_toggle_visibility().trim().to_owned();
    if !hk_vis.is_empty() {
        cfg.hotkeys.toggle_live_visibility = hk_vis;
    }

    // Tab 1: Live Translate
    cfg.live.display_mode = live_display_mode_from_index(state.get_live_display_mode_index());
    cfg.live.source_lang =
        live_source_lang_from_index(state.get_live_source_lang_index()).to_owned();
    cfg.live.reading_direction = reading_direction_from_index(state.get_live_reading_dir_index());
    cfg.live.target_lang = target_lang_from_index(state.get_live_target_lang_index()).to_owned();
    cfg.live.debounce_ms = (state.get_live_debounce_ms().clamp(100, 2000)) as u64;
    cfg.live.overlay_opacity = (state.get_live_overlay_opacity_pct().clamp(30, 100) as f32) / 100.0;
    cfg.live.overlay_font_size = state.get_live_overlay_font_size().clamp(10, 28) as f32;

    // Tab 2: OCR & Models
    cfg.ocr.tier = ocr_tier_from_index(state.get_ocr_tier_index());
    cfg.ocr.snip_pack = ocr_pack_from_index(state.get_ocr_snip_lang_index());
    cfg.ocr.device = execution_device_from_index(state.get_ocr_device_index());
    cfg.ocr.idle_unload_secs = (state.get_ocr_idle_unload_minutes().clamp(1, 60) as u64) * 60;

    // Tab 3: Dịch & AI
    cfg.translate.provider =
        translate_provider_from_index(state.get_live_translate_provider_index()).to_owned();
    cfg.translate.ai_provider = ask_ai_provider_from_index(state.get_ask_ai_provider_index());
    let gemini_model = state.get_gemini_model().trim().to_owned();
    if !gemini_model.is_empty() {
        cfg.translate.gemini_model = gemini_model;
    }
    let openai_base_url = state.get_openai_base_url().trim().to_owned();
    if !openai_base_url.is_empty() {
        cfg.translate.openai_base_url = openai_base_url;
    }
    let openai_model = state.get_openai_model().trim().to_owned();
    if !openai_model.is_empty() {
        cfg.translate.openai_model = openai_model;
    }
    let ollama_url = state.get_ollama_base_url().trim().to_owned();
    if !ollama_url.is_empty() {
        cfg.translate.ollama_base_url = ollama_url;
    }
    let ollama_model = state.get_ollama_model().trim().to_owned();
    if !ollama_model.is_empty() {
        cfg.translate.ollama_model = ollama_model;
    }
    cfg.translate.deepl_free_tier = state.get_deepl_free_tier();
    cfg.translate.custom_prompt = state.get_ai_custom_prompt().to_string();
    cfg.translate.target_lang = cfg.live.target_lang.clone();

    // Tab 4: Tìm ảnh
    cfg.search.engine = visual_search_engine_from_index(state.get_visual_search_engine_index());
    cfg.search.enable_clipboard_fallback = state.get_visual_search_clipboard_fallback();

    cfg
}

fn pack_ui_metadata(pack: OcrLanguagePack) -> (&'static str, &'static str) {
    match pack {
        OcrLanguagePack::CjkEn => (
            "CJK + EN (Unified PP-OCRv5)",
            "Tiếng Trung, Tiếng Nhật, Tiếng Anh — mặc định cho đọc truyện & lướt web",
        ),
        OcrLanguagePack::VietnameseLatin => (
            "Tiếng Việt / Latin (latin_PP-OCRv5)",
            "Nhận diện chính xác dấu tiếng Việt & chữ Latin khi lướt web",
        ),
        OcrLanguagePack::Korean => (
            "Tiếng Hàn (korean_PP-OCRv5)",
            "Chuyên biệt cho manhwa & văn bản tiếng Hàn",
        ),
    }
}

fn format_size_mb(bytes: u64) -> String {
    let mb = ((bytes as f64) / 1_000_000.0).round().max(1.0) as u64;
    format!("~{mb} MB")
}

/// Converts [`PackInstallStatus`] entries for `active_tier` into Slint [`ModelPackRow`] items.
#[must_use]
pub fn build_model_pack_rows(
    statuses: &[PackInstallStatus],
    active_tier: OcrModelTier,
) -> Vec<ModelPackRow> {
    let order = [
        OcrLanguagePack::CjkEn,
        OcrLanguagePack::VietnameseLatin,
        OcrLanguagePack::Korean,
    ];

    order
        .into_iter()
        .map(|pack| {
            let (display_name, description) = pack_ui_metadata(pack);
            let matched = statuses
                .iter()
                .find(|s| s.pack == pack && s.tier == active_tier);
            let installed = matched.is_some_and(|s| s.installed);
            let expected_bytes = matched.map_or(20_000_000, |s| s.expected_size_bytes);
            let status_text = if installed {
                "Đã cài đặt · SHA-256 hợp lệ"
            } else {
                "Chưa tải"
            };

            ModelPackRow {
                pack_id: SharedString::from(pack_to_id(pack)),
                tier_id: SharedString::from(tier_to_id(active_tier)),
                display_name: SharedString::from(display_name),
                description: SharedString::from(description),
                size_label: SharedString::from(format_size_mb(expected_bytes)),
                installed,
                downloading: false,
                progress: if installed { 1.0 } else { 0.0 },
                status_text: SharedString::from(status_text),
            }
        })
        .collect()
}

/// Applies [`PackInstallStatus`] entries onto `state.model_packs` for `active_tier`.
pub fn apply_model_statuses_to_app_state(
    state: &AppState<'_>,
    statuses: &[PackInstallStatus],
    active_tier: OcrModelTier,
) {
    let rows = build_model_pack_rows(statuses, active_tier);
    state.set_model_packs(ModelRc::new(VecModel::from(rows)));
}

fn update_pack_download_progress_on_state(
    state: &AppState<'_>,
    pack: OcrLanguagePack,
    tier: OcrModelTier,
    progress: f32,
    status_text: &str,
) {
    let target_pack_id = pack_to_id(pack);
    let target_tier_id = tier_to_id(tier);
    let model = state.get_model_packs();
    let mut rows = Vec::with_capacity(model.row_count());
    for i in 0..model.row_count() {
        if let Some(mut row) = model.row_data(i) {
            if row.pack_id.as_str() == target_pack_id && row.tier_id.as_str() == target_tier_id {
                row.downloading = progress > 0.0 && progress < 1.0;
                row.progress = progress.clamp(0.0, 1.0);
                row.status_text = SharedString::from(status_text);
            }
            rows.push(row);
        }
    }
    state.set_model_packs(ModelRc::new(VecModel::from(rows)));
}

fn resolve_offline_import_dir(pack_id: &str, tier_id: &str) -> PathBuf {
    if let Ok(custom) = std::env::var("STS_OFFLINE_MODEL_DIR")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    let base = ModelStore::default_os_dir().unwrap_or_else(|_| PathBuf::from("models"));
    base.join("offline_import").join(pack_id).join(tier_id)
}

fn apply_native_overlay_style<C>(
    window_weak: slint::Weak<C>,
    style: OverlayStyle,
    configurator: &Arc<dyn OverlayWindowConfigurator>,
    window_label: &'static str,
) where
    C: ComponentHandle + 'static,
{
    let try_apply = move |comp: &C, cfg: &Arc<dyn OverlayWindowConfigurator>| -> bool {
        comp.window()
            .with_winit_window(|winit_window: &winit::window::Window| {
                // Set winit hit-test state first so subsequent native Win32/X11 extended styles
                // (WS_EX_LAYERED | WS_EX_TRANSPARENT | WDA_EXCLUDEFROMCAPTURE / XShape) stay intact.
                if let Err(err) = winit_window.set_cursor_hittest(!style.click_through) {
                    tracing::debug!(
                        window = window_label,
                        click_through = style.click_through,
                        error = %err,
                        "winit set_cursor_hittest returned error"
                    );
                }
                match winit_window.window_handle() {
                    Ok(handle) => {
                        if let Err(err) = cfg.apply_style(handle.as_raw(), style) {
                            tracing::warn!(
                                window = window_label,
                                ?style,
                                error = %err,
                                "failed to apply native overlay window style"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            window = window_label,
                            error = %err,
                            "failed to extract RawWindowHandle for overlay window"
                        );
                    }
                }
            })
            .is_some()
    };

    if let Some(comp) = window_weak.upgrade()
        && !try_apply(&comp, configurator)
    {
        let cfg_retry = Arc::clone(configurator);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(comp_retry) = window_weak.upgrade() {
                let _ = try_apply(&comp_retry, &cfg_retry);
            }
        });
    }
}

fn position_live_inplace_window(
    window: &LiveInPlaceWindow,
    monitor: &MonitorGeometry,
    rect: PhysicalRect,
) {
    let px = monitor.x.saturating_add(i32::try_from(rect.x).unwrap_or(0));
    let py = monitor.y.saturating_add(i32::try_from(rect.y).unwrap_or(0));
    window
        .window()
        .set_position(slint::PhysicalPosition::new(px, py));
    window.window().set_size(slint::PhysicalSize::new(
        rect.width.max(1),
        rect.height.max(1),
    ));
}

fn position_live_controls_window(
    window: &LiveControlsWindow,
    monitor: &MonitorGeometry,
    rect: PhysicalRect,
) {
    let scale = if monitor.scale_factor.is_finite() && monitor.scale_factor > 0.0 {
        monitor.scale_factor
    } else {
        1.0
    };
    let pill_w = (480.0 * scale).round() as i32;
    let pill_h = (56.0 * scale).round() as i32;
    let margin = (12.0 * scale).round() as i32;

    let mon_w = i32::try_from(monitor.width).unwrap_or(1920);
    let mon_h = i32::try_from(monitor.height).unwrap_or(1080);
    let region_center_x = monitor
        .x
        .saturating_add(i32::try_from(rect.x.saturating_add(rect.width / 2)).unwrap_or(0));

    let min_x = monitor.x.saturating_add(margin);
    let max_x = monitor
        .x
        .saturating_add(mon_w.saturating_sub(pill_w).saturating_sub(margin))
        .max(min_x);
    let target_x = region_center_x
        .saturating_sub(pill_w / 2)
        .clamp(min_x, max_x);

    let min_y = monitor.y.saturating_add(margin);
    let max_y = monitor
        .y
        .saturating_add(mon_h.saturating_sub(pill_h).saturating_sub(margin))
        .max(min_y);
    let region_top = monitor.y.saturating_add(i32::try_from(rect.y).unwrap_or(0));
    let target_y = if region_top.saturating_sub(pill_h).saturating_sub(margin) >= min_y {
        region_top.saturating_sub(pill_h).saturating_sub(margin)
    } else {
        min_y
    }
    .clamp(min_y, max_y);

    window
        .window()
        .set_position(slint::PhysicalPosition::new(target_x, target_y));
}

fn position_live_panel_window(
    window: &LivePanelWindow,
    monitor: &MonitorGeometry,
    rect: PhysicalRect,
) {
    let scale = if monitor.scale_factor.is_finite() && monitor.scale_factor > 0.0 {
        monitor.scale_factor
    } else {
        1.0
    };
    let panel_w = (360.0 * scale).round() as i32;
    let margin = (16.0 * scale).round() as i32;
    let mon_w = i32::try_from(monitor.width).unwrap_or(1920);

    let min_x = monitor.x.saturating_add(margin);
    let max_x = monitor
        .x
        .saturating_add(mon_w.saturating_sub(panel_w).saturating_sub(margin))
        .max(min_x);
    let region_right = monitor
        .x
        .saturating_add(i32::try_from(rect.x.saturating_add(rect.width)).unwrap_or(0));
    let target_x = if region_right.saturating_add(margin) <= max_x {
        region_right.saturating_add(margin)
    } else {
        max_x
    }
    .clamp(min_x, max_x);

    let target_y = monitor
        .y
        .saturating_add(i32::try_from(rect.y).unwrap_or(0))
        .max(monitor.y.saturating_add(margin));

    window
        .window()
        .set_position(slint::PhysicalPosition::new(target_x, target_y));
}

macro_rules! bind_app_actions {
    ($window:expr, $cmd_tx:expr, $settings_weak:expr, $last_config:expr) => {{
        let actions = $window.global::<AppActions>();

        {
            let tx = $cmd_tx.clone();
            actions.on_cancel_overlay(move || {
                let _ = tx.send(AppCommand::CancelOverlay);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_selection_changed(move |x, y, width, height| {
                let _ = tx.send(AppCommand::UpdateSelection(LogicalRect {
                    x,
                    y,
                    width,
                    height,
                }));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_action_visual_search(move |x, y, width, height| {
                let _ = tx.send(AppCommand::RunVisualSearch(LogicalRect {
                    x,
                    y,
                    width,
                    height,
                }));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_action_copy_ocr(move |x, y, width, height| {
                let _ = tx.send(AppCommand::RunOcrCopy(LogicalRect {
                    x,
                    y,
                    width,
                    height,
                }));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_action_quick_translate(move |x, y, width, height| {
                let _ = tx.send(AppCommand::RunQuickTranslate(LogicalRect {
                    x,
                    y,
                    width,
                    height,
                }));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_action_ask_ai(move |x, y, width, height| {
                let _ = tx.send(AppCommand::RunAskAi(LogicalRect {
                    x,
                    y,
                    width,
                    height,
                }));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_copy_result_text(move |text| {
                let _ = tx.send(AppCommand::CopyResultText(text.to_string()));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_search_text_google(move |text| {
                let _ = tx.send(AppCommand::SearchTextGoogle(text.to_string()));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_switch_snip_ocr_pack(move |pack_id| {
                if let Some(pack) = pack_from_id(pack_id.as_str()) {
                    let _ = tx.send(AppCommand::SwitchSnipOcrPack(pack));
                }
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_start_starter_download(move || {
                let _ = tx.send(AppCommand::StartStarterDownload);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_dismiss_first_run(move || {
                let _ = tx.send(AppCommand::DismissFirstRun);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_open_settings_tab(move |tab| {
                let _ = tx.send(AppCommand::OpenSettingsTab(tab));
            });
        }

        {
            let tx = $cmd_tx.clone();
            let settings_weak = $settings_weak.clone();
            let last_config = Arc::clone(&$last_config);
            actions.on_save_settings(move || {
                if let Some(settings_win) = settings_weak.upgrade() {
                    let base = match last_config.lock() {
                        Ok(guard) => guard.clone(),
                        Err(poisoned) => poisoned.into_inner().clone(),
                    };
                    let updated =
                        read_config_from_app_state(&settings_win.global::<AppState>(), &base);
                    let _ = tx.send(AppCommand::SaveSettings(updated));
                }
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_download_model_pack(move |pack_id, tier_id| {
                if let (Some(pack), Some(tier)) = (
                    pack_from_id(pack_id.as_str()),
                    tier_from_id(tier_id.as_str()),
                ) {
                    let _ = tx.send(AppCommand::DownloadModelPack { pack, tier });
                }
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_delete_model_pack(move |pack_id, tier_id| {
                if let (Some(pack), Some(tier)) = (
                    pack_from_id(pack_id.as_str()),
                    tier_from_id(tier_id.as_str()),
                ) {
                    let _ = tx.send(AppCommand::DeleteModelPack { pack, tier });
                }
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_import_offline_model(move |pack_id, tier_id| {
                if let (Some(pack), Some(tier)) = (
                    pack_from_id(pack_id.as_str()),
                    tier_from_id(tier_id.as_str()),
                ) {
                    let dir = resolve_offline_import_dir(pack_id.as_str(), tier_id.as_str());
                    let _ = tx.send(AppCommand::ImportOfflineModel { pack, tier, dir });
                }
            });
        }

        // Live Translate callbacks (7 new Milestone 2 actions)
        {
            let tx = $cmd_tx.clone();
            actions.on_live_confirm_region(move |x, y, width, height| {
                let _ = tx.send(AppCommand::ConfirmLiveRegion(LogicalRect {
                    x,
                    y,
                    width,
                    height,
                }));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_live_toggle_pause(move || {
                let _ = tx.send(AppCommand::ToggleLivePause);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_live_toggle_visibility(move || {
                let _ = tx.send(AppCommand::ToggleLiveVisibility);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_live_switch_display_mode(move || {
                let _ = tx.send(AppCommand::SwitchLiveDisplayMode);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_live_reselect_region(move || {
                let _ = tx.send(AppCommand::ReselectLiveRegion);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_live_stop(move || {
                let _ = tx.send(AppCommand::StopLiveTranslate);
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_live_copy_block(move |text| {
                let _ = tx.send(AppCommand::CopyLiveBlock(text.to_string()));
            });
        }

        // Ask AI streaming & BYOK OS Keyring callbacks
        {
            let tx = $cmd_tx.clone();
            actions.on_submit_ask_ai(move |prompt| {
                let _ = tx.send(AppCommand::SubmitAskAiPrompt(prompt.to_string()));
            });
        }

        {
            let tx = $cmd_tx.clone();
            actions.on_stop_ask_ai(move || {
                let _ = tx.send(AppCommand::StopAskAi);
            });
        }

        {
            let tx = $cmd_tx.clone();
            let settings_weak = $settings_weak.clone();
            actions.on_save_api_key(move |slot_id, value| {
                if let Some(slot) = SecretSlot::from_slot_id(slot_id.as_str()) {
                    let secret_value = value.to_string();
                    if let Some(settings_win) = settings_weak.upgrade() {
                        clear_key_input_for_slot(&settings_win.global::<AppState>(), slot);
                    }
                    let _ = tx.send(AppCommand::SaveApiKey {
                        slot,
                        value: secret_value,
                    });
                }
            });
        }

        {
            let tx = $cmd_tx.clone();
            let settings_weak = $settings_weak.clone();
            actions.on_clear_api_key(move |slot_id| {
                if let Some(slot) = SecretSlot::from_slot_id(slot_id.as_str()) {
                    if let Some(settings_win) = settings_weak.upgrade() {
                        clear_key_input_for_slot(&settings_win.global::<AppState>(), slot);
                    }
                    let _ = tx.send(AppCommand::ClearApiKey(slot));
                }
            });
        }

        {
            let tx = $cmd_tx.clone();
            let settings_weak = $settings_weak.clone();
            actions.on_test_ai_provider(move |target_id| {
                let target = match target_id.trim().to_ascii_lowercase().as_str() {
                    "ask-ai" | "ask_ai" | "vision" | "ai" => ProviderTestTarget::AskAi,
                    _ => ProviderTestTarget::LiveTranslator,
                };
                if let Some(settings_win) = settings_weak.upgrade() {
                    let state = settings_win.global::<AppState>();
                    state.set_ai_testing(true);
                    state.set_ai_test_status(SharedString::default());
                }
                let _ = tx.send(AppCommand::TestAiProvider(target));
            });
        }
    }};
}

/// Initializes all 6 Slint windows (`SnipOverlayWindow`, `FirstRunOcrWindow`, `SettingsWindow`,
/// `LiveInPlaceWindow`, `LivePanelWindow`, `LiveControlsWindow`), binds all `AppActions`
/// callbacks to `cmd_tx`, and spawns a task forwarding `ui_rx` events onto the Slint UI thread
/// via `slint::invoke_from_event_loop`.
pub fn setup_ui_bridge(
    cmd_tx: UnboundedSender<AppCommand>,
    mut ui_rx: UnboundedReceiver<UiEvent>,
    initial_config: &AppConfig,
    initial_statuses: &[PackInstallStatus],
    overlay_configurator: Arc<dyn OverlayWindowConfigurator>,
) -> anyhow::Result<UiWindows> {
    let snip_overlay = SnipOverlayWindow::new()?;
    let first_run_ocr = FirstRunOcrWindow::new()?;
    let settings = SettingsWindow::new()?;
    let live_inplace = LiveInPlaceWindow::new()?;
    let live_panel = LivePanelWindow::new()?;
    let live_controls = LiveControlsWindow::new()?;

    let display_server = detect_display_server();
    let show_wayland_notice = display_server.is_wayland();
    let can_inplace = resolve_effective_live_mode(LiveDisplayMode::InPlace, display_server).mode
        == LiveDisplayMode::InPlace;

    for state in [
        snip_overlay.global::<AppState>(),
        first_run_ocr.global::<AppState>(),
        settings.global::<AppState>(),
        live_inplace.global::<AppState>(),
        live_panel.global::<AppState>(),
        live_controls.global::<AppState>(),
    ] {
        apply_config_to_app_state(&state, initial_config, show_wayland_notice);
        reset_live_runtime_state(&state, can_inplace);
    }
    apply_model_statuses_to_app_state(
        &settings.global::<AppState>(),
        initial_statuses,
        initial_config.ocr.tier,
    );

    let snip_weak = snip_overlay.as_weak();
    let first_run_weak = first_run_ocr.as_weak();
    let settings_weak = settings.as_weak();
    let live_inplace_weak = live_inplace.as_weak();
    let live_panel_weak = live_panel.as_weak();
    let live_controls_weak = live_controls.as_weak();
    let last_config = Arc::new(Mutex::new(initial_config.clone()));
    let live_runtime = Arc::new(Mutex::new(LiveUiRuntime::default()));

    bind_app_actions!(snip_overlay, cmd_tx, settings_weak, last_config);
    bind_app_actions!(first_run_ocr, cmd_tx, settings_weak, last_config);
    bind_app_actions!(settings, cmd_tx, settings_weak, last_config);
    bind_app_actions!(live_inplace, cmd_tx, settings_weak, last_config);
    bind_app_actions!(live_panel, cmd_tx, settings_weak, last_config);
    bind_app_actions!(live_controls, cmd_tx, settings_weak, last_config);

    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            let handles = UiWeakHandles {
                snip: snip_weak.clone(),
                first_run: first_run_weak.clone(),
                settings: settings_weak.clone(),
                live_inplace: live_inplace_weak.clone(),
                live_panel: live_panel_weak.clone(),
                live_controls: live_controls_weak.clone(),
            };
            let last_cfg = Arc::clone(&last_config);
            let live_rt = Arc::clone(&live_runtime);
            let overlay_cfg = Arc::clone(&overlay_configurator);

            let _ = slint::invoke_from_event_loop(move || {
                apply_ui_event(event, &handles, &last_cfg, &live_rt, &overlay_cfg);
            });
        }
    });

    Ok(UiWindows {
        snip_overlay,
        first_run_ocr,
        settings,
        live_inplace,
        live_panel,
        live_controls,
    })
}

struct UiWeakHandles {
    snip: slint::Weak<SnipOverlayWindow>,
    first_run: slint::Weak<FirstRunOcrWindow>,
    settings: slint::Weak<SettingsWindow>,
    live_inplace: slint::Weak<LiveInPlaceWindow>,
    live_panel: slint::Weak<LivePanelWindow>,
    live_controls: slint::Weak<LiveControlsWindow>,
}

fn update_all_live_states<F>(handles: &UiWeakHandles, mut f: F)
where
    F: FnMut(&AppState<'_>),
{
    if let Some(win) = handles.live_inplace.upgrade() {
        f(&win.global::<AppState>());
    }
    if let Some(win) = handles.live_panel.upgrade() {
        f(&win.global::<AppState>());
    }
    if let Some(win) = handles.live_controls.upgrade() {
        f(&win.global::<AppState>());
    }
}

fn show_snip_window_with_mode(
    snip: &SnipOverlayWindow,
    monitor: &MonitorGeometry,
    frame: &CaptureFrame,
    live_selecting_region: bool,
) {
    let state = snip.global::<AppState>();
    if let Some(img) = capture_frame_to_slint_image(frame) {
        state.set_screenshot_image(img);
    }
    state.set_live_selecting_region(live_selecting_region);
    state.set_has_selection(false);
    state.set_selection_x(0.0);
    state.set_selection_y(0.0);
    state.set_selection_width(0.0);
    state.set_selection_height(0.0);
    state.set_card_mode(SharedString::from("none"));
    state.set_ocr_text(SharedString::default());
    state.set_translated_text(SharedString::default());
    state.set_card_status_text(SharedString::default());
    state.set_card_error_text(SharedString::default());
    state.set_toast_text(SharedString::default());
    state.set_ai_prompt(SharedString::default());
    state.set_ai_answer(SharedString::default());
    state.set_ai_streaming(false);

    snip.window()
        .set_position(slint::PhysicalPosition::new(monitor.x, monitor.y));
    snip.window().set_size(slint::PhysicalSize::new(
        monitor.width.max(1),
        monitor.height.max(1),
    ));
    let _ = snip.show();
}

fn apply_ui_event(
    event: UiEvent,
    handles: &UiWeakHandles,
    last_config: &Arc<Mutex<AppConfig>>,
    live_runtime: &Arc<Mutex<LiveUiRuntime>>,
    overlay_configurator: &Arc<dyn OverlayWindowConfigurator>,
) {
    match event {
        UiEvent::ShowSnipOverlay { monitor, frame } => {
            if let Some(snip) = handles.snip.upgrade() {
                show_snip_window_with_mode(&snip, &monitor, &frame, false);
            }
        }

        UiEvent::ShowLiveRegionPicker { monitor, frame } => {
            if let Some(snip) = handles.snip.upgrade() {
                show_snip_window_with_mode(&snip, &monitor, &frame, true);
            }
        }

        UiEvent::HideSnipOverlay => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_live_selecting_region(false);
                state.set_card_mode(SharedString::from("none"));
                state.set_has_selection(false);
                state.set_toast_text(SharedString::default());
                state.set_ai_streaming(false);
                state.set_ai_answer(SharedString::default());
                state.set_ai_prompt(SharedString::default());
                let _ = snip.hide();
            }
        }

        UiEvent::SelectionUpdated(rect) => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_has_selection(true);
                state.set_selection_x(rect.x);
                state.set_selection_y(rect.y);
                state.set_selection_width(rect.width);
                state.set_selection_height(rect.height);
            }
        }

        UiEvent::ShowCardLoading => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_streaming(false);
                state.set_card_error_text(SharedString::default());
                state.set_card_mode(SharedString::from("loading"));
            }
        }

        UiEvent::ShowInlineResult {
            ocr_text,
            translated_text,
            pack,
            status_text,
        } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_streaming(false);
                state.set_active_snip_ocr_pack(SharedString::from(pack_to_id(pack)));
                state.set_ocr_text(SharedString::from(ocr_text));
                state.set_card_status_text(SharedString::from(status_text));
                state.set_card_error_text(SharedString::default());
                match translated_text {
                    Some(translated) => {
                        state.set_translated_text(SharedString::from(translated));
                        state.set_card_mode(SharedString::from("translate"));
                    }
                    None => {
                        state.set_translated_text(SharedString::default());
                        state.set_card_mode(SharedString::from("ocr"));
                    }
                }
            }
        }

        UiEvent::ShowCardError { message } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_streaming(false);
                state.set_card_error_text(SharedString::from(message));
                state.set_card_mode(SharedString::from("error"));
            }
        }

        UiEvent::ShowAskAiUnconfigured => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_streaming(false);
                state.set_ai_provider_configured(false);
                state.set_card_mode(SharedString::from("ai-unconfigured"));
            }
        }

        UiEvent::ShowAskAiReady { provider_label } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_provider_configured(true);
                state.set_ai_provider_label(SharedString::from(provider_label));
                state.set_ai_streaming(false);
                state.set_ai_answer(SharedString::default());
                state.set_ai_prompt(SharedString::default());
                state.set_card_error_text(SharedString::default());
                state.set_card_status_text(SharedString::default());
                state.set_card_mode(SharedString::from("ai"));
            }
        }

        UiEvent::AskAiStreamStarted { provider_label } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_provider_configured(true);
                state.set_ai_provider_label(SharedString::from(provider_label));
                state.set_ai_answer(SharedString::default());
                state.set_ai_streaming(true);
                state.set_card_error_text(SharedString::default());
                state.set_card_status_text(SharedString::from("Đang stream phản hồi..."));
                state.set_card_mode(SharedString::from("ai"));
            }
        }

        UiEvent::AskAiStreamDelta { chunk } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                let mut current = state.get_ai_answer().to_string();
                current.push_str(&chunk);
                state.set_ai_answer(SharedString::from(current));
            }
        }

        UiEvent::AskAiStreamFinished {
            full_answer,
            provider_label,
        } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_streaming(false);
                if !full_answer.is_empty() {
                    state.set_ai_answer(SharedString::from(full_answer));
                }
                state.set_ai_provider_label(SharedString::from(provider_label.as_str()));
                state.set_card_status_text(SharedString::from(format!(
                    "Hoàn tất · {provider_label}"
                )));
            }
        }

        UiEvent::AskAiStreamError { message } => {
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                state.set_ai_streaming(false);
                state.set_card_error_text(SharedString::from(message));
                state.set_card_mode(SharedString::from("error"));
            }
        }

        UiEvent::VisualSearchDone(outcome) => {
            if let VisualSearchOutcome::FallbackClipboard { .. } = outcome
                && let Some(snip) = handles.snip.upgrade()
            {
                snip.global::<AppState>().set_toast_text(SharedString::from(
                    "Đã copy ảnh vào Clipboard — nhấn Ctrl+V trên trang tìm ảnh.",
                ));
            }
        }

        UiEvent::ShowFirstRunOcrModal { pack, tier } => {
            if let Some(first_run) = handles.first_run.upgrade() {
                let state = first_run.global::<AppState>();
                state.set_starter_downloading(false);
                state.set_starter_download_progress(0.0);
                state.set_starter_error_text(SharedString::default());
                state.set_starter_status_text(SharedString::from(format!(
                    "Sẵn sàng tải gói PP-OCRv5 {} ({}) có kiểm tra SHA-256.",
                    tier_to_id(tier),
                    pack_to_id(pack)
                )));
                let _ = first_run.show();
            }
        }

        UiEvent::StarterDownloadProgress {
            progress,
            status_text,
        } => {
            if let Some(first_run) = handles.first_run.upgrade() {
                let state = first_run.global::<AppState>();
                state.set_starter_downloading(true);
                state.set_starter_download_progress(progress.clamp(0.0, 1.0));
                state.set_starter_status_text(SharedString::from(status_text));
                state.set_starter_error_text(SharedString::default());
            }
        }

        UiEvent::StarterDownloadFailed { message } => {
            if let Some(first_run) = handles.first_run.upgrade() {
                let state = first_run.global::<AppState>();
                state.set_starter_downloading(false);
                state.set_starter_error_text(SharedString::from(message));
            }
        }

        UiEvent::HideFirstRunOcrModal => {
            if let Some(first_run) = handles.first_run.upgrade() {
                let state = first_run.global::<AppState>();
                state.set_starter_downloading(false);
                let _ = first_run.hide();
            }
        }

        UiEvent::ShowSettings {
            tab,
            config,
            model_statuses,
            show_wayland_notice,
            has_gemini_key,
            has_openai_key,
            has_deepl_key,
        } => {
            if let Ok(mut guard) = last_config.lock() {
                *guard = (*config).clone();
            }
            let ai_configured = resolve_ai_configured(&config, has_gemini_key, has_openai_key);
            if let Some(settings) = handles.settings.upgrade() {
                let state = settings.global::<AppState>();
                apply_config_to_app_state(&state, &config, show_wayland_notice);
                apply_model_statuses_to_app_state(&state, &model_statuses, config.ocr.tier);
                apply_keyring_status_to_app_state(
                    &state,
                    has_gemini_key,
                    has_openai_key,
                    has_deepl_key,
                    ai_configured,
                );
                state.set_ai_testing(false);
                state.set_ai_test_status(SharedString::default());
                state.set_settings_active_tab(tab.clamp(0, 4));
                state.set_settings_save_status(SharedString::default());
                let _ = settings.show();
            }
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                apply_config_to_app_state(&state, &config, show_wayland_notice);
                apply_keyring_status_to_app_state(
                    &state,
                    has_gemini_key,
                    has_openai_key,
                    has_deepl_key,
                    ai_configured,
                );
            }
            update_all_live_states(handles, |state| {
                apply_config_to_app_state(state, &config, show_wayland_notice);
            });
        }

        UiEvent::SettingsSaved {
            config,
            status_text,
        } => {
            if let Ok(mut guard) = last_config.lock() {
                *guard = (*config).clone();
            }
            let show_wayland_notice = detect_display_server().is_wayland();
            if let Some(settings) = handles.settings.upgrade() {
                let state = settings.global::<AppState>();
                let has_gemini = state.get_has_gemini_key();
                let has_openai = state.get_has_openai_key();
                apply_config_to_app_state(&state, &config, show_wayland_notice);
                state.set_ai_provider_configured(resolve_ai_configured(
                    &config, has_gemini, has_openai,
                ));
                state.set_settings_save_status(SharedString::from(status_text));
            }
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                let has_gemini = state.get_has_gemini_key();
                let has_openai = state.get_has_openai_key();
                apply_config_to_app_state(&state, &config, show_wayland_notice);
                state.set_ai_provider_configured(resolve_ai_configured(
                    &config, has_gemini, has_openai,
                ));
            }
            update_all_live_states(handles, |state| {
                apply_config_to_app_state(state, &config, show_wayland_notice);
            });
        }

        UiEvent::KeyringStatusUpdated {
            has_gemini,
            has_openai,
            has_deepl,
            ai_provider_configured,
            status_text,
        } => {
            if let Some(settings) = handles.settings.upgrade() {
                let state = settings.global::<AppState>();
                apply_keyring_status_to_app_state(
                    &state,
                    has_gemini,
                    has_openai,
                    has_deepl,
                    ai_provider_configured,
                );
                state.set_settings_save_status(SharedString::from(status_text.as_str()));
            }
            if let Some(snip) = handles.snip.upgrade() {
                let state = snip.global::<AppState>();
                apply_keyring_status_to_app_state(
                    &state,
                    has_gemini,
                    has_openai,
                    has_deepl,
                    ai_provider_configured,
                );
            }
        }

        UiEvent::ProviderTestFinished { ok, message } => {
            if let Some(settings) = handles.settings.upgrade() {
                let state = settings.global::<AppState>();
                state.set_ai_testing(false);
                let formatted = if ok {
                    format!("✓ {message}")
                } else {
                    format!("✗ {message}")
                };
                state.set_ai_test_status(SharedString::from(formatted));
            }
        }

        UiEvent::ModelPackDownloadProgress {
            pack,
            tier,
            progress,
            status_text,
        } => {
            if let Some(settings) = handles.settings.upgrade() {
                update_pack_download_progress_on_state(
                    &settings.global::<AppState>(),
                    pack,
                    tier,
                    progress,
                    &status_text,
                );
            }
        }

        UiEvent::ModelPacksUpdated(statuses) => {
            if let Some(settings) = handles.settings.upgrade() {
                let state = settings.global::<AppState>();
                let active_tier = ocr_tier_from_index(state.get_ocr_tier_index());
                apply_model_statuses_to_app_state(&state, &statuses, active_tier);
            }
        }

        UiEvent::LiveSessionStarted {
            display_mode,
            can_inplace,
            overlay_monitor,
            overlay_physical_rect,
            notice,
        } => {
            {
                let mut rt = match live_runtime.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                rt.display_mode = display_mode;
                rt.overlay_visible = true;
                rt.overlay_monitor = Some(overlay_monitor.clone());
                rt.overlay_physical_rect = overlay_physical_rect;
            }

            let status = notice
                .as_deref()
                .unwrap_or("Đang theo dõi vùng đọc truyện...");
            let mode_id = live_display_mode_to_id(display_mode);

            update_all_live_states(handles, |state| {
                state.set_live_active(true);
                state.set_live_paused(false);
                state.set_live_overlay_visible(true);
                state.set_live_display_mode(SharedString::from(mode_id));
                state.set_live_can_inplace(can_inplace);
                state.set_live_rate_limited(false);
                state.set_live_error_text(SharedString::default());
                state.set_live_status_text(SharedString::from(status));
                state.set_live_blocks(ModelRc::new(VecModel::<LiveTranslatedBlockRow>::default()));
            });

            if let Some(controls) = handles.live_controls.upgrade() {
                position_live_controls_window(&controls, &overlay_monitor, overlay_physical_rect);
                let _ = controls.show();
                apply_native_overlay_style(
                    handles.live_controls.clone(),
                    OverlayStyle::live_panel(),
                    overlay_configurator,
                    "LiveControlsWindow",
                );
            }

            match display_mode {
                LiveDisplayMode::InPlace => {
                    if let Some(panel) = handles.live_panel.upgrade() {
                        let _ = panel.hide();
                    }
                    if let Some(inplace) = handles.live_inplace.upgrade() {
                        position_live_inplace_window(
                            &inplace,
                            &overlay_monitor,
                            overlay_physical_rect,
                        );
                        let _ = inplace.show();
                        apply_native_overlay_style(
                            handles.live_inplace.clone(),
                            OverlayStyle::live_inplace(),
                            overlay_configurator,
                            "LiveInPlaceWindow",
                        );
                    }
                }
                LiveDisplayMode::FloatingPanel => {
                    if let Some(inplace) = handles.live_inplace.upgrade() {
                        let _ = inplace.hide();
                    }
                    if let Some(panel) = handles.live_panel.upgrade() {
                        position_live_panel_window(&panel, &overlay_monitor, overlay_physical_rect);
                        let _ = panel.show();
                        apply_native_overlay_style(
                            handles.live_panel.clone(),
                            OverlayStyle::live_panel(),
                            overlay_configurator,
                            "LivePanelWindow",
                        );
                    }
                }
            }
        }

        UiEvent::LiveBlocksUpdated {
            blocks,
            status_text,
        } => {
            let rows = live_blocks_to_rows(&blocks);
            update_all_live_states(handles, |state| {
                state.set_live_blocks(ModelRc::new(VecModel::from(rows.clone())));
                state.set_live_status_text(SharedString::from(status_text.as_str()));
                state.set_live_rate_limited(false);
                state.set_live_error_text(SharedString::default());
            });
        }

        UiEvent::LivePauseChanged(paused) => {
            let status = if paused {
                "Live Translate đang tạm dừng"
            } else {
                "Đang theo dõi vùng đọc truyện..."
            };
            update_all_live_states(handles, |state| {
                state.set_live_paused(paused);
                state.set_live_status_text(SharedString::from(status));
            });
        }

        UiEvent::LiveVisibilityChanged(visible) => {
            let mode = {
                let mut rt = match live_runtime.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                rt.overlay_visible = visible;
                rt.display_mode
            };

            update_all_live_states(handles, |state| {
                state.set_live_overlay_visible(visible);
            });

            match mode {
                LiveDisplayMode::InPlace => {
                    if let Some(inplace) = handles.live_inplace.upgrade() {
                        if visible {
                            let _ = inplace.show();
                            apply_native_overlay_style(
                                handles.live_inplace.clone(),
                                OverlayStyle::live_inplace(),
                                overlay_configurator,
                                "LiveInPlaceWindow",
                            );
                        } else {
                            let _ = inplace.hide();
                        }
                    }
                }
                LiveDisplayMode::FloatingPanel => {
                    if let Some(panel) = handles.live_panel.upgrade() {
                        if visible {
                            let _ = panel.show();
                            apply_native_overlay_style(
                                handles.live_panel.clone(),
                                OverlayStyle::live_panel(),
                                overlay_configurator,
                                "LivePanelWindow",
                            );
                        } else {
                            let _ = panel.hide();
                        }
                    }
                }
            }
        }

        UiEvent::LiveDisplayModeChanged(mode) => {
            let (visible, monitor, rect) = {
                let mut rt = match live_runtime.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                rt.display_mode = mode;
                (
                    rt.overlay_visible,
                    rt.overlay_monitor.clone(),
                    rt.overlay_physical_rect,
                )
            };

            let mode_id = live_display_mode_to_id(mode);
            update_all_live_states(handles, |state| {
                state.set_live_display_mode(SharedString::from(mode_id));
            });

            match mode {
                LiveDisplayMode::InPlace => {
                    if let Some(panel) = handles.live_panel.upgrade() {
                        let _ = panel.hide();
                    }
                    if visible && let Some(inplace) = handles.live_inplace.upgrade() {
                        if let Some(mon) = &monitor {
                            position_live_inplace_window(&inplace, mon, rect);
                        }
                        let _ = inplace.show();
                        apply_native_overlay_style(
                            handles.live_inplace.clone(),
                            OverlayStyle::live_inplace(),
                            overlay_configurator,
                            "LiveInPlaceWindow",
                        );
                    }
                }
                LiveDisplayMode::FloatingPanel => {
                    if let Some(inplace) = handles.live_inplace.upgrade() {
                        let _ = inplace.hide();
                    }
                    if visible && let Some(panel) = handles.live_panel.upgrade() {
                        if let Some(mon) = &monitor {
                            position_live_panel_window(&panel, mon, rect);
                        }
                        let _ = panel.show();
                        apply_native_overlay_style(
                            handles.live_panel.clone(),
                            OverlayStyle::live_panel(),
                            overlay_configurator,
                            "LivePanelWindow",
                        );
                    }
                }
            }
        }

        UiEvent::LiveRateLimited { message } => {
            update_all_live_states(handles, |state| {
                state.set_live_rate_limited(true);
                state.set_live_error_text(SharedString::from(message.as_str()));
                state.set_live_status_text(SharedString::from("Bộ dịch bị giới hạn tốc độ (429)"));
            });
        }

        UiEvent::LiveSessionStopped => {
            {
                let mut rt = match live_runtime.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                rt.overlay_visible = true;
            }
            update_all_live_states(handles, |state| {
                state.set_live_active(false);
                state.set_live_paused(false);
                state.set_live_rate_limited(false);
                state.set_live_error_text(SharedString::default());
                state.set_live_status_text(SharedString::default());
                state.set_live_blocks(ModelRc::new(VecModel::<LiveTranslatedBlockRow>::default()));
            });
            if let Some(inplace) = handles.live_inplace.upgrade() {
                let _ = inplace.hide();
            }
            if let Some(panel) = handles.live_panel.upgrade() {
                let _ = panel.hide();
            }
            if let Some(controls) = handles.live_controls.upgrade() {
                let _ = controls.hide();
            }
        }

        UiEvent::ShowToast { message } => {
            if let Some(snip) = handles.snip.upgrade() {
                snip.global::<AppState>()
                    .set_toast_text(SharedString::from(message.as_str()));
            }
            if let Some(settings) = handles.settings.upgrade() {
                settings
                    .global::<AppState>()
                    .set_settings_save_status(SharedString::from(message.as_str()));
            }
            if let Some(panel) = handles.live_panel.upgrade() {
                let state = panel.global::<AppState>();
                if state.get_live_active() {
                    state.set_live_status_text(SharedString::from(message));
                }
            }
        }

        UiEvent::Quit => {
            let _ = slint::quit_event_loop();
        }
    }
}
