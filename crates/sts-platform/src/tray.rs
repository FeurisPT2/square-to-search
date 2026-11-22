//! System tray icon and 4-item context menu forwarding commands to Tokio channels.

#[cfg(any(target_os = "windows", target_os = "linux"))]
use std::sync::{Mutex, Once, OnceLock};

use tokio::sync::mpsc::UnboundedSender;
#[cfg(any(target_os = "windows", target_os = "linux"))]
use tracing::{debug, warn};
#[cfg(any(target_os = "windows", target_os = "linux"))]
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(any(target_os = "windows", target_os = "linux"))]
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::PlatformError;

/// Default tooltip shown on the system tray icon.
pub const DEFAULT_TRAY_TOOLTIP: &str = "Square to Search";

/// Menu item identifier for *Square to Search*.
pub const MENU_ID_SQUARE_TO_SEARCH: &str = "sts.square_to_search";
/// Menu item identifier for *Live Translate*.
pub const MENU_ID_LIVE_TRANSLATE: &str = "sts.live_translate";
/// Menu item identifier for *Settings*.
pub const MENU_ID_SETTINGS: &str = "sts.settings";
/// Menu item identifier for *Quit*.
pub const MENU_ID_QUIT: &str = "sts.quit";

/// Commands emitted when the user activates a system tray menu item (Spec §5.2.A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayCommand {
    /// Start Square to Search freeze-frame snipping.
    SquareToSearch,
    /// Toggle Continuous Live Translate.
    ToggleLiveTranslate,
    /// Open the 5-tab Settings window.
    OpenSettings,
    /// Exit the background tray application.
    Quit,
}

/// Maps a tray menu item identifier string back to its corresponding [`TrayCommand`].
#[must_use]
pub fn command_from_menu_id(id: &str) -> Option<TrayCommand> {
    match id {
        MENU_ID_SQUARE_TO_SEARCH => Some(TrayCommand::SquareToSearch),
        MENU_ID_LIVE_TRANSLATE => Some(TrayCommand::ToggleLiveTranslate),
        MENU_ID_SETTINGS => Some(TrayCommand::OpenSettings),
        MENU_ID_QUIT => Some(TrayCommand::Quit),
        _ => None,
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
static TRAY_SENDER: OnceLock<Mutex<Option<UnboundedSender<TrayCommand>>>> = OnceLock::new();
#[cfg(any(target_os = "windows", target_os = "linux"))]
static TRAY_HANDLER_INSTALLED: Once = Once::new();

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn tray_sender() -> &'static Mutex<Option<UnboundedSender<TrayCommand>>> {
    TRAY_SENDER.get_or_init(|| Mutex::new(None))
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn handle_menu_event(event: MenuEvent) {
    let Some(command) = command_from_menu_id(event.id.as_ref()) else {
        return;
    };

    let Ok(guard) = tray_sender().lock() else {
        warn!("tray event sender mutex poisoned");
        return;
    };

    if let Some(tx) = guard.as_ref()
        && tx.send(command).is_err()
    {
        debug!("tray command channel closed before delivering {command:?}");
    }
}

/// Generates a crisp 32x32 RGBA tray icon (indigo rounded badge with white selection frame).
fn default_tray_icon_rgba() -> (Vec<u8>, u32, u32) {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE as usize) * (SIZE as usize) * 4];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let idx = ((y * SIZE + x) as usize) * 4;
            let in_rounded_box = (4..SIZE - 4).contains(&x) || (4..SIZE - 4).contains(&y);
            if !in_rounded_box {
                continue;
            }

            let on_frame_edge = (x == 8 || x == 9 || x == SIZE - 9 || x == SIZE - 10)
                && (8..=SIZE - 9).contains(&y)
                || (y == 8 || y == 9 || y == SIZE - 9 || y == SIZE - 10)
                    && (8..=SIZE - 9).contains(&x);

            let pixel = if on_frame_edge {
                [255, 255, 255, 255]
            } else {
                [79, 70, 229, 255]
            };

            if let Some(slot) = rgba.get_mut(idx..idx + 4) {
                slot.copy_from_slice(&pixel);
            }
        }
    }

    (rgba, SIZE, SIZE)
}

/// Owns the OS system tray icon and forwards menu activations to an [`UnboundedSender`].
pub struct TrayService {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    tray_icon: TrayIcon,
}

impl TrayService {
    /// Creates the system tray icon with the 4 required menu items (*Square to Search*,
    /// *Live Translate*, *Settings*, *Quit*) and wires `MenuEvent` forwarding to `tx`.
    pub fn new(tx: UnboundedSender<TrayCommand>) -> Result<Self, PlatformError> {
        Self::with_tooltip(tx, DEFAULT_TRAY_TOOLTIP)
    }

    /// Creates the system tray icon with a custom tooltip string.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub fn with_tooltip(
        tx: UnboundedSender<TrayCommand>,
        tooltip: &str,
    ) -> Result<Self, PlatformError> {
        let square_item =
            MenuItem::with_id(MENU_ID_SQUARE_TO_SEARCH, "Square to Search", true, None);
        let live_item = MenuItem::with_id(MENU_ID_LIVE_TRANSLATE, "Live Translate", true, None);
        let separator = PredefinedMenuItem::separator();
        let settings_item = MenuItem::with_id(MENU_ID_SETTINGS, "Settings", true, None);
        let quit_item = MenuItem::with_id(MENU_ID_QUIT, "Quit", true, None);

        let menu = Menu::new();
        menu.append_items(&[
            &square_item,
            &live_item,
            &separator,
            &settings_item,
            &quit_item,
        ])
        .map_err(|err| PlatformError::TrayFailed(err.to_string()))?;

        let (rgba, width, height) = default_tray_icon_rgba();
        let icon = Icon::from_rgba(rgba, width, height)
            .map_err(|err| PlatformError::TrayFailed(err.to_string()))?;

        {
            let mut guard = tray_sender().lock().map_err(|_| {
                PlatformError::TrayFailed(String::from("tray event sender mutex poisoned"))
            })?;
            *guard = Some(tx);
        }

        TRAY_HANDLER_INSTALLED.call_once(|| {
            MenuEvent::set_event_handler(Some(handle_menu_event));
        });

        let tray_icon = TrayIconBuilder::new()
            .with_tooltip(tooltip)
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .build()
            .map_err(|err| PlatformError::TrayFailed(err.to_string()))?;

        Ok(Self { tray_icon })
    }

    /// Creates the system tray icon with a custom tooltip string.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    pub fn with_tooltip(
        _tx: UnboundedSender<TrayCommand>,
        _tooltip: &str,
    ) -> Result<Self, PlatformError> {
        Err(PlatformError::TrayFailed(String::from(
            "system tray is only supported on Windows and Linux",
        )))
    }

    /// Updates the tooltip displayed when hovering over the tray icon.
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    pub fn set_tooltip(&self, tooltip: &str) -> Result<(), PlatformError> {
        self.tray_icon
            .set_tooltip(Some(tooltip))
            .map_err(|err| PlatformError::TrayFailed(err.to_string()))
    }

    /// Updates the tooltip displayed when hovering over the tray icon.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    pub fn set_tooltip(&self, _tooltip: &str) -> Result<(), PlatformError> {
        Err(PlatformError::TrayFailed(String::from(
            "system tray is only supported on Windows and Linux",
        )))
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
impl Drop for TrayService {
    fn drop(&mut self) {
        if let Ok(mut guard) = tray_sender().lock() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_all_four_tray_menu_ids_to_commands() {
        assert_eq!(
            command_from_menu_id(MENU_ID_SQUARE_TO_SEARCH),
            Some(TrayCommand::SquareToSearch)
        );
        assert_eq!(
            command_from_menu_id(MENU_ID_LIVE_TRANSLATE),
            Some(TrayCommand::ToggleLiveTranslate)
        );
        assert_eq!(
            command_from_menu_id(MENU_ID_SETTINGS),
            Some(TrayCommand::OpenSettings)
        );
        assert_eq!(command_from_menu_id(MENU_ID_QUIT), Some(TrayCommand::Quit));
        assert_eq!(command_from_menu_id("sts.unknown"), None);
    }

    #[test]
    fn default_tray_icon_rgba_has_valid_32x32_dimensions() {
        let (rgba, width, height) = default_tray_icon_rgba();
        assert_eq!((width, height), (32, 32));
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert!(rgba.iter().any(|&b| b != 0));
    }
}
