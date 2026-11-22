//! Platform abstraction layer for `square-to-search`: display server detection, screen and window
//! capture, native overlay window styling, OS clipboard, global hotkeys, and system tray
//! integration.

pub mod capture;
pub mod clipboard;
pub mod display_env;
pub mod hotkeys;
pub mod instance;
pub mod overlay;
pub mod secrets;
pub mod tray;
pub mod wayland;

pub use capture::{ScreenCapturer, WindowGeometry, XcapCapturer, crop_frame};
pub use clipboard::{ArboardClipboard, ClipboardService};
pub use display_env::{
    DisplayServer, WaylandCompositor, detect_display_server, detect_display_server_from_env,
};
pub use hotkeys::{
    GlobalHotkeyService, HotkeyAction, HotkeyBinding, HotkeyBindingFailure, HotkeyManager,
    HotkeyRegistrationReport, parse_hotkey_binding,
};
pub use instance::SingleInstanceGuard;
pub use overlay::{
    EffectiveLiveMode, NativeOverlayConfigurator, OverlayStyle, OverlayWindowConfigurator,
    is_rect_fullscreen_or_maximized, resolve_effective_live_mode,
    resolve_effective_live_mode_for_target,
};
pub use secrets::{KeyringSecretStore, MemorySecretStore, SecretStore};
pub use tray::{TrayCommand, TrayService};
#[cfg(target_os = "linux")]
pub use wayland::{
    WaylandBoundShortcut, WaylandGlobalShortcutsPortal, WaylandScreenCastPortal,
    WaylandScreenCastSession, WaylandScreenCastStream, WaylandScreenshotPortal,
};
pub use wayland::{
    WaylandLayerSurfaceConfig, WaylandPortalCapabilities, WaylandShortcutSpec,
    default_wayland_shortcut_specs,
};

/// Errors produced by `sts-platform` OS and display integration operations.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// Requested crop rectangle had zero width or height after clamping to frame bounds.
    #[error("invalid crop rectangle: resulting width or height is zero")]
    InvalidCropRect,

    /// Input framebuffer byte length did not match `width * height * 4`.
    #[error(
        "invalid RGBA frame buffer for {width}x{height}: expected {expected} bytes, got {actual}"
    )]
    InvalidFrameBuffer {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },

    /// No active monitors were reported by the OS display subsystem.
    #[error("no monitors found")]
    NoMonitorsFound,

    /// Requested monitor identifier was not found among active displays.
    #[error("monitor with id {0} not found")]
    MonitorNotFound(u32),

    /// Requested OS window identifier was not found among capturable windows.
    #[error("window with id {0} not found")]
    WindowNotFound(u32),

    /// Screen capture or monitor/window enumeration failed.
    #[error("screen capture failed: {0}")]
    CaptureFailed(String),

    /// Native overlay window styling (click-through, capture exclusion, or topmost) failed.
    #[error("overlay window styling failed: {0}")]
    OverlayStyleFailed(String),

    /// OS clipboard initialization or write failed.
    #[error("clipboard operation failed: {0}")]
    ClipboardFailed(String),

    /// Hotkey binding string could not be parsed into a valid shortcut.
    #[error("invalid hotkey binding `{spec}`: {reason}")]
    InvalidHotkey { spec: String, reason: String },

    /// Global hotkey registration, unregistration, or manager creation failed.
    #[error("global hotkey operation failed: {0}")]
    HotkeyFailed(String),

    /// System tray icon or menu creation failed.
    #[error("system tray operation failed: {0}")]
    TrayFailed(String),

    /// OS keyring or secret store operation failed.
    #[error("secret store operation failed: {0}")]
    SecretStoreFailed(String),

    /// Linux Wayland desktop portal operation failed.
    #[error("wayland portal operation failed: {0}")]
    WaylandPortalFailed(String),

    /// Single-instance advisory lock initialization or acquisition failed.
    #[error("single-instance lock failed: {0}")]
    SingleInstanceFailed(String),
}
