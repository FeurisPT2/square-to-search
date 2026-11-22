//! Native overlay window styling (click-through, anti-self-capture, always-on-top) and
//! display-server-aware Live Translate mode resolution (Spec §3.1, Decision 2A).

use raw_window_handle::RawWindowHandle;
use sts_core::geometry::{MonitorGeometry, PhysicalRect};
use sts_core::types::LiveDisplayMode;

use crate::PlatformError;
use crate::display_env::{DisplayServer, WaylandCompositor};

/// Native OS window style configuration for Live Translate overlay windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OverlayStyle {
    /// When `true`, mouse/pointer input passes straight through the window.
    pub click_through: bool,
    /// When `true`, excludes the window from screen capture (`WDA_EXCLUDEFROMCAPTURE` on Windows)
    /// to prevent self-capture loops (Anti-Goal #1).
    pub exclude_from_capture: bool,
    /// When `true`, keeps the overlay above standard application windows.
    pub always_on_top: bool,
}

impl OverlayStyle {
    /// Preset for the transparent click-through In-place overlay window.
    #[must_use]
    pub const fn live_inplace() -> Self {
        Self {
            click_through: true,
            exclude_from_capture: true,
            always_on_top: true,
        }
    }

    /// Preset for clickable Live Translate floating windows (Floating Panel & Live Controls pill).
    #[must_use]
    pub const fn live_panel() -> Self {
        Self {
            click_through: false,
            exclude_from_capture: true,
            always_on_top: true,
        }
    }
}

/// Effective Live Translate display mode after applying platform display-server constraints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLiveMode {
    /// Resolved display mode to activate.
    pub mode: LiveDisplayMode,
    /// Optional Vietnamese user-facing notice when the requested mode was downgraded.
    pub notice: Option<String>,
}

/// Returns `true` when `target` covers at least 90% of `monitor.width` and 85% of
/// `monitor.height` (fullscreen or maximized reader window threshold per Decision 2A).
#[must_use]
pub fn is_rect_fullscreen_or_maximized(target: PhysicalRect, monitor: &MonitorGeometry) -> bool {
    if monitor.width == 0 || monitor.height == 0 {
        return false;
    }

    u64::from(target.width) * 100 >= u64::from(monitor.width) * 90
        && u64::from(target.height) * 100 >= u64::from(monitor.height) * 85
}

/// Resolves the effective Live Translate display mode for `display` and whether the target reader
/// region/window is fullscreen or maximized (Spec §3.1, Decision 2A).
#[must_use]
pub fn resolve_effective_live_mode_for_target(
    requested: LiveDisplayMode,
    display: DisplayServer,
    target_is_fullscreen_or_maximized: bool,
) -> EffectiveLiveMode {
    match (requested, display) {
        (LiveDisplayMode::FloatingPanel, _) => EffectiveLiveMode {
            mode: LiveDisplayMode::FloatingPanel,
            notice: None,
        },
        (LiveDisplayMode::InPlace, DisplayServer::Windows | DisplayServer::LinuxX11) => {
            EffectiveLiveMode {
                mode: LiveDisplayMode::InPlace,
                notice: None,
            }
        }
        (
            LiveDisplayMode::InPlace,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Gnome,
            },
        ) => EffectiveLiveMode {
            mode: LiveDisplayMode::FloatingPanel,
            notice: Some(String::from(
                "GNOME Wayland không hỗ trợ lớp phủ trực tiếp (wlr-layer-shell) — đã tự động chuyển sang Bảng dịch nổi.",
            )),
        },
        (LiveDisplayMode::InPlace, DisplayServer::LinuxWayland { .. })
            if display.supports_wayland_layer_shell() && target_is_fullscreen_or_maximized =>
        {
            EffectiveLiveMode {
                mode: LiveDisplayMode::InPlace,
                notice: None,
            }
        }
        (LiveDisplayMode::InPlace, DisplayServer::LinuxWayland { .. })
            if display.supports_wayland_layer_shell() =>
        {
            EffectiveLiveMode {
                mode: LiveDisplayMode::FloatingPanel,
                notice: Some(String::from(
                    "Lớp phủ trực tiếp cho cửa sổ thả nổi chưa được hỗ trợ trên Wayland — đã tự động chuyển sang Bảng dịch nổi.",
                )),
            }
        }
        (LiveDisplayMode::InPlace, DisplayServer::LinuxWayland { .. }) => EffectiveLiveMode {
            mode: LiveDisplayMode::FloatingPanel,
            notice: Some(String::from(
                "Trình tổng hợp Wayland hiện tại không hỗ trợ wlr-layer-shell — đã tự động chuyển sang Bảng dịch nổi.",
            )),
        },
        (LiveDisplayMode::InPlace, DisplayServer::Other) => EffectiveLiveMode {
            mode: LiveDisplayMode::FloatingPanel,
            notice: Some(String::from(
                "Môi trường hiển thị hiện tại không hỗ trợ lớp phủ trực tiếp — đã tự động chuyển sang Bảng dịch nổi.",
            )),
        },
    }
}

/// Resolves the effective Live Translate display mode for `display` (Spec §3.1, Decision 2A).
///
/// Assumes a floating/non-fullscreen target (`target_is_fullscreen_or_maximized = false`).
#[must_use]
pub fn resolve_effective_live_mode(
    requested: LiveDisplayMode,
    display: DisplayServer,
) -> EffectiveLiveMode {
    resolve_effective_live_mode_for_target(requested, display, false)
}

/// Abstraction for applying native OS overlay attributes to a window handle.
pub trait OverlayWindowConfigurator: Send + Sync {
    /// Applies `style` to the native window identified by `handle`.
    fn apply_style(
        &self,
        handle: RawWindowHandle,
        style: OverlayStyle,
    ) -> Result<(), PlatformError>;
}

/// Production [`OverlayWindowConfigurator`] backed by Win32 on Windows and X11 Shape on Linux.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeOverlayConfigurator;

impl NativeOverlayConfigurator {
    /// Creates a new native overlay window configurator.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl OverlayWindowConfigurator for NativeOverlayConfigurator {
    fn apply_style(
        &self,
        handle: RawWindowHandle,
        style: OverlayStyle,
    ) -> Result<(), PlatformError> {
        #[cfg(target_os = "windows")]
        {
            apply_windows_overlay_style(handle, style)
        }

        #[cfg(target_os = "linux")]
        {
            apply_linux_overlay_style(handle, style)
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            let _ = (handle, style);
            Err(PlatformError::OverlayStyleFailed(String::from(
                "native overlay styling is only supported on Windows and Linux",
            )))
        }
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn compute_win32_ex_style(current_ex_style: isize, style: OverlayStyle) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
    };

    let toolwindow = WS_EX_TOOLWINDOW as isize;
    if style.click_through {
        let click_through_flags = (WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE) as isize;
        current_ex_style | click_through_flags | toolwindow
    } else {
        let non_clickable_flags = (WS_EX_TRANSPARENT | WS_EX_NOACTIVATE) as isize;
        (current_ex_style | toolwindow) & !non_clickable_flags
    }
}

#[cfg(target_os = "windows")]
fn apply_windows_overlay_style(
    handle: RawWindowHandle,
    style: OverlayStyle,
) -> Result<(), PlatformError> {
    use windows_sys::Win32::Foundation::{GetLastError, HWND, SetLastError};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, HWND_NOTOPMOST, HWND_TOPMOST, IsWindow, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowDisplayAffinity, SetWindowLongPtrW,
        SetWindowPos, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };

    let raw_hwnd = match handle {
        RawWindowHandle::Win32(win32) => win32.hwnd.get(),
        other => {
            return Err(PlatformError::OverlayStyleFailed(format!(
                "expected Win32 window handle on Windows, got {other:?}"
            )));
        }
    };

    if raw_hwnd == 0 {
        return Err(PlatformError::OverlayStyleFailed(String::from(
            "Win32 HWND must be non-zero",
        )));
    }

    let hwnd: HWND = std::ptr::without_provenance_mut(raw_hwnd as usize);
    if hwnd.is_null() {
        return Err(PlatformError::OverlayStyleFailed(String::from(
            "Win32 HWND pointer resolved to null",
        )));
    }

    // SAFETY: `hwnd` is verified non-null above. `IsWindow` is a Win32 user32 handle validation
    // call that checks whether an opaque HWND refers to an existing window without dereferencing
    // caller memory.
    let is_valid = unsafe { IsWindow(hwnd) };
    if is_valid == 0 {
        return Err(PlatformError::OverlayStyleFailed(format!(
            "Win32 HWND {raw_hwnd:#x} does not refer to a valid window"
        )));
    }

    // SAFETY: `hwnd` is a validated live Win32 window handle. Per Win32 documentation for
    // `GetWindowLongPtrW`, clearing the thread-local last error before the call distinguishes a
    // legitimate zero extended-style mask from a failure.
    let current_ex_style = unsafe {
        SetLastError(0);
        let value = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if value == 0 {
            let err = GetLastError();
            if err != 0 {
                return Err(PlatformError::OverlayStyleFailed(format!(
                    "GetWindowLongPtrW(GWL_EXSTYLE) failed with Win32 error {err}"
                )));
            }
        }
        value
    };

    let next_ex_style = compute_win32_ex_style(current_ex_style, style);

    // SAFETY: `hwnd` is a validated live Win32 window handle and `next_ex_style` only modifies
    // documented `WS_EX_*` extended style bits. Clearing the thread-local last error before
    // `SetWindowLongPtrW` distinguishes a previous value of 0 from an actual Win32 error.
    unsafe {
        SetLastError(0);
        let prev = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next_ex_style);
        if prev == 0 {
            let err = GetLastError();
            if err != 0 {
                return Err(PlatformError::OverlayStyleFailed(format!(
                    "SetWindowLongPtrW(GWL_EXSTYLE) failed with Win32 error {err}"
                )));
            }
        }
    }

    let affinity = if style.exclude_from_capture {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };

    // SAFETY: `hwnd` is a validated live Win32 window handle and `affinity` is either
    // `WDA_EXCLUDEFROMCAPTURE` (0x11) or `WDA_NONE` (0x0). No Rust pointers are passed.
    let affinity_ok = unsafe { SetWindowDisplayAffinity(hwnd, affinity) };
    if affinity_ok == 0 {
        // SAFETY: `GetLastError` reads only the calling thread's Win32 last-error code.
        let err = unsafe { GetLastError() };
        return Err(PlatformError::OverlayStyleFailed(format!(
            "SetWindowDisplayAffinity({affinity:#x}) failed with Win32 error {err}"
        )));
    }

    let insert_after = if style.always_on_top {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED;

    // SAFETY: `hwnd` is a validated live Win32 window handle, `insert_after` is a documented Win32
    // z-order sentinel (`HWND_TOPMOST` or `HWND_NOTOPMOST`), and `SWP_NOMOVE | SWP_NOSIZE` ensures
    // position and size parameters are ignored while `SWP_FRAMECHANGED` applies updated ex-styles.
    let pos_ok = unsafe { SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags) };
    if pos_ok == 0 {
        // SAFETY: `GetLastError` reads only the calling thread's Win32 last-error code.
        let err = unsafe { GetLastError() };
        return Err(PlatformError::OverlayStyleFailed(format!(
            "SetWindowPos failed with Win32 error {err}"
        )));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_x11_input_shape(window_id: u32, click_through: bool) -> Result<(), PlatformError> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::{shape, xproto};

    let (conn, _screen_num) = x11rb::connect(None).map_err(|err| {
        PlatformError::OverlayStyleFailed(format!("failed to connect to X11 display: {err}"))
    })?;

    if click_through {
        shape::rectangles(
            &conn,
            shape::SO::SET,
            shape::SK::INPUT,
            xproto::ClipOrdering::UNSORTED,
            window_id,
            0,
            0,
            &[],
        )
        .map_err(|err| {
            PlatformError::OverlayStyleFailed(format!(
                "XShape input rectangles request failed for window {window_id}: {err}"
            ))
        })?
        .check()
        .map_err(|err| {
            PlatformError::OverlayStyleFailed(format!(
                "XShape input rectangles rejected for window {window_id}: {err}"
            ))
        })?;
    } else {
        shape::mask(
            &conn,
            shape::SO::SET,
            shape::SK::INPUT,
            window_id,
            0,
            0,
            x11rb::NONE,
        )
        .map_err(|err| {
            PlatformError::OverlayStyleFailed(format!(
                "XShape input mask reset failed for window {window_id}: {err}"
            ))
        })?
        .check()
        .map_err(|err| {
            PlatformError::OverlayStyleFailed(format!(
                "XShape input mask reset rejected for window {window_id}: {err}"
            ))
        })?;
    }

    conn.flush().map_err(|err| {
        PlatformError::OverlayStyleFailed(format!("X11 connection flush failed: {err}"))
    })?;

    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_linux_overlay_style(
    handle: RawWindowHandle,
    style: OverlayStyle,
) -> Result<(), PlatformError> {
    match handle {
        RawWindowHandle::Xlib(xlib) => {
            let window_id = u32::try_from(xlib.window).map_err(|_| {
                PlatformError::OverlayStyleFailed(format!(
                    "Xlib window id {} exceeds X11 u32 range",
                    xlib.window
                ))
            })?;
            if window_id == 0 {
                return Err(PlatformError::OverlayStyleFailed(String::from(
                    "Xlib window handle is zero",
                )));
            }
            apply_x11_input_shape(window_id, style.click_through)
        }
        RawWindowHandle::Xcb(xcb) => apply_x11_input_shape(xcb.window.get(), style.click_through),
        RawWindowHandle::Wayland(_) => {
            tracing::debug!(
                ?style,
                "Wayland overlay window uses compositor/portal semantics; no X11 shape applied"
            );
            Ok(())
        }
        other => Err(PlatformError::OverlayStyleFailed(format!(
            "unsupported Linux window handle for overlay styling: {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_effective_live_mode_honors_inplace_on_windows_and_x11() {
        let win_inplace =
            resolve_effective_live_mode(LiveDisplayMode::InPlace, DisplayServer::Windows);
        assert_eq!(win_inplace.mode, LiveDisplayMode::InPlace);
        assert!(win_inplace.notice.is_none());

        let x11_inplace =
            resolve_effective_live_mode(LiveDisplayMode::InPlace, DisplayServer::LinuxX11);
        assert_eq!(x11_inplace.mode, LiveDisplayMode::InPlace);
        assert!(x11_inplace.notice.is_none());

        let wayland_panel = resolve_effective_live_mode(
            LiveDisplayMode::FloatingPanel,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Gnome,
            },
        );
        assert_eq!(wayland_panel.mode, LiveDisplayMode::FloatingPanel);
        assert!(wayland_panel.notice.is_none());
    }

    #[test]
    fn resolve_effective_live_mode_falls_back_to_floating_panel_on_wayland() {
        // Review Focus #5: Wayland InPlace -> FloatingPanel with Vietnamese notice mentioning "Wayland".
        for compositor in [
            WaylandCompositor::Gnome,
            WaylandCompositor::Kde,
            WaylandCompositor::Hyprland,
            WaylandCompositor::Sway,
            WaylandCompositor::Other,
        ] {
            let eff = resolve_effective_live_mode(
                LiveDisplayMode::InPlace,
                DisplayServer::LinuxWayland { compositor },
            );
            assert_eq!(eff.mode, LiveDisplayMode::FloatingPanel);
            assert!(
                eff.notice
                    .as_deref()
                    .is_some_and(|notice| notice.contains("Wayland")),
                "expected Vietnamese Wayland fallback notice for {compositor:?}, got {:?}",
                eff.notice
            );
        }

        let other = resolve_effective_live_mode(LiveDisplayMode::InPlace, DisplayServer::Other);
        assert_eq!(other.mode, LiveDisplayMode::FloatingPanel);
        assert!(other.notice.is_some());
    }

    #[test]
    fn is_rect_fullscreen_or_maximized_enforces_90_by_85_percent_threshold() {
        let monitor = MonitorGeometry {
            id: 1,
            name: String::from("Primary"),
            x: 0,
            y: 0,
            width: 2000,
            height: 1000,
            scale_factor: 1.0,
            is_primary: true,
        };

        // Exact 90% width x 85% height threshold -> true
        assert!(is_rect_fullscreen_or_maximized(
            PhysicalRect {
                x: 0,
                y: 0,
                width: 1800,
                height: 850,
            },
            &monitor
        ));

        // Fullscreen -> true
        assert!(is_rect_fullscreen_or_maximized(
            PhysicalRect {
                x: 0,
                y: 0,
                width: 2000,
                height: 1000,
            },
            &monitor
        ));

        // Just below width threshold (1799 < 1800) -> false
        assert!(!is_rect_fullscreen_or_maximized(
            PhysicalRect {
                x: 0,
                y: 0,
                width: 1799,
                height: 1000,
            },
            &monitor
        ));

        // Just below height threshold (849 < 850) -> false
        assert!(!is_rect_fullscreen_or_maximized(
            PhysicalRect {
                x: 0,
                y: 0,
                width: 2000,
                height: 849,
            },
            &monitor
        ));

        // Floating 70% x 70% window -> false
        assert!(!is_rect_fullscreen_or_maximized(
            PhysicalRect {
                x: 100,
                y: 100,
                width: 1400,
                height: 700,
            },
            &monitor
        ));

        // Degenerate zero-sized monitor -> false
        let zero_mon = MonitorGeometry {
            width: 0,
            height: 0,
            ..monitor
        };
        assert!(!is_rect_fullscreen_or_maximized(
            PhysicalRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            &zero_mon
        ));
    }

    #[test]
    fn resolve_effective_live_mode_for_target_covers_exhaustive_decision_2a_matrix() {
        let displays = [
            DisplayServer::Windows,
            DisplayServer::LinuxX11,
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Kde,
            },
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Hyprland,
            },
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Sway,
            },
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Gnome,
            },
            DisplayServer::LinuxWayland {
                compositor: WaylandCompositor::Other,
            },
            DisplayServer::Other,
        ];

        for display in displays {
            for fullscreen in [false, true] {
                let panel = resolve_effective_live_mode_for_target(
                    LiveDisplayMode::FloatingPanel,
                    display,
                    fullscreen,
                );
                assert_eq!(panel.mode, LiveDisplayMode::FloatingPanel);
                assert!(panel.notice.is_none());

                let inplace = resolve_effective_live_mode_for_target(
                    LiveDisplayMode::InPlace,
                    display,
                    fullscreen,
                );

                let expected_inplace =
                    matches!(
                        display,
                        DisplayServer::Windows
                            | DisplayServer::LinuxX11
                            | DisplayServer::LinuxWayland {
                                compositor: WaylandCompositor::Kde
                                    | WaylandCompositor::Hyprland
                                    | WaylandCompositor::Sway,
                            }
                    ) && (matches!(display, DisplayServer::Windows | DisplayServer::LinuxX11)
                        || fullscreen);

                if expected_inplace {
                    assert_eq!(
                        inplace.mode,
                        LiveDisplayMode::InPlace,
                        "expected InPlace for {display:?} fullscreen={fullscreen}"
                    );
                    assert!(
                        inplace.notice.is_none(),
                        "expected no notice for {display:?} fullscreen={fullscreen}"
                    );
                } else {
                    assert_eq!(
                        inplace.mode,
                        LiveDisplayMode::FloatingPanel,
                        "expected FloatingPanel fallback for {display:?} fullscreen={fullscreen}"
                    );
                    assert!(
                        inplace.notice.as_deref().is_some_and(|n| !n.is_empty()),
                        "expected non-empty Vietnamese fallback notice for {display:?} fullscreen={fullscreen}"
                    );
                    if display.is_wayland() {
                        assert!(
                            inplace
                                .notice
                                .as_deref()
                                .is_some_and(|n| n.contains("Wayland")),
                            "expected Wayland notice for {display:?}, got {:?}",
                            inplace.notice
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn overlay_style_presets_exclude_from_capture_for_all_live_windows() {
        let inplace = OverlayStyle::live_inplace();
        assert!(inplace.click_through);
        assert!(inplace.exclude_from_capture);
        assert!(inplace.always_on_top);

        let panel = OverlayStyle::live_panel();
        assert!(!panel.click_through);
        assert!(panel.exclude_from_capture);
        assert!(panel.always_on_top);
    }

    #[test]
    fn overlay_configurator_trait_is_dyn_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn OverlayWindowConfigurator>();
        assert_send_sync::<NativeOverlayConfigurator>();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn native_overlay_configurator_rejects_mismatched_and_invalid_win32_handles() {
        use std::num::NonZeroIsize;

        use raw_window_handle::{Win32WindowHandle, XlibWindowHandle};

        let configurator = NativeOverlayConfigurator::new();

        let wrong_handle = RawWindowHandle::Xlib(XlibWindowHandle::new(42));
        let wrong_res = configurator.apply_style(wrong_handle, OverlayStyle::live_inplace());
        assert!(matches!(
            wrong_res,
            Err(PlatformError::OverlayStyleFailed(_))
        ));

        if let Some(fake_hwnd) = NonZeroIsize::new(0x7FFF_1234) {
            let invalid_win32 = RawWindowHandle::Win32(Win32WindowHandle::new(fake_hwnd));
            let invalid_res = configurator.apply_style(invalid_win32, OverlayStyle::live_inplace());
            assert!(matches!(
                invalid_res,
                Err(PlatformError::OverlayStyleFailed(_))
            ));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_overlay_configurator_accepts_wayland_handle_and_rejects_zero_xlib_window() {
        use std::ffi::c_void;
        use std::ptr::NonNull;

        use raw_window_handle::{WaylandWindowHandle, XlibWindowHandle};

        let configurator = NativeOverlayConfigurator::new();

        let mut sentinel = 0u8;
        let surface = NonNull::from(&mut sentinel).cast::<c_void>();
        let wayland_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(surface));
        assert!(
            configurator
                .apply_style(wayland_handle, OverlayStyle::live_panel())
                .is_ok()
        );

        let zero_xlib = RawWindowHandle::Xlib(XlibWindowHandle::new(0));
        assert!(matches!(
            configurator.apply_style(zero_xlib, OverlayStyle::live_inplace()),
            Err(PlatformError::OverlayStyleFailed(_))
        ));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn win32_ex_style_toggles_click_through_bits_while_preserving_existing_flags() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
        };

        let existing_app_bit = 0x0000_0100_isize; // WS_EX_WINDOWEDGE
        let inplace_bits = compute_win32_ex_style(existing_app_bit, OverlayStyle::live_inplace());
        let expected_inplace = existing_app_bit
            | (WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE) as isize;
        assert_eq!(inplace_bits, expected_inplace);

        let panel_bits = compute_win32_ex_style(inplace_bits, OverlayStyle::live_panel());
        assert_ne!(panel_bits & existing_app_bit, 0);
        assert_ne!(panel_bits & (WS_EX_TOOLWINDOW as isize), 0);
        assert_eq!(panel_bits & (WS_EX_TRANSPARENT as isize), 0);
        assert_eq!(panel_bits & (WS_EX_NOACTIVATE as isize), 0);
    }
}
