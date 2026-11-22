//! Display server and Wayland compositor capability detection.

/// Known Linux Wayland compositors relevant to overlay capability gating (Spec §3.1, Decision 2A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaylandCompositor {
    /// GNOME Mutter (does not support `wlr-layer-shell`).
    Gnome,
    /// KDE Plasma / KWin (supports `wlr-layer-shell`).
    Kde,
    /// Hyprland (supports `wlr-layer-shell`).
    Hyprland,
    /// Sway / wlroots (supports `wlr-layer-shell`).
    Sway,
    /// Any other Wayland compositor.
    Other,
}

/// Active desktop display server environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayServer {
    /// Microsoft Windows 10/11 desktop session.
    Windows,
    /// Linux X11 desktop session.
    LinuxX11,
    /// Linux Wayland desktop session with detected compositor family.
    LinuxWayland { compositor: WaylandCompositor },
    /// Headless, TTY, or unsupported OS session.
    Other,
}

impl DisplayServer {
    /// Returns `true` if the display server supports `wlr-layer-shell` for fullscreen/maximized
    /// Wayland overlays (KDE Plasma, Hyprland, Sway).
    #[must_use]
    pub const fn supports_wayland_layer_shell(&self) -> bool {
        matches!(
            self,
            Self::LinuxWayland {
                compositor: WaylandCompositor::Kde
                    | WaylandCompositor::Hyprland
                    | WaylandCompositor::Sway,
            }
        )
    }

    /// Returns `true` if floating windowed in-place overlays are supported (Windows and Linux X11;
    /// disabled on Linux Wayland v1 per Decision 2A).
    #[must_use]
    pub const fn supports_windowed_inplace(&self) -> bool {
        matches!(self, Self::Windows | Self::LinuxX11)
    }

    /// Returns `true` when running under any Linux Wayland compositor.
    #[must_use]
    pub const fn is_wayland(&self) -> bool {
        matches!(self, Self::LinuxWayland { .. })
    }
}

fn classify_wayland_compositor(xdg_current_desktop: Option<&str>) -> WaylandCompositor {
    let Some(raw) = xdg_current_desktop else {
        return WaylandCompositor::Other;
    };

    for segment in raw.split(':') {
        let normalized = segment.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if normalized == "hyprland" || normalized.contains("hyprland") {
            return WaylandCompositor::Hyprland;
        }
        if normalized == "sway" || normalized.contains("sway") {
            return WaylandCompositor::Sway;
        }
        if normalized == "kde" || normalized.contains("kde") || normalized.contains("plasma") {
            return WaylandCompositor::Kde;
        }
        if normalized == "gnome" || normalized.contains("gnome") {
            return WaylandCompositor::Gnome;
        }
    }

    WaylandCompositor::Other
}

/// Pure, deterministic display server detection from OS name and environment variables.
#[must_use]
pub fn detect_display_server_from_env(
    os: &str,
    xdg_session_type: Option<&str>,
    wayland_display: Option<&str>,
    xdg_current_desktop: Option<&str>,
) -> DisplayServer {
    if os.eq_ignore_ascii_case("windows") {
        return DisplayServer::Windows;
    }

    if !os.eq_ignore_ascii_case("linux") {
        return DisplayServer::Other;
    }

    let session_type = xdg_session_type.map(str::trim).filter(|s| !s.is_empty());
    let has_wayland_display = wayland_display.is_some_and(|v| !v.trim().is_empty());

    if session_type.is_some_and(|s| s.eq_ignore_ascii_case("wayland"))
        || (session_type.is_none() && has_wayland_display)
    {
        return DisplayServer::LinuxWayland {
            compositor: classify_wayland_compositor(xdg_current_desktop),
        };
    }

    if session_type.is_some_and(|s| s.eq_ignore_ascii_case("x11")) || session_type.is_none() {
        return DisplayServer::LinuxX11;
    }

    DisplayServer::Other
}

/// Detects the current process's display server from [`std::env::consts::OS`] and environment
/// variables (`XDG_SESSION_TYPE`, `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`).
#[must_use]
pub fn detect_display_server() -> DisplayServer {
    let xdg_session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let xdg_current_desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
                Some(String::from("Hyprland"))
            } else if std::env::var_os("SWAYSOCK").is_some() {
                Some(String::from("sway"))
            } else {
                None
            }
        });

    detect_display_server_from_env(
        std::env::consts::OS,
        xdg_session_type.as_deref(),
        wayland_display.as_deref(),
        xdg_current_desktop.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gnome_wayland_and_disables_windowed_inplace_flag() {
        let ds = detect_display_server_from_env(
            "linux",
            Some("wayland"),
            Some("wayland-0"),
            Some("ubuntu:GNOME"),
        );
        assert_eq!(
            ds,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Gnome,
            }
        );
        assert!(!ds.supports_wayland_layer_shell());
        assert!(!ds.supports_windowed_inplace());
        assert!(ds.is_wayland());
    }

    #[test]
    fn detects_hyprland_wayland_supports_layer_shell() {
        let ds = detect_display_server_from_env(
            "linux",
            Some("wayland"),
            Some("wayland-1"),
            Some("Hyprland"),
        );
        assert_eq!(
            ds,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Hyprland,
            }
        );
        assert!(ds.supports_wayland_layer_shell());
        assert!(!ds.supports_windowed_inplace());
    }

    #[test]
    fn detects_kde_and_sway_wayland_layer_shell_support() {
        let kde = detect_display_server_from_env(
            "linux",
            Some("wayland"),
            Some("wayland-0"),
            Some("KDE"),
        );
        assert_eq!(
            kde,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Kde,
            }
        );
        assert!(kde.supports_wayland_layer_shell());
        assert!(!kde.supports_windowed_inplace());

        let sway = detect_display_server_from_env("linux", None, Some("wayland-0"), Some("sway"));
        assert_eq!(
            sway,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Sway,
            }
        );
        assert!(sway.supports_wayland_layer_shell());
        assert!(!sway.supports_windowed_inplace());
    }

    #[test]
    fn detects_windows_and_linux_x11_windowed_inplace_support() {
        let win = detect_display_server_from_env("windows", None, None, None);
        assert_eq!(win, DisplayServer::Windows);
        assert!(win.supports_windowed_inplace());
        assert!(!win.supports_wayland_layer_shell());
        assert!(!win.is_wayland());

        let x11 = detect_display_server_from_env("linux", Some("x11"), None, Some("GNOME"));
        assert_eq!(x11, DisplayServer::LinuxX11);
        assert!(x11.supports_windowed_inplace());
        assert!(!x11.supports_wayland_layer_shell());
        assert!(!x11.is_wayland());
    }

    #[test]
    fn detects_unknown_wayland_compositor_and_non_desktop_sessions() {
        let cosmic = detect_display_server_from_env(
            "linux",
            Some("wayland"),
            Some("wayland-0"),
            Some("COSMIC"),
        );
        assert_eq!(
            cosmic,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Other,
            }
        );
        assert!(!cosmic.supports_wayland_layer_shell());
        assert!(!cosmic.supports_windowed_inplace());

        let tty = detect_display_server_from_env("linux", Some("tty"), None, None);
        assert_eq!(tty, DisplayServer::Other);
        assert!(!tty.supports_windowed_inplace());

        let other_os = detect_display_server_from_env("macos", None, None, None);
        assert_eq!(other_os, DisplayServer::Other);
    }

    #[test]
    fn detect_display_server_matches_current_host_without_panicking() {
        let detected = detect_display_server();
        if cfg!(target_os = "windows") {
            assert_eq!(detected, DisplayServer::Windows);
        }
    }
}
