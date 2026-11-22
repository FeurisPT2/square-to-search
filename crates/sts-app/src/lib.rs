//! Application orchestration, async state machine, and Slint UI bridge for `square-to-search`.

pub mod live;
pub mod state;
pub mod ui_bridge;

#[allow(clippy::all, clippy::unwrap_used, clippy::expect_used)]
mod slint_generated {
    slint::include_modules!();
}

pub use live::{
    DEFAULT_MIN_FONT_SIZE, LiveLoopConfig, LiveRenderedBlock, LiveSessionHandle, LiveTarget,
    spawn_live_session,
};
pub use slint_generated::*;
pub use state::{
    AppCommand, AppController, AppServices, BoxFuture, DefaultProviderBuilder, PackDownloader,
    PendingSnipAction, ProgressCallback, ProviderBuilder, ProviderTestTarget, SnipActionKind,
    UiEvent, VisualSearcher, encode_frame_as_png, load_provider_secrets, pack_from_id, pack_to_id,
    tier_from_id, tier_to_id,
};
pub use ui_bridge::{
    UiWindows, apply_config_to_app_state, apply_keyring_status_to_app_state,
    apply_model_statuses_to_app_state, build_model_pack_rows, capture_frame_to_slint_image,
    clear_key_inputs_on_app_state, live_blocks_to_rows, live_display_mode_to_id,
    read_config_from_app_state, rgba_to_slint_color, setup_ui_bridge,
};
