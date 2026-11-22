//! Coordinate types and HiDPI / multi-monitor crop math separating logical UI coordinates
//! from physical framebuffer pixels.

use serde::{Deserialize, Serialize};

/// Axis-aligned rectangle in physical framebuffer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct PhysicalRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl PhysicalRect {
    /// Returns `true` when either dimension is zero.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Axis-aligned rectangle in logical UI coordinates (scaled by a display's `scale_factor`).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LogicalRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Geometry and DPI metadata for a single monitor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorGeometry {
    pub id: u32,
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
    pub is_primary: bool,
}

fn clamp_coord(val: f32, max: f32) -> f32 {
    if val.is_nan() {
        0.0
    } else {
        val.clamp(0.0, max)
    }
}

/// Normalizes an arbitrary drag gesture (including reverse right-to-left or bottom-to-top drags
/// and out-of-bounds endpoints) into a non-negative [`LogicalRect`] clamped within the viewport.
#[must_use]
pub fn normalize_drag_rect(
    start_x: f32,
    start_y: f32,
    end_x: f32,
    end_y: f32,
    viewport_w: f32,
    viewport_h: f32,
) -> LogicalRect {
    let max_w = if viewport_w.is_finite() {
        viewport_w.max(0.0)
    } else {
        0.0
    };
    let max_h = if viewport_h.is_finite() {
        viewport_h.max(0.0)
    } else {
        0.0
    };

    let x1 = clamp_coord(start_x, max_w);
    let x2 = clamp_coord(end_x, max_w);
    let y1 = clamp_coord(start_y, max_h);
    let y2 = clamp_coord(end_y, max_h);

    let min_x = x1.min(x2);
    let max_x = x1.max(x2);
    let min_y = y1.min(y2);
    let max_y = y1.max(y2);

    LogicalRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

fn sanitize_scale(scale_factor: f32) -> f64 {
    if scale_factor.is_finite() {
        f64::from(scale_factor.max(0.1))
    } else {
        1.0
    }
}

/// Converts a logical selection rectangle on a monitor overlay into a physical pixel crop
/// rectangle clamped strictly within `0..monitor.width` and `0..monitor.height`.
#[must_use]
pub fn logical_to_physical_crop(rect: LogicalRect, monitor: &MonitorGeometry) -> PhysicalRect {
    if !rect.x.is_finite()
        || !rect.y.is_finite()
        || !rect.width.is_finite()
        || !rect.height.is_finite()
    {
        return PhysicalRect::default();
    }

    let scale = sanitize_scale(monitor.scale_factor);

    let max_w = f64::from(monitor.width);
    let max_h = f64::from(monitor.height);

    let lx0 = f64::from(rect.x);
    let ly0 = f64::from(rect.y);
    let lx1 = lx0 + f64::from(rect.width.max(0.0));
    let ly1 = ly0 + f64::from(rect.height.max(0.0));

    let px0 = (lx0 * scale).round().clamp(0.0, max_w) as u32;
    let py0 = (ly0 * scale).round().clamp(0.0, max_h) as u32;
    let px1 = (lx1 * scale).round().clamp(0.0, max_w) as u32;
    let py1 = (ly1 * scale).round().clamp(0.0, max_h) as u32;

    PhysicalRect {
        x: px0,
        y: py0,
        width: px1.saturating_sub(px0),
        height: py1.saturating_sub(py0),
    }
}

/// Converts a physical pixel rectangle on a monitor into logical UI coordinates clamped within
/// the monitor's bounds.
#[must_use]
pub fn physical_to_logical_rect(rect: PhysicalRect, monitor: &MonitorGeometry) -> LogicalRect {
    let scale = sanitize_scale(monitor.scale_factor);
    let px0 = rect.x.min(monitor.width);
    let py0 = rect.y.min(monitor.height);
    let px1 = rect.x.saturating_add(rect.width).min(monitor.width);
    let py1 = rect.y.saturating_add(rect.height).min(monitor.height);

    LogicalRect {
        x: (f64::from(px0) / scale) as f32,
        y: (f64::from(py0) / scale) as f32,
        width: (f64::from(px1.saturating_sub(px0)) / scale) as f32,
        height: (f64::from(py1.saturating_sub(py0)) / scale) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_drag_rect_handles_reverse_drag_and_clamps_to_viewport() {
        let rect = normalize_drag_rect(500.0, 400.0, -20.0, 100.0, 800.0, 600.0);
        assert_eq!(
            rect,
            LogicalRect {
                x: 0.0,
                y: 100.0,
                width: 500.0,
                height: 300.0,
            }
        );
    }

    #[test]
    fn normalize_drag_rect_handles_bottom_to_top_and_right_overflow() {
        let rect = normalize_drag_rect(950.0, 720.0, 200.0, 50.0, 800.0, 600.0);
        assert_eq!(
            rect,
            LogicalRect {
                x: 200.0,
                y: 50.0,
                width: 600.0,
                height: 550.0,
            }
        );
    }

    #[test]
    fn normalize_drag_rect_handles_non_finite_and_negative_viewport() {
        let nan_rect = normalize_drag_rect(f32::NAN, 10.0, 100.0, f32::INFINITY, 800.0, 600.0);
        assert_eq!(
            nan_rect,
            LogicalRect {
                x: 0.0,
                y: 10.0,
                width: 100.0,
                height: 590.0,
            }
        );

        let neg_vp = normalize_drag_rect(10.0, 10.0, 50.0, 50.0, -100.0, -50.0);
        assert_eq!(
            neg_vp,
            LogicalRect {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 0.0,
            }
        );
    }

    #[test]
    fn logical_to_physical_crop_scales_hidpi_and_clamps_within_frame() {
        let monitor = MonitorGeometry {
            id: 1,
            name: "Left".into(),
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
            scale_factor: 1.5,
            is_primary: false,
        };
        let logical = LogicalRect {
            x: 100.0,
            y: 50.0,
            width: 2000.0,
            height: 800.0,
        };
        let crop = logical_to_physical_crop(logical, &monitor);
        assert_eq!(
            crop,
            PhysicalRect {
                x: 150,
                y: 75,
                width: 1770,
                height: 1005,
            }
        );
    }

    #[test]
    fn logical_to_physical_crop_handles_125_and_200_dpi_on_negative_origin_monitors() {
        let mon_125 = MonitorGeometry {
            id: 2,
            name: "TopLeft".into(),
            x: -2560,
            y: -1440,
            width: 2560,
            height: 1440,
            scale_factor: 1.25,
            is_primary: false,
        };
        let crop_125 = logical_to_physical_crop(
            LogicalRect {
                x: 40.0,
                y: 80.0,
                width: 200.0,
                height: 120.0,
            },
            &mon_125,
        );
        assert_eq!(
            crop_125,
            PhysicalRect {
                x: 50,
                y: 100,
                width: 250,
                height: 150,
            }
        );

        let mon_200 = MonitorGeometry {
            id: 3,
            name: "HiDPI4K".into(),
            x: -3840,
            y: -2160,
            width: 3840,
            height: 2160,
            scale_factor: 2.0,
            is_primary: false,
        };
        let crop_200 = logical_to_physical_crop(
            LogicalRect {
                x: 100.0,
                y: 200.0,
                width: 1850.0,
                height: 950.0,
            },
            &mon_200,
        );
        assert_eq!(
            crop_200,
            PhysicalRect {
                x: 200,
                y: 400,
                width: 3640,
                height: 1760,
            }
        );
        assert!(crop_200.x + crop_200.width <= mon_200.width);
        assert!(crop_200.y + crop_200.height <= mon_200.height);
    }

    #[test]
    fn logical_to_physical_crop_clamps_invalid_scale_and_out_of_bounds_rects() {
        let monitor = MonitorGeometry {
            id: 4,
            name: "ZeroScale".into(),
            x: 0,
            y: 0,
            width: 1000,
            height: 800,
            scale_factor: 0.0,
            is_primary: true,
        };
        let scaled = logical_to_physical_crop(
            LogicalRect {
                x: 100.0,
                y: 200.0,
                width: 300.0,
                height: 400.0,
            },
            &monitor,
        );
        assert_eq!(
            scaled,
            PhysicalRect {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            }
        );

        let past_edge = logical_to_physical_crop(
            LogicalRect {
                x: 50_000.0,
                y: 50_000.0,
                width: 100.0,
                height: 100.0,
            },
            &monitor,
        );
        assert!(past_edge.is_empty());
        assert!(past_edge.x + past_edge.width <= monitor.width);
        assert!(past_edge.y + past_edge.height <= monitor.height);

        let partial_neg = logical_to_physical_crop(
            LogicalRect {
                x: -50.0,
                y: -20.0,
                width: 200.0,
                height: 100.0,
            },
            &MonitorGeometry {
                scale_factor: 1.0,
                ..monitor
            },
        );
        assert_eq!(
            partial_neg,
            PhysicalRect {
                x: 0,
                y: 0,
                width: 150,
                height: 80,
            }
        );
    }

    #[test]
    fn fractional_hidpi_and_negative_origin_physical_roundtrip() {
        let scales = [1.0_f32, 1.25, 1.5, 1.75, 2.0];
        let sample_rects = [
            PhysicalRect {
                x: 0,
                y: 0,
                width: 320,
                height: 180,
            },
            PhysicalRect {
                x: 101,
                y: 77,
                width: 333,
                height: 221,
            },
            PhysicalRect {
                x: 1800,
                y: 900,
                width: 760,
                height: 540,
            },
        ];

        for scale in scales {
            let monitor = MonitorGeometry {
                id: 10,
                name: format!("NegOrigin-{scale}"),
                x: -2560,
                y: -1440,
                width: 2560,
                height: 1440,
                scale_factor: scale,
                is_primary: false,
            };

            for &phys in &sample_rects {
                let logical = physical_to_logical_rect(phys, &monitor);
                assert!(logical.x.is_finite() && logical.y.is_finite());
                assert!(logical.width.is_finite() && logical.height.is_finite());
                assert!(logical.x >= 0.0 && logical.y >= 0.0);

                let roundtrip = logical_to_physical_crop(logical, &monitor);
                assert_eq!(
                    roundtrip, phys,
                    "roundtrip mismatch at scale {scale} for {phys:?} via {logical:?}"
                );
                assert!(roundtrip.x + roundtrip.width <= monitor.width);
                assert!(roundtrip.y + roundtrip.height <= monitor.height);
            }

            let overflow_logical = LogicalRect {
                x: -40.0,
                y: -25.0,
                width: 5000.0,
                height: 3000.0,
            };
            let clamped_crop = logical_to_physical_crop(overflow_logical, &monitor);
            assert!(clamped_crop.x + clamped_crop.width <= monitor.width);
            assert!(clamped_crop.y + clamped_crop.height <= monitor.height);

            let overflow_physical = PhysicalRect {
                x: 2400,
                y: 1300,
                width: 500,
                height: 500,
            };
            let clamped_logical = physical_to_logical_rect(overflow_physical, &monitor);
            let reclamped_crop = logical_to_physical_crop(clamped_logical, &monitor);
            assert!(reclamped_crop.x + reclamped_crop.width <= monitor.width);
            assert!(reclamped_crop.y + reclamped_crop.height <= monitor.height);
            assert_eq!(
                reclamped_crop,
                PhysicalRect {
                    x: 2400,
                    y: 1300,
                    width: 160,
                    height: 140,
                }
            );
        }
    }

    #[test]
    fn physical_to_logical_rect_sanitizes_invalid_scale_factors() {
        let phys = PhysicalRect {
            x: 10,
            y: 20,
            width: 30,
            height: 40,
        };
        let zero_scale = MonitorGeometry {
            id: 11,
            name: "Zero".into(),
            x: 0,
            y: 0,
            width: 1000,
            height: 800,
            scale_factor: 0.0,
            is_primary: true,
        };
        assert_eq!(
            physical_to_logical_rect(phys, &zero_scale),
            LogicalRect {
                x: 100.0,
                y: 200.0,
                width: 300.0,
                height: 400.0,
            }
        );

        let nan_scale = MonitorGeometry {
            scale_factor: f32::NAN,
            ..zero_scale
        };
        assert_eq!(
            physical_to_logical_rect(phys, &nan_scale),
            LogicalRect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            }
        );
    }
}
