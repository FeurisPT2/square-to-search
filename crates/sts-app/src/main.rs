//! Desktop binary entrypoint for `square-to-search`.

use std::sync::Arc;
use std::time::Duration;

use slint::ComponentHandle;
use sts_app::AppState;
use sts_app::state::{
    AppCommand, AppController, AppServices, DefaultProviderBuilder, PackDownloader,
    ProviderBuilder, VisualSearcher, load_provider_secrets,
};
use sts_app::ui_bridge::{apply_keyring_status_to_app_state, setup_ui_bridge};
use sts_core::cache::TranslationCache;
use sts_core::config::ConfigStore;
use sts_ocr::downloader::ModelDownloader;
use sts_ocr::engine::{IdleManagedOcrEngine, OarOcrEngine, OcrEngine};
use sts_ocr::manifest::{ModelManifest, ModelStore};
use sts_platform::capture::{ScreenCapturer, XcapCapturer};
use sts_platform::clipboard::{ArboardClipboard, ClipboardService};
use sts_platform::display_env::detect_display_server;
use sts_platform::hotkeys::{GlobalHotkeyService, HotkeyAction, HotkeyBinding};
use sts_platform::overlay::{NativeOverlayConfigurator, OverlayWindowConfigurator};
use sts_platform::secrets::{KeyringSecretStore, SecretStore};
use sts_platform::tray::{TrayCommand, TrayService};
use sts_search::{BrowserOpener, SystemBrowserOpener, VisualSearchService};
use sts_translate::{CachedFallbackTranslator, ProviderSecrets, Translator};
use tokio::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

fn init_tracing() {
    let _ = tracing_subscriber::fmt().with_target(false).try_init();
}

fn select_slint_winit_backend() {
    if let Err(err) = slint::BackendSelector::new()
        .backend_name("winit".into())
        .select()
    {
        tracing::warn!(
            error = %err,
            "failed to explicitly select Slint winit backend; continuing with default backend"
        );
    }
}

fn spawn_tray_forwarder(cmd_tx: UnboundedSender<AppCommand>) -> Option<TrayService> {
    let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel::<TrayCommand>();
    match TrayService::new(tray_tx) {
        Ok(tray) => {
            tokio::spawn(async move {
                while let Some(tray_cmd) = tray_rx.recv().await {
                    let mapped = match tray_cmd {
                        TrayCommand::SquareToSearch => AppCommand::TriggerSquareToSearch,
                        TrayCommand::ToggleLiveTranslate => AppCommand::ToggleLiveTranslate,
                        TrayCommand::OpenSettings => AppCommand::OpenSettingsTab(0),
                        TrayCommand::Quit => AppCommand::Quit,
                    };
                    if cmd_tx.send(mapped).is_err() {
                        break;
                    }
                }
            });
            Some(tray)
        }
        Err(err) => {
            tracing::warn!(error = %err, "system tray unavailable in current environment");
            None
        }
    }
}

fn spawn_hotkey_forwarder(
    cmd_tx: UnboundedSender<AppCommand>,
    bindings: &[HotkeyBinding],
) -> Option<GlobalHotkeyService> {
    let (hk_tx, mut hk_rx) = tokio::sync::mpsc::unbounded_channel::<HotkeyAction>();
    match GlobalHotkeyService::with_bindings(hk_tx, bindings) {
        Ok(service) => {
            tokio::spawn(async move {
                while let Some(action) = hk_rx.recv().await {
                    let mapped = match action {
                        HotkeyAction::SquareToSearch => AppCommand::TriggerSquareToSearch,
                        HotkeyAction::ToggleLiveTranslate => AppCommand::ToggleLiveTranslate,
                        HotkeyAction::ToggleLiveVisibility => AppCommand::ToggleLiveVisibility,
                    };
                    if cmd_tx.send(mapped).is_err() {
                        break;
                    }
                }
            });
            Some(service)
        }
        Err(err) => {
            tracing::warn!(error = %err, "global hotkeys unavailable in current environment");
            None
        }
    }
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    select_slint_winit_backend();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _rt_guard = runtime.enter();

    let config_path = ConfigStore::default_os_path()?;
    let config_store = ConfigStore::new(config_path);
    let initial_config = config_store.load_or_default()?.into_config();

    let models_dir = ModelStore::default_os_dir()?;
    let model_store = ModelStore::new(models_dir);
    let model_manifest = ModelManifest::builtin();
    let initial_statuses = model_store.list_statuses(&model_manifest);

    let downloader: Arc<dyn PackDownloader> = Arc::new(ModelDownloader::new()?);
    let capturer: Arc<dyn ScreenCapturer> = Arc::new(XcapCapturer::new());
    let clipboard: Arc<dyn ClipboardService> = Arc::new(ArboardClipboard::new());
    let display_server = detect_display_server();

    let idle_timeout = Duration::from_secs(initial_config.ocr.idle_unload_secs.max(60));
    let oar_backend = OarOcrEngine::new(model_store.clone(), initial_config.ocr.device);
    let managed_ocr = Arc::new(IdleManagedOcrEngine::with_idle_timeout(
        oar_backend,
        idle_timeout,
    ));
    let _reaper_handle = managed_ocr.spawn_idle_reaper(Duration::from_secs(30));
    let ocr: Arc<dyn OcrEngine> = managed_ocr;

    let secrets: Arc<dyn SecretStore> = Arc::new(KeyringSecretStore::new());
    let provider_builder: Arc<dyn ProviderBuilder> = Arc::new(DefaultProviderBuilder);
    let translation_cache = Arc::new(Mutex::new(TranslationCache::new(
        initial_config.translate.cache_capacity,
    )));
    let initial_secrets = match runtime.block_on(load_provider_secrets(&secrets)) {
        Ok(loaded) => loaded,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to read BYOK secrets from OS keyring on startup; continuing without keys"
            );
            ProviderSecrets::default()
        }
    };
    let translator: Arc<dyn Translator> = match provider_builder.build_translator(
        &initial_config.translate,
        &initial_secrets,
        Arc::clone(&translation_cache),
    ) {
        Ok(built) => built,
        Err(err) => {
            tracing::warn!(
                error = %err,
                provider = %initial_config.translate.provider,
                "configured translator unavailable on startup; falling back to Free Web"
            );
            Arc::new(CachedFallbackTranslator::with_default_free_providers(
                Arc::clone(&translation_cache),
            ))
        }
    };

    let browser: Arc<dyn BrowserOpener> = Arc::new(SystemBrowserOpener::new());
    let visual_search: Arc<dyn VisualSearcher> = Arc::new(VisualSearchService::new(
        Arc::clone(&browser),
        Arc::clone(&clipboard),
    )?);

    let services = AppServices {
        config_store,
        model_store,
        model_manifest,
        downloader,
        capturer,
        clipboard,
        ocr,
        translator,
        visual_search,
        browser,
        display_server,
        secrets,
        translation_cache,
        provider_builder: Arc::clone(&provider_builder),
    };

    let (cmd_tx, ui_rx, _controller_handle) = AppController::spawn(services)?;

    let _tray_service = spawn_tray_forwarder(cmd_tx.clone());
    let hotkey_bindings = [
        HotkeyBinding::new(
            HotkeyAction::SquareToSearch,
            initial_config.hotkeys.square_to_search.clone(),
        ),
        HotkeyBinding::new(
            HotkeyAction::ToggleLiveTranslate,
            initial_config.hotkeys.toggle_live_translate.clone(),
        ),
        HotkeyBinding::new(
            HotkeyAction::ToggleLiveVisibility,
            initial_config.hotkeys.toggle_live_visibility.clone(),
        ),
    ];

    #[cfg(target_os = "linux")]
    let _wayland_shortcuts_portal = if display_server.is_wayland() {
        match runtime.block_on(
            sts_platform::WaylandGlobalShortcutsPortal::register_from_config(
                &initial_config.hotkeys,
            ),
        ) {
            Ok(portal) => {
                tracing::info!("registered global shortcuts via Wayland GlobalShortcuts portal");
                Some(portal)
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "Wayland GlobalShortcuts portal unavailable; falling back to global-hotkey"
                );
                None
            }
        }
    } else {
        None
    };

    #[cfg(target_os = "linux")]
    let _hotkey_service = if _wayland_shortcuts_portal.is_none() {
        spawn_hotkey_forwarder(cmd_tx.clone(), &hotkey_bindings)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let _hotkey_service = spawn_hotkey_forwarder(cmd_tx.clone(), &hotkey_bindings);

    let overlay_configurator: Arc<dyn OverlayWindowConfigurator> =
        Arc::new(NativeOverlayConfigurator::new());
    let windows = setup_ui_bridge(
        cmd_tx,
        ui_rx,
        &initial_config,
        &initial_statuses,
        overlay_configurator,
    )?;

    let has_gemini = initial_secrets
        .gemini_api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    let has_openai = initial_secrets
        .openai_api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    let has_deepl = initial_secrets
        .deepl_api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    let ai_configured = provider_builder
        .build_vision_ai(&initial_config.translate, &initial_secrets)
        .ok()
        .flatten()
        .is_some();
    apply_keyring_status_to_app_state(
        &windows.settings.global::<AppState>(),
        has_gemini,
        has_openai,
        has_deepl,
        ai_configured,
    );
    apply_keyring_status_to_app_state(
        &windows.snip_overlay.global::<AppState>(),
        has_gemini,
        has_openai,
        has_deepl,
        ai_configured,
    );

    tracing::info!("square-to-search initialized; entering Slint event loop");
    slint::run_event_loop_until_quit()?;
    Ok(())
}
