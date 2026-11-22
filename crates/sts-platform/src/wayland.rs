//! Linux Wayland capability descriptors and `xdg-desktop-portal` (`ashpd`) adapters.

use std::path::PathBuf;

use sts_core::config::HotkeyConfig;

use crate::PlatformError;
use crate::display_env::DisplayServer;
use crate::hotkeys::{HotkeyAction, HotkeyBinding};

/// Summary of Wayland portal and `wlr-layer-shell` capabilities for a display environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WaylandPortalCapabilities {
    /// Whether `org.freedesktop.portal.GlobalShortcuts` is applicable.
    pub global_shortcuts: bool,
    /// Whether `org.freedesktop.portal.ScreenCast` (PipeWire) is applicable.
    pub screencast: bool,
    /// Whether `org.freedesktop.portal.Screenshot` fallback is applicable.
    pub screenshot_fallback: bool,
    /// Whether `wlr-layer-shell` fullscreen/maximized In-place overlays are supported (Decision 2A).
    pub layer_shell_fullscreen: bool,
}

impl WaylandPortalCapabilities {
    /// Computes Wayland portal and layer-shell capabilities for `display`.
    #[must_use]
    pub const fn for_display(display: DisplayServer) -> Self {
        match display {
            DisplayServer::LinuxWayland { .. } => Self {
                global_shortcuts: true,
                screencast: true,
                screenshot_fallback: true,
                layer_shell_fullscreen: display.supports_wayland_layer_shell(),
            },
            DisplayServer::Windows | DisplayServer::LinuxX11 | DisplayServer::Other => Self {
                global_shortcuts: false,
                screencast: false,
                screenshot_fallback: false,
                layer_shell_fullscreen: false,
            },
        }
    }
}

/// Configuration descriptor for `wlr-layer-shell` fullscreen Live Translate overlays on KDE,
/// Hyprland, and Sway (Decision 2A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WaylandLayerSurfaceConfig {
    /// Anchor surface to top, bottom, left, and right edges of the output.
    pub anchor_all_edges: bool,
    /// Layer-shell exclusive zone (`-1` ignores panels/docks so fullscreen overlays align edge-to-edge).
    pub exclusive_zone: i32,
    /// Keep keyboard interactivity disabled so reader focus is never stolen.
    pub keyboard_interactivity: bool,
    /// Use an empty input region (`wl_region`) for pointer click-through.
    pub input_region_empty: bool,
}

impl Default for WaylandLayerSurfaceConfig {
    fn default() -> Self {
        Self::for_live_inplace()
    }
}

impl WaylandLayerSurfaceConfig {
    /// Returns the fullscreen click-through `wlr-layer-shell` preset for Live Translate In-place.
    #[must_use]
    pub const fn for_live_inplace() -> Self {
        Self {
            anchor_all_edges: true,
            exclusive_zone: -1,
            keyboard_interactivity: false,
            input_region_empty: true,
        }
    }
}

/// Cross-platform descriptor for registering a shortcut with `org.freedesktop.portal.GlobalShortcuts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandShortcutSpec {
    pub id: &'static str,
    pub action: HotkeyAction,
    pub description: &'static str,
    pub preferred_trigger: String,
}

/// Maps application [`HotkeyConfig`] bindings into Wayland GlobalShortcuts descriptors.
#[must_use]
pub fn default_wayland_shortcut_specs(config: &HotkeyConfig) -> Vec<WaylandShortcutSpec> {
    [
        (HotkeyAction::SquareToSearch, &config.square_to_search),
        (
            HotkeyAction::ToggleLiveTranslate,
            &config.toggle_live_translate,
        ),
        (
            HotkeyAction::ToggleLiveVisibility,
            &config.toggle_live_visibility,
        ),
    ]
    .into_iter()
    .map(|(action, trigger)| WaylandShortcutSpec {
        id: HotkeyBinding::portal_id(action),
        action,
        description: HotkeyBinding::description(action),
        preferred_trigger: trigger.trim().to_owned(),
    })
    .collect()
}

/// Formats a user hotkey spec (`"Ctrl+Shift+S"`) into an XDG GlobalShortcuts `preferred_trigger`
/// hint (`"CTRL+SHIFT+S"`). Returns `None` for empty specifications.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn xdg_preferred_trigger(spec: &str) -> Option<String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parts: Vec<String> = trimmed
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let upper = part.to_ascii_uppercase();
            match upper.as_str() {
                "CTRL" | "CONTROL" => String::from("CTRL"),
                "ALT" => String::from("ALT"),
                "SHIFT" => String::from("SHIFT"),
                "SUPER" | "META" | "WIN" | "CMD" | "COMMAND" => String::from("LOGO"),
                _ => upper,
            }
        })
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("+"))
    }
}

/// Decodes a `file://` URI returned by `org.freedesktop.portal.Screenshot` into a local path.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn file_uri_to_local_path(uri: &str) -> Result<PathBuf, PlatformError> {
    let raw_path = if let Some(rest) = uri.strip_prefix("file://localhost") {
        rest
    } else if let Some(rest) = uri.strip_prefix("file://") {
        rest
    } else {
        return Err(PlatformError::WaylandPortalFailed(format!(
            "expected file:// URI from Screenshot portal, got `{uri}`"
        )));
    };

    if raw_path.is_empty() {
        return Err(PlatformError::WaylandPortalFailed(String::from(
            "Screenshot portal returned an empty file:// path",
        )));
    }

    let bytes = raw_path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = bytes.get(idx + 1).and_then(|b| decode_hex_nibble(*b));
            let lo = bytes.get(idx + 2).and_then(|b| decode_hex_nibble(*b));
            match (hi, lo) {
                (Some(h), Some(l)) => {
                    decoded.push((h << 4) | l);
                    idx += 3;
                }
                _ => {
                    return Err(PlatformError::WaylandPortalFailed(format!(
                        "invalid percent-encoding in Screenshot portal URI `{uri}`"
                    )));
                }
            }
        } else {
            decoded.push(bytes[idx]);
            idx += 1;
        }
    }

    let path_str = String::from_utf8(decoded).map_err(|_| {
        PlatformError::WaylandPortalFailed(format!(
            "Screenshot portal URI path is not valid UTF-8: `{uri}`"
        ))
    })?;
    Ok(PathBuf::from(path_str))
}

#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
const fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
mod linux_portals {
    use std::os::fd::OwnedFd;

    use ashpd::desktop::global_shortcuts::{
        BindShortcutsOptions, GlobalShortcuts, ListShortcutsOptions, NewShortcut, Shortcut,
    };
    use ashpd::desktop::screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions,
    };
    use ashpd::desktop::screenshot::Screenshot;
    use ashpd::desktop::{PersistMode, Session};
    use sts_core::config::HotkeyConfig;
    use sts_core::types::CaptureFrame;

    use super::{
        WaylandShortcutSpec, default_wayland_shortcut_specs, file_uri_to_local_path,
        xdg_preferred_trigger,
    };
    use crate::PlatformError;

    fn map_ashpd_error(context: &str, err: ashpd::Error) -> PlatformError {
        PlatformError::WaylandPortalFailed(format!("{context}: {err}"))
    }

    fn map_bound_shortcut(shortcut: &Shortcut) -> WaylandBoundShortcut {
        WaylandBoundShortcut {
            id: shortcut.id().to_owned(),
            description: shortcut.description().to_owned(),
            trigger_description: shortcut.trigger_description().to_owned(),
        }
    }

    /// Metadata describing a bound global shortcut returned by `GlobalShortcuts`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WaylandBoundShortcut {
        pub id: String,
        pub description: String,
        pub trigger_description: String,
    }

    /// Adapter around `org.freedesktop.portal.GlobalShortcuts`.
    #[derive(Debug)]
    pub struct WaylandGlobalShortcutsPortal {
        proxy: GlobalShortcuts,
        session: Session<GlobalShortcuts>,
    }

    impl WaylandGlobalShortcutsPortal {
        /// Creates a portal session and binds `specs` via `org.freedesktop.portal.GlobalShortcuts`.
        pub async fn register(specs: &[WaylandShortcutSpec]) -> Result<Self, PlatformError> {
            let proxy = GlobalShortcuts::new()
                .await
                .map_err(|err| map_ashpd_error("connect GlobalShortcuts portal", err))?;
            let session = proxy
                .create_session(Default::default())
                .await
                .map_err(|err| map_ashpd_error("create GlobalShortcuts session", err))?;

            let shortcuts: Vec<NewShortcut> = specs
                .iter()
                .map(|spec| {
                    let trigger = xdg_preferred_trigger(&spec.preferred_trigger);
                    NewShortcut::new(spec.id, spec.description)
                        .preferred_trigger(trigger.as_deref())
                })
                .collect();

            let request = match proxy
                .bind_shortcuts(&session, &shortcuts, None, BindShortcutsOptions::default())
                .await
            {
                Ok(request) => request,
                Err(err) => {
                    let _ = session.close().await;
                    return Err(map_ashpd_error("bind GlobalShortcuts", err));
                }
            };

            let bound = match request.response() {
                Ok(bound) => bound,
                Err(err) => {
                    let _ = session.close().await;
                    return Err(map_ashpd_error("GlobalShortcuts bind response", err));
                }
            };

            tracing::debug!(
                shortcut_count = bound.shortcuts().len(),
                "registered Wayland GlobalShortcuts session"
            );

            Ok(Self { proxy, session })
        }

        /// Convenience wrapper mapping [`HotkeyConfig`] through [`default_wayland_shortcut_specs`].
        pub async fn register_from_config(config: &HotkeyConfig) -> Result<Self, PlatformError> {
            let specs = default_wayland_shortcut_specs(config);
            Self::register(&specs).await
        }

        /// Lists shortcuts currently bound to this portal session.
        pub async fn list_shortcuts(&self) -> Result<Vec<WaylandBoundShortcut>, PlatformError> {
            let request = self
                .proxy
                .list_shortcuts(&self.session, ListShortcutsOptions::default())
                .await
                .map_err(|err| map_ashpd_error("list GlobalShortcuts", err))?;
            let response = request
                .response()
                .map_err(|err| map_ashpd_error("GlobalShortcuts list response", err))?;
            Ok(response
                .shortcuts()
                .iter()
                .map(map_bound_shortcut)
                .collect())
        }

        /// Borrows the underlying `ashpd` [`GlobalShortcuts`] proxy (e.g., to listen to
        /// `receive_activated()`).
        #[must_use]
        pub const fn proxy(&self) -> &GlobalShortcuts {
            &self.proxy
        }

        /// Borrows the active `GlobalShortcuts` portal [`Session`].
        #[must_use]
        pub const fn session(&self) -> &Session<GlobalShortcuts> {
            &self.session
        }

        /// Explicitly closes the `GlobalShortcuts` portal session.
        pub async fn close(self) -> Result<(), PlatformError> {
            self.session
                .close()
                .await
                .map_err(|err| map_ashpd_error("close GlobalShortcuts session", err))
        }
    }

    /// Adapter around `org.freedesktop.portal.Screenshot`.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct WaylandScreenshotPortal;

    impl WaylandScreenshotPortal {
        /// Requests a screenshot via `org.freedesktop.portal.Screenshot` and returns its URI.
        pub async fn capture_uri(interactive: bool, modal: bool) -> Result<String, PlatformError> {
            let request = Screenshot::request()
                .interactive(interactive)
                .modal(modal)
                .send()
                .await
                .map_err(|err| map_ashpd_error("request Screenshot portal", err))?;
            let response = request
                .response()
                .map_err(|err| map_ashpd_error("Screenshot portal response", err))?;
            Ok(response.uri().as_str().to_owned())
        }

        /// Requests a screenshot via `org.freedesktop.portal.Screenshot` and decodes the resulting
        /// image file into an RGBA8 [`CaptureFrame`].
        pub async fn capture_frame(
            interactive: bool,
            modal: bool,
        ) -> Result<CaptureFrame, PlatformError> {
            let uri = Self::capture_uri(interactive, modal).await?;
            let path = file_uri_to_local_path(&uri)?;

            tokio::task::spawn_blocking(move || {
                let reader = image::ImageReader::open(&path).map_err(|err| {
                    PlatformError::WaylandPortalFailed(format!(
                        "failed to open Screenshot portal image `{}`: {err}",
                        path.display()
                    ))
                })?;
                let decoded = reader.decode().map_err(|err| {
                    PlatformError::WaylandPortalFailed(format!(
                        "failed to decode Screenshot portal image `{}`: {err}",
                        path.display()
                    ))
                })?;
                let rgba = decoded.into_rgba8();
                Ok(CaptureFrame {
                    width: rgba.width(),
                    height: rgba.height(),
                    scale_factor: 1.0,
                    rgba: rgba.into_raw(),
                })
            })
            .await
            .map_err(|err| {
                PlatformError::WaylandPortalFailed(format!(
                    "Screenshot image decoding task failed: {err}"
                ))
            })?
        }
    }

    /// Metadata for a single PipeWire stream returned by `ScreenCast`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WaylandScreenCastStream {
        pub pipewire_node_id: u32,
        pub position: Option<(i32, i32)>,
        pub size: Option<(i32, i32)>,
        pub is_window: bool,
        pub is_monitor: bool,
        pub mapping_id: Option<String>,
        pub pipewire_serial: Option<u64>,
    }

    /// Active `org.freedesktop.portal.ScreenCast` session with PipeWire remote file descriptor.
    #[derive(Debug)]
    pub struct WaylandScreenCastSession {
        _proxy: Screencast,
        session: Session<Screencast>,
        pub streams: Vec<WaylandScreenCastStream>,
        pub restore_token: Option<String>,
        pub pipewire_fd: OwnedFd,
    }

    impl WaylandScreenCastSession {
        /// Closes the underlying `ScreenCast` portal session.
        pub async fn close(self) -> Result<(), PlatformError> {
            self.session
                .close()
                .await
                .map_err(|err| map_ashpd_error("close ScreenCast session", err))
        }
    }

    /// Adapter around `org.freedesktop.portal.ScreenCast` requesting window or monitor capture.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct WaylandScreenCastPortal;

    impl WaylandScreenCastPortal {
        /// Starts a `ScreenCast` session requesting `SourceType::WINDOW | SourceType::MONITOR` and
        /// opens the PipeWire remote file descriptor.
        pub async fn start_capture(
            restore_token: Option<&str>,
        ) -> Result<WaylandScreenCastSession, PlatformError> {
            let proxy = Screencast::new()
                .await
                .map_err(|err| map_ashpd_error("connect ScreenCast portal", err))?;
            let session = proxy
                .create_session(Default::default())
                .await
                .map_err(|err| map_ashpd_error("create ScreenCast session", err))?;

            let options = SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::HIDDEN)
                .set_sources(SourceType::WINDOW | SourceType::MONITOR)
                .set_multiple(false)
                .set_persist_mode(PersistMode::Application)
                .set_restore_token(restore_token);

            let select_request = match proxy.select_sources(&session, options).await {
                Ok(request) => request,
                Err(err) => {
                    let _ = session.close().await;
                    return Err(map_ashpd_error("select ScreenCast sources", err));
                }
            };
            if let Err(err) = select_request.response() {
                let _ = session.close().await;
                return Err(map_ashpd_error("ScreenCast select_sources response", err));
            }

            let start_request = match proxy
                .start(&session, None, StartCastOptions::default())
                .await
            {
                Ok(request) => request,
                Err(err) => {
                    let _ = session.close().await;
                    return Err(map_ashpd_error("start ScreenCast session", err));
                }
            };
            let streams_response = match start_request.response() {
                Ok(response) => response,
                Err(err) => {
                    let _ = session.close().await;
                    return Err(map_ashpd_error("ScreenCast start response", err));
                }
            };

            let restore_token = streams_response.restore_token().map(str::to_owned);
            let streams: Vec<WaylandScreenCastStream> = streams_response
                .streams()
                .iter()
                .map(|stream| {
                    let source_type = stream.source_type();
                    WaylandScreenCastStream {
                        pipewire_node_id: stream.pipe_wire_node_id(),
                        position: stream.position(),
                        size: stream.size(),
                        is_window: source_type.is_some_and(|s| s.contains(SourceType::WINDOW)),
                        is_monitor: source_type.is_some_and(|s| s.contains(SourceType::MONITOR)),
                        mapping_id: stream.mapping_id().map(str::to_owned),
                        pipewire_serial: stream.pipewire_serial(),
                    }
                })
                .collect();

            if streams.is_empty() {
                let _ = session.close().await;
                return Err(PlatformError::WaylandPortalFailed(String::from(
                    "ScreenCast portal returned no active streams",
                )));
            }

            let pipewire_fd = match proxy
                .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
                .await
            {
                Ok(fd) => fd,
                Err(err) => {
                    let _ = session.close().await;
                    return Err(map_ashpd_error("open PipeWire remote", err));
                }
            };

            Ok(WaylandScreenCastSession {
                _proxy: proxy,
                session,
                streams,
                restore_token,
                pipewire_fd,
            })
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux_portals::{
    WaylandBoundShortcut, WaylandGlobalShortcutsPortal, WaylandScreenCastPortal,
    WaylandScreenCastSession, WaylandScreenCastStream, WaylandScreenshotPortal,
};

#[cfg(test)]
mod tests {
    use sts_core::config::HotkeyConfig;

    use super::*;
    use crate::display_env::WaylandCompositor;

    #[test]
    fn portal_capabilities_match_display_server_and_layer_shell_support() {
        let win = WaylandPortalCapabilities::for_display(DisplayServer::Windows);
        assert!(!win.global_shortcuts);
        assert!(!win.screencast);
        assert!(!win.screenshot_fallback);
        assert!(!win.layer_shell_fullscreen);

        let x11 = WaylandPortalCapabilities::for_display(DisplayServer::LinuxX11);
        assert!(!x11.global_shortcuts);
        assert!(!x11.screencast);
        assert!(!x11.screenshot_fallback);
        assert!(!x11.layer_shell_fullscreen);

        let other = WaylandPortalCapabilities::for_display(DisplayServer::Other);
        assert!(!other.global_shortcuts);
        assert!(!other.screencast);
        assert!(!other.screenshot_fallback);
        assert!(!other.layer_shell_fullscreen);

        let gnome = WaylandPortalCapabilities::for_display(DisplayServer::LinuxWayland {
            compositor: WaylandCompositor::Gnome,
        });
        assert!(gnome.global_shortcuts);
        assert!(gnome.screencast);
        assert!(gnome.screenshot_fallback);
        assert!(!gnome.layer_shell_fullscreen);

        let other_wl = WaylandPortalCapabilities::for_display(DisplayServer::LinuxWayland {
            compositor: WaylandCompositor::Other,
        });
        assert!(other_wl.global_shortcuts);
        assert!(other_wl.screencast);
        assert!(other_wl.screenshot_fallback);
        assert!(!other_wl.layer_shell_fullscreen);

        for compositor in [
            WaylandCompositor::Kde,
            WaylandCompositor::Hyprland,
            WaylandCompositor::Sway,
        ] {
            let caps =
                WaylandPortalCapabilities::for_display(DisplayServer::LinuxWayland { compositor });
            assert!(caps.global_shortcuts);
            assert!(caps.screencast);
            assert!(caps.screenshot_fallback);
            assert!(caps.layer_shell_fullscreen);
        }
    }

    #[test]
    fn layer_surface_config_for_live_inplace_is_click_through_and_non_focusing() {
        let cfg = WaylandLayerSurfaceConfig::for_live_inplace();
        assert!(cfg.anchor_all_edges);
        assert_eq!(cfg.exclusive_zone, -1);
        assert!(!cfg.keyboard_interactivity);
        assert!(cfg.input_region_empty);
        assert_eq!(WaylandLayerSurfaceConfig::default(), cfg);
    }

    #[test]
    fn default_wayland_shortcut_specs_map_all_hotkey_config_bindings() {
        let config = HotkeyConfig {
            square_to_search: String::from("Ctrl+Shift+S"),
            toggle_live_translate: String::from("Alt+F9"),
            toggle_live_visibility: String::from("Super+H"),
        };

        let specs = default_wayland_shortcut_specs(&config);
        assert_eq!(specs.len(), 3);

        assert_eq!(
            specs[0],
            WaylandShortcutSpec {
                id: HotkeyBinding::portal_id(HotkeyAction::SquareToSearch),
                action: HotkeyAction::SquareToSearch,
                description: HotkeyBinding::description(HotkeyAction::SquareToSearch),
                preferred_trigger: String::from("Ctrl+Shift+S"),
            }
        );
        assert_eq!(
            specs[1],
            WaylandShortcutSpec {
                id: HotkeyBinding::portal_id(HotkeyAction::ToggleLiveTranslate),
                action: HotkeyAction::ToggleLiveTranslate,
                description: HotkeyBinding::description(HotkeyAction::ToggleLiveTranslate),
                preferred_trigger: String::from("Alt+F9"),
            }
        );
        assert_eq!(
            specs[2],
            WaylandShortcutSpec {
                id: HotkeyBinding::portal_id(HotkeyAction::ToggleLiveVisibility),
                action: HotkeyAction::ToggleLiveVisibility,
                description: HotkeyBinding::description(HotkeyAction::ToggleLiveVisibility),
                preferred_trigger: String::from("Super+H"),
            }
        );
    }

    #[test]
    fn xdg_preferred_trigger_normalizes_modifiers_and_rejects_empty() {
        assert_eq!(
            xdg_preferred_trigger("Ctrl+Shift+S").as_deref(),
            Some("CTRL+SHIFT+S")
        );
        assert_eq!(
            xdg_preferred_trigger(" Super + Alt + f10 ").as_deref(),
            Some("LOGO+ALT+F10")
        );
        assert_eq!(xdg_preferred_trigger("   "), None);
    }

    #[test]
    fn file_uri_to_local_path_decodes_percent_encoding_and_rejects_bad_uris()
    -> Result<(), PlatformError> {
        assert_eq!(
            file_uri_to_local_path("file:///tmp/Screen%20Shot%201.png")?,
            PathBuf::from("/tmp/Screen Shot 1.png")
        );
        assert_eq!(
            file_uri_to_local_path("file://localhost/tmp/capture.png")?,
            PathBuf::from("/tmp/capture.png")
        );
        assert!(matches!(
            file_uri_to_local_path("https://example.com/a.png"),
            Err(PlatformError::WaylandPortalFailed(_))
        ));
        assert!(matches!(
            file_uri_to_local_path("file:///tmp/bad%ZZ.png"),
            Err(PlatformError::WaylandPortalFailed(_))
        ));
        Ok(())
    }
}
