//! Cross-platform screen capture abstraction and pure RGBA sub-region cropping.

use image::RgbaImage;
use sts_core::geometry::{MonitorGeometry, PhysicalRect};
use sts_core::types::CaptureFrame;
use tracing::debug;

use crate::PlatformError;

/// Geometry and metadata for a single OS application window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowGeometry {
    pub id: u32,
    pub title: String,
    pub app_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_minimized: bool,
}

/// Abstraction over OS monitor/window enumeration and freeze-frame screen capture.
pub trait ScreenCapturer: Send + Sync {
    /// Lists all active monitors with their physical geometry and DPI scale factor.
    fn list_monitors(&self) -> Result<Vec<MonitorGeometry>, PlatformError>;

    /// Captures a full RGBA8 freeze-frame from the monitor with the given `monitor_id`.
    fn capture_monitor(&self, monitor_id: u32) -> Result<CaptureFrame, PlatformError>;

    /// Captures the currently active (focused) monitor, falling back to the primary or first
    /// available monitor, returning both its geometry and framebuffer.
    fn capture_active_monitor(&self) -> Result<(MonitorGeometry, CaptureFrame), PlatformError>;

    /// Lists visible, non-minimized OS application windows with their physical geometry.
    ///
    /// Default implementation returns an empty list so minimal test fakes remain source-compatible.
    fn list_windows(&self) -> Result<Vec<WindowGeometry>, PlatformError> {
        Ok(Vec::new())
    }

    /// Captures a single target OS application window by `window_id`, returning both its
    /// synchronized geometry and RGBA8 framebuffer.
    ///
    /// Default implementation returns [`PlatformError::WindowNotFound`] so minimal test fakes
    /// remain source-compatible.
    fn capture_window(
        &self,
        window_id: u32,
    ) -> Result<(WindowGeometry, CaptureFrame), PlatformError> {
        Err(PlatformError::WindowNotFound(window_id))
    }
}

/// Production [`ScreenCapturer`] backed by `xcap`.
#[derive(Debug, Default, Clone, Copy)]
pub struct XcapCapturer;

impl XcapCapturer {
    /// Creates a new `xcap`-backed screen capturer.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

fn sanitize_scale_factor(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    }
}

fn monitor_to_geometry(monitor: &xcap::Monitor) -> Result<MonitorGeometry, PlatformError> {
    let id = monitor
        .id()
        .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
    let name = monitor.name().unwrap_or_else(|_| format!("Monitor {id}"));
    let x = monitor.x().unwrap_or(0);
    let y = monitor.y().unwrap_or(0);
    let width = monitor
        .width()
        .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
    let height = monitor
        .height()
        .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
    let scale_factor = sanitize_scale_factor(monitor.scale_factor().unwrap_or(1.0));
    let is_primary = monitor.is_primary().unwrap_or(false);

    Ok(MonitorGeometry {
        id,
        name,
        x,
        y,
        width,
        height,
        scale_factor,
        is_primary,
    })
}

fn rgba_image_to_frame(img: RgbaImage, scale_factor: f32) -> CaptureFrame {
    let width = img.width();
    let height = img.height();
    CaptureFrame {
        width,
        height,
        scale_factor: sanitize_scale_factor(scale_factor),
        rgba: img.into_raw(),
    }
}

fn find_focused_monitor() -> Option<xcap::Monitor> {
    let windows = match xcap::Window::all() {
        Ok(windows) => windows,
        Err(err) => {
            debug!("failed to enumerate windows while resolving active monitor: {err}");
            return None;
        }
    };

    for window in windows {
        if window.is_focused().unwrap_or(false) && !window.is_minimized().unwrap_or(false) {
            match window.current_monitor() {
                Ok(monitor) => return Some(monitor),
                Err(err) => {
                    debug!("focused window monitor lookup failed: {err}");
                }
            }
        }
    }

    None
}

fn select_fallback_monitor() -> Result<xcap::Monitor, PlatformError> {
    let monitors =
        xcap::Monitor::all().map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;

    let mut fallback: Option<xcap::Monitor> = None;
    for monitor in monitors {
        if monitor.is_primary().unwrap_or(false) {
            return Ok(monitor);
        }
        if fallback.is_none() {
            fallback = Some(monitor);
        }
    }

    fallback.ok_or(PlatformError::NoMonitorsFound)
}

fn window_to_geometry(window: &xcap::Window) -> Result<WindowGeometry, PlatformError> {
    let id = window
        .id()
        .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
    let title = window.title().unwrap_or_default();
    let app_name = window.app_name().unwrap_or_default();
    let x = window.x().unwrap_or(0);
    let y = window.y().unwrap_or(0);
    let width = window.width().unwrap_or(0);
    let height = window.height().unwrap_or(0);
    let scale_factor = window
        .current_monitor()
        .ok()
        .and_then(|monitor| monitor.scale_factor().ok())
        .map_or(1.0, sanitize_scale_factor);
    let is_minimized = window.is_minimized().unwrap_or(false);

    Ok(WindowGeometry {
        id,
        title,
        app_name,
        x,
        y,
        width,
        height,
        scale_factor,
        is_minimized,
    })
}

pub(crate) fn filter_visible_windows(windows: Vec<WindowGeometry>) -> Vec<WindowGeometry> {
    windows
        .into_iter()
        .filter(|window| !window.is_minimized && window.width > 0 && window.height > 0)
        .collect()
}

pub(crate) fn sync_window_geometry_to_frame(geometry: &mut WindowGeometry, frame: &CaptureFrame) {
    geometry.width = frame.width;
    geometry.height = frame.height;
}

impl ScreenCapturer for XcapCapturer {
    fn list_monitors(&self) -> Result<Vec<MonitorGeometry>, PlatformError> {
        let monitors =
            xcap::Monitor::all().map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        monitors.iter().map(monitor_to_geometry).collect()
    }

    fn capture_monitor(&self, monitor_id: u32) -> Result<CaptureFrame, PlatformError> {
        let monitors =
            xcap::Monitor::all().map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        let target = monitors
            .into_iter()
            .find(|monitor| monitor.id().ok() == Some(monitor_id))
            .ok_or(PlatformError::MonitorNotFound(monitor_id))?;

        let scale_factor = sanitize_scale_factor(target.scale_factor().unwrap_or(1.0));
        let image = target
            .capture_image()
            .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        Ok(rgba_image_to_frame(image, scale_factor))
    }

    fn capture_active_monitor(&self) -> Result<(MonitorGeometry, CaptureFrame), PlatformError> {
        let monitor = match find_focused_monitor() {
            Some(mon) => mon,
            None => select_fallback_monitor()?,
        };

        let mut geometry = monitor_to_geometry(&monitor)?;
        let image = monitor
            .capture_image()
            .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        let frame = rgba_image_to_frame(image, geometry.scale_factor);
        // Ensure MonitorGeometry reflects the captured physical framebuffer dimensions so
        // logical-to-physical crop coordinates stay in sync across Windows and scaled Linux X11/Wayland.
        geometry.width = frame.width;
        geometry.height = frame.height;
        Ok((geometry, frame))
    }

    fn list_windows(&self) -> Result<Vec<WindowGeometry>, PlatformError> {
        let windows =
            xcap::Window::all().map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        let mut geometries = Vec::with_capacity(windows.len());
        for window in &windows {
            match window_to_geometry(window) {
                Ok(geometry) => geometries.push(geometry),
                Err(err) => {
                    debug!("skipping window whose geometry could not be queried: {err}");
                }
            }
        }
        Ok(filter_visible_windows(geometries))
    }

    fn capture_window(
        &self,
        window_id: u32,
    ) -> Result<(WindowGeometry, CaptureFrame), PlatformError> {
        if window_id == 0 {
            return Err(PlatformError::WindowNotFound(0));
        }

        let windows =
            xcap::Window::all().map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        let target = windows
            .into_iter()
            .find(|window| window.id().ok() == Some(window_id))
            .ok_or(PlatformError::WindowNotFound(window_id))?;

        let mut geometry = window_to_geometry(&target)?;
        if geometry.is_minimized {
            return Err(PlatformError::CaptureFailed(format!(
                "window {window_id} is minimized"
            )));
        }

        let image = target
            .capture_image()
            .map_err(|err| PlatformError::CaptureFailed(err.to_string()))?;
        let frame = rgba_image_to_frame(image, geometry.scale_factor);
        if frame.width == 0 || frame.height == 0 {
            return Err(PlatformError::CaptureFailed(format!(
                "captured empty framebuffer ({}x{}) for window {window_id}",
                frame.width, frame.height
            )));
        }
        // Keep WindowGeometry synchronized with the actual physical framebuffer returned by xcap.
        sync_window_geometry_to_frame(&mut geometry, &frame);
        Ok((geometry, frame))
    }
}

/// Extracts a physical-pixel RGBA8 sub-region from `frame`, clamping coordinates cleanly to the
/// framebuffer edges.
///
/// # Errors
///
/// Returns [`PlatformError::InvalidFrameBuffer`] if `frame.rgba.len()` does not match
/// `frame.expected_byte_len()`, or [`PlatformError::InvalidCropRect`] if the clamped crop region
/// has zero width or height.
pub fn crop_frame(frame: &CaptureFrame, rect: PhysicalRect) -> Result<CaptureFrame, PlatformError> {
    let expected = frame.expected_byte_len();
    if frame.rgba.len() != expected {
        return Err(PlatformError::InvalidFrameBuffer {
            width: frame.width,
            height: frame.height,
            expected,
            actual: frame.rgba.len(),
        });
    }

    let x0 = rect.x.min(frame.width);
    let y0 = rect.y.min(frame.height);
    let x1 = rect.x.saturating_add(rect.width).min(frame.width);
    let y1 = rect.y.saturating_add(rect.height).min(frame.height);

    let crop_w = x1.saturating_sub(x0);
    let crop_h = y1.saturating_sub(y0);

    if crop_w == 0 || crop_h == 0 {
        return Err(PlatformError::InvalidCropRect);
    }

    let src_stride = (frame.width as usize).saturating_mul(4);
    let dst_row_bytes = (crop_w as usize).saturating_mul(4);
    let col_offset = (x0 as usize).saturating_mul(4);
    let total_bytes = dst_row_bytes.saturating_mul(crop_h as usize);

    let mut rgba = Vec::with_capacity(total_bytes);
    for row in y0..y1 {
        let row_start = (row as usize)
            .saturating_mul(src_stride)
            .saturating_add(col_offset);
        let row_end = row_start.saturating_add(dst_row_bytes);
        let slice = frame
            .rgba
            .get(row_start..row_end)
            .ok_or(PlatformError::InvalidCropRect)?;
        rgba.extend_from_slice(slice);
    }

    Ok(CaptureFrame {
        width: crop_w,
        height: crop_h,
        scale_factor: frame.scale_factor,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use sts_core::geometry::{
        logical_to_physical_crop, normalize_drag_rect, physical_to_logical_rect,
    };

    use super::*;

    fn synthetic_frame(width: u32, height: u32, scale_factor: f32) -> CaptureFrame {
        let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[(x & 0xFF) as u8, (y & 0xFF) as u8, 0, 255]);
            }
        }
        CaptureFrame {
            width,
            height,
            scale_factor,
            rgba,
        }
    }

    #[test]
    fn crop_frame_extracts_exact_subregion_rgba_and_rejects_zero_size() -> Result<(), PlatformError>
    {
        let frame = synthetic_frame(4, 4, 1.5);
        let cropped = crop_frame(
            &frame,
            PhysicalRect {
                x: 1,
                y: 2,
                width: 2,
                height: 2,
            },
        )?;

        assert_eq!(cropped.width, 2);
        assert_eq!(cropped.height, 2);
        assert_eq!(cropped.scale_factor, 1.5);
        assert_eq!(
            cropped.rgba,
            vec![
                1, 2, 0, 255, 2, 2, 0, 255, // row y=2: (1,2), (2,2)
                1, 3, 0, 255, 2, 3, 0, 255, // row y=3: (1,3), (2,3)
            ]
        );

        let zero_w = crop_frame(
            &frame,
            PhysicalRect {
                x: 1,
                y: 1,
                width: 0,
                height: 2,
            },
        );
        assert!(matches!(zero_w, Err(PlatformError::InvalidCropRect)));

        let zero_h = crop_frame(
            &frame,
            PhysicalRect {
                x: 1,
                y: 1,
                width: 2,
                height: 0,
            },
        );
        assert!(matches!(zero_h, Err(PlatformError::InvalidCropRect)));
        Ok(())
    }

    #[test]
    fn crop_frame_clamps_overflowing_edges_and_rejects_completely_out_of_bounds()
    -> Result<(), PlatformError> {
        let frame = synthetic_frame(4, 4, 2.0);

        let clamped = crop_frame(
            &frame,
            PhysicalRect {
                x: 2,
                y: 3,
                width: u32::MAX,
                height: 50,
            },
        )?;
        assert_eq!(clamped.width, 2);
        assert_eq!(clamped.height, 1);
        assert_eq!(clamped.rgba, vec![2, 3, 0, 255, 3, 3, 0, 255]);

        let outside = crop_frame(
            &frame,
            PhysicalRect {
                x: 4,
                y: 0,
                width: 2,
                height: 2,
            },
        );
        assert!(matches!(outside, Err(PlatformError::InvalidCropRect)));
        Ok(())
    }

    #[test]
    fn crop_frame_rejects_mismatched_framebuffer_length() {
        let bad_frame = CaptureFrame {
            width: 4,
            height: 4,
            scale_factor: 1.0,
            rgba: vec![0; 12],
        };
        let err = crop_frame(
            &bad_frame,
            PhysicalRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        );
        assert!(matches!(
            err,
            Err(PlatformError::InvalidFrameBuffer {
                width: 4,
                height: 4,
                expected: 64,
                actual: 12,
            })
        ));
    }

    #[test]
    fn hidpi_reverse_drag_on_negative_origin_monitor_crops_cleanly() -> Result<(), PlatformError> {
        // Review Focus #1: reverse drag + edge overflow on 125%, 150%, and 200% DPI negative-origin monitors
        let monitor_150 = MonitorGeometry {
            id: 2,
            name: "LeftHiDPI150".into(),
            x: -120,
            y: -60,
            width: 120,
            height: 90,
            scale_factor: 1.5,
            is_primary: false,
        };
        let frame_150 = synthetic_frame(
            monitor_150.width,
            monitor_150.height,
            monitor_150.scale_factor,
        );
        let logical_150 = normalize_drag_rect(100.0, 80.0, 20.0, 10.0, 80.0, 60.0);
        let phys_150 = logical_to_physical_crop(logical_150, &monitor_150);
        assert_eq!(
            phys_150,
            PhysicalRect {
                x: 30,
                y: 15,
                width: 90,
                height: 75,
            }
        );

        let cropped_150 = crop_frame(&frame_150, phys_150)?;
        assert_eq!(cropped_150.width, 90);
        assert_eq!(cropped_150.height, 75);
        assert_eq!(cropped_150.rgba.len(), 90 * 75 * 4);
        assert_eq!(&cropped_150.rgba[0..4], &[30, 15, 0, 255]);
        let last = cropped_150.rgba.len() - 4;
        assert_eq!(&cropped_150.rgba[last..], &[119, 89, 0, 255]);

        let monitor_125 = MonitorGeometry {
            id: 3,
            name: "TopLeft125".into(),
            x: -200,
            y: -100,
            width: 200,
            height: 100,
            scale_factor: 1.25,
            is_primary: false,
        };
        let frame_125 = synthetic_frame(
            monitor_125.width,
            monitor_125.height,
            monitor_125.scale_factor,
        );
        let logical_125 = normalize_drag_rect(160.0, 90.0, -10.0, 20.0, 160.0, 80.0);
        let phys_125 = logical_to_physical_crop(logical_125, &monitor_125);
        let cropped_125 = crop_frame(&frame_125, phys_125)?;
        assert_eq!(cropped_125.width, 200);
        assert_eq!(cropped_125.height, 75);
        assert_eq!(&cropped_125.rgba[0..4], &[0, 25, 0, 255]);

        let monitor_200 = MonitorGeometry {
            id: 4,
            name: "HiDPI200".into(),
            x: -200,
            y: 0,
            width: 200,
            height: 160,
            scale_factor: 2.0,
            is_primary: false,
        };
        let frame_200 = synthetic_frame(
            monitor_200.width,
            monitor_200.height,
            monitor_200.scale_factor,
        );
        let logical_200 = normalize_drag_rect(120.0, 100.0, 10.0, 15.0, 100.0, 80.0);
        let phys_200 = logical_to_physical_crop(logical_200, &monitor_200);
        let cropped_200 = crop_frame(&frame_200, phys_200)?;
        assert_eq!(cropped_200.width, 180);
        assert_eq!(cropped_200.height, 130);
        assert_eq!(&cropped_200.rgba[0..4], &[20, 30, 0, 255]);

        Ok(())
    }

    struct MinimalFakeCapturer;

    impl ScreenCapturer for MinimalFakeCapturer {
        fn list_monitors(&self) -> Result<Vec<MonitorGeometry>, PlatformError> {
            Ok(Vec::new())
        }

        fn capture_monitor(&self, monitor_id: u32) -> Result<CaptureFrame, PlatformError> {
            Err(PlatformError::MonitorNotFound(monitor_id))
        }

        fn capture_active_monitor(&self) -> Result<(MonitorGeometry, CaptureFrame), PlatformError> {
            Err(PlatformError::NoMonitorsFound)
        }
    }

    #[test]
    fn screen_capturer_default_window_methods_preserve_minimal_implementations()
    -> Result<(), PlatformError> {
        let capturer = MinimalFakeCapturer;
        assert!(capturer.list_windows()?.is_empty());

        let err = capturer.capture_window(4242);
        assert!(matches!(err, Err(PlatformError::WindowNotFound(4242))));
        Ok(())
    }

    #[test]
    fn filter_visible_windows_drops_minimized_and_zero_sized_windows() {
        let windows = vec![
            WindowGeometry {
                id: 1,
                title: String::from("Manga Reader"),
                app_name: String::from("firefox"),
                x: -1920,
                y: 0,
                width: 1280,
                height: 900,
                scale_factor: 1.5,
                is_minimized: false,
            },
            WindowGeometry {
                id: 2,
                title: String::from("Minimized Window"),
                app_name: String::from("reader"),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                scale_factor: 1.0,
                is_minimized: true,
            },
            WindowGeometry {
                id: 3,
                title: String::from("Zero Width"),
                app_name: String::from("hidden"),
                x: 0,
                y: 0,
                width: 0,
                height: 500,
                scale_factor: 1.0,
                is_minimized: false,
            },
            WindowGeometry {
                id: 4,
                title: String::from("Zero Height"),
                app_name: String::from("hidden"),
                x: 0,
                y: 0,
                width: 500,
                height: 0,
                scale_factor: 1.0,
                is_minimized: false,
            },
        ];

        let visible = filter_visible_windows(windows);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, 1);
        assert_eq!(visible[0].x, -1920);
    }

    #[test]
    fn sync_window_geometry_updates_dimensions_from_captured_frame() {
        let mut geom = WindowGeometry {
            id: 7,
            title: String::from("Comic Viewer"),
            app_name: String::from("viewer"),
            x: 100,
            y: 50,
            width: 800,
            height: 600,
            scale_factor: 1.25,
            is_minimized: false,
        };
        let frame = synthetic_frame(1000, 750, 1.25);

        sync_window_geometry_to_frame(&mut geom, &frame);
        assert_eq!(geom.width, 1000);
        assert_eq!(geom.height, 750);
    }

    #[test]
    fn platform_error_display_covers_window_not_found_and_overlay_style_failed() {
        let not_found = PlatformError::WindowNotFound(123).to_string();
        assert!(not_found.contains("123"));

        let style_failed = PlatformError::OverlayStyleFailed(String::from("bad hwnd")).to_string();
        assert!(style_failed.contains("bad hwnd"));
    }

    #[test]
    fn xcap_capturer_rejects_zero_window_id_and_is_dyn_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn ScreenCapturer>();

        let capturer = XcapCapturer::new();
        let res = capturer.capture_window(0);
        assert!(
            matches!(res, Err(PlatformError::WindowNotFound(0))),
            "expected WindowNotFound(0), got {res:?}"
        );
    }

    #[test]
    fn crop_frame_supports_negative_origin_and_all_fractional_hidpi_scales()
    -> Result<(), PlatformError> {
        for (idx, scale_factor) in [1.0_f32, 1.25, 1.5, 1.75, 2.0].into_iter().enumerate() {
            let monitor = MonitorGeometry {
                id: u32::try_from(idx + 10).unwrap_or(10),
                name: format!("NegOrigin-{scale_factor}"),
                x: -2560,
                y: -1440,
                width: 280,
                height: 210,
                scale_factor,
                is_primary: false,
            };
            let frame = synthetic_frame(monitor.width, monitor.height, monitor.scale_factor);
            let viewport_w = f32::from(u16::try_from(monitor.width).unwrap_or(280)) / scale_factor;
            let viewport_h = f32::from(u16::try_from(monitor.height).unwrap_or(210)) / scale_factor;

            // Drag extending past right/bottom edge on a negative-origin fractional HiDPI monitor.
            let logical = normalize_drag_rect(
                24.0,
                16.0,
                viewport_w + 40.0,
                viewport_h + 25.0,
                viewport_w,
                viewport_h,
            );
            let phys = logical_to_physical_crop(logical, &monitor);
            assert!(phys.x + phys.width <= monitor.width);
            assert!(phys.y + phys.height <= monitor.height);

            let cropped = crop_frame(&frame, phys)?;
            assert_eq!(cropped.width, phys.width);
            assert_eq!(cropped.height, phys.height);
            assert_eq!(cropped.scale_factor, scale_factor);
            assert_eq!(cropped.rgba.len(), cropped.expected_byte_len());
            assert_eq!(
                &cropped.rgba[0..4],
                &[(phys.x & 0xFF) as u8, (phys.y & 0xFF) as u8, 0, 255]
            );

            let back_to_logical = physical_to_logical_rect(phys, &monitor);
            assert!(back_to_logical.x >= 0.0);
            assert!(back_to_logical.y >= 0.0);
            assert!(back_to_logical.width > 0.0);
            assert!(back_to_logical.height > 0.0);
        }
        Ok(())
    }

    #[test]
    fn crop_frame_clamps_fractional_rounding_overshoot_on_negative_origin_monitor()
    -> Result<(), PlatformError> {
        let monitor = MonitorGeometry {
            id: 99,
            name: String::from("Left175"),
            x: -1920,
            y: -1080,
            width: 175,
            height: 140,
            scale_factor: 1.75,
            is_primary: false,
        };
        let frame = synthetic_frame(monitor.width, monitor.height, monitor.scale_factor);

        // Simulate a 1-2px fractional rounding overshoot past the right/bottom framebuffer edges.
        let overshoot = PhysicalRect {
            x: 75,
            y: 40,
            width: 102,
            height: 103,
        };
        let cropped = crop_frame(&frame, overshoot)?;
        assert_eq!(cropped.width, 100);
        assert_eq!(cropped.height, 100);
        assert_eq!(cropped.scale_factor, 1.75);
        assert_eq!(&cropped.rgba[0..4], &[75, 40, 0, 255]);
        let last = cropped.rgba.len() - 4;
        assert_eq!(&cropped.rgba[last..], &[174, 139, 0, 255]);
        Ok(())
    }
}
