//! Speech-bubble clustering (horizontal and vertical CJK/comic layouts), adaptive backdrop color
//! sampling, WCAG text contrast selection, and auto-fit Vietnamese typography math.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::geometry::PhysicalRect;
use crate::types::{CaptureFrame, OcrLine, ReadingDirection, TextBlock};

/// Fallback solid white backdrop when a frame or bounding box is invalid.
const FALLBACK_BG_RGBA: [u8; 4] = [255, 255, 255, 255];

/// High-contrast dark ink text color (`#111827`) used on light speech bubbles.
const DARK_TEXT_RGBA: [u8; 4] = [17, 24, 39, 255];

/// High-contrast light text color (`#F8FAFC`) used on dark speech bubbles.
const LIGHT_TEXT_RGBA: [u8; 4] = [248, 250, 252, 255];

/// Geometric and confidence thresholds controlling [`cluster_ocr_lines_with_config`].
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterConfig {
    /// Minimum OCR recognition confidence in `0.0..=1.0`; lines below this are dropped.
    pub min_confidence: f32,
    /// Maximum horizontal gap between vertical columns as a multiple of `max(w_a, w_b)`.
    pub horizontal_gap_factor: f32,
    /// Maximum vertical gap between horizontal lines as a multiple of `max(h_a, h_b)`.
    pub vertical_gap_factor: f32,
    /// Row-band tolerance as a multiple of the median block height when ordering bubbles.
    pub band_tolerance_factor: f32,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            min_confidence: 0.3,
            horizontal_gap_factor: 1.25,
            vertical_gap_factor: 0.9,
            band_tolerance_factor: 0.6,
        }
    }
}

/// Clusters OCR lines into ordered speech-bubble [`TextBlock`]s using [`ClusterConfig::default`].
#[must_use]
pub fn cluster_ocr_lines(lines: &[OcrLine], direction: ReadingDirection) -> Vec<TextBlock> {
    cluster_ocr_lines_with_config(lines, direction, &ClusterConfig::default())
}

/// Clusters OCR lines into ordered speech-bubble [`TextBlock`]s with explicit thresholds.
#[must_use]
pub fn cluster_ocr_lines_with_config(
    lines: &[OcrLine],
    direction: ReadingDirection,
    cfg: &ClusterConfig,
) -> Vec<TextBlock> {
    let defaults = ClusterConfig::default();
    let min_confidence = if cfg.min_confidence.is_finite() {
        cfg.min_confidence.clamp(0.0, 1.0)
    } else {
        defaults.min_confidence
    };
    let h_gap_factor = if cfg.horizontal_gap_factor.is_finite() {
        cfg.horizontal_gap_factor.max(0.0)
    } else {
        defaults.horizontal_gap_factor
    };
    let v_gap_factor = if cfg.vertical_gap_factor.is_finite() {
        cfg.vertical_gap_factor.max(0.0)
    } else {
        defaults.vertical_gap_factor
    };
    let band_tolerance_factor = if cfg.band_tolerance_factor.is_finite() {
        cfg.band_tolerance_factor.max(0.0)
    } else {
        defaults.band_tolerance_factor
    };

    let filtered: Vec<&OcrLine> = lines
        .iter()
        .filter(|line| {
            line.confidence.is_finite()
                && line.confidence >= min_confidence
                && !line.bbox.is_empty()
                && !line.text.trim().is_empty()
        })
        .collect();

    if filtered.is_empty() {
        return Vec::new();
    }

    let mut dsu = DisjointSet::new(filtered.len());
    for i in 0..filtered.len() {
        for j in (i + 1)..filtered.len() {
            if should_merge(filtered[i], filtered[j], h_gap_factor, v_gap_factor) {
                dsu.union(i, j);
            }
        }
    }

    let mut groups: BTreeMap<usize, Vec<&OcrLine>> = BTreeMap::new();
    for (idx, &line) in filtered.iter().enumerate() {
        let root = dsu.find(idx);
        groups.entry(root).or_default().push(line);
    }

    let mut blocks: Vec<TextBlock> = groups
        .into_values()
        .filter_map(|members| build_text_block(members))
        .collect();

    let resolved_direction = resolve_reading_direction(direction, &blocks);
    sort_blocks_in_reading_order(&mut blocks, resolved_direction, band_tolerance_factor);
    blocks
}

/// Samples a 1-2px perimeter ring around `bbox` clamped to frame bounds and returns the
/// per-channel median RGBA backdrop (robust against interior text ink touching box edges).
///
/// Falls back to opaque white `[255, 255, 255, 255]` if `frame` or `bbox` is invalid.
#[must_use]
pub fn sample_bubble_background(frame: &CaptureFrame, bbox: PhysicalRect) -> [u8; 4] {
    if frame.width == 0 || frame.height == 0 || bbox.is_empty() {
        return FALLBACK_BG_RGBA;
    }

    let width = frame.width as usize;
    let height = frame.height as usize;
    let Some(expected_len) = width.checked_mul(height).and_then(|n| n.checked_mul(4)) else {
        return FALLBACK_BG_RGBA;
    };
    if frame.rgba.len() != expected_len || bbox.x >= frame.width || bbox.y >= frame.height {
        return FALLBACK_BG_RGBA;
    }

    let max_x = frame.width - 1;
    let max_y = frame.height - 1;
    let x0 = bbox.x.min(max_x);
    let y0 = bbox.y.min(max_y);
    let x1 = bbox
        .x
        .saturating_add(bbox.width)
        .saturating_sub(1)
        .min(max_x);
    let y1 = bbox
        .y
        .saturating_add(bbox.height)
        .saturating_sub(1)
        .min(max_y);

    let mut hist_r = [0u32; 256];
    let mut hist_g = [0u32; 256];
    let mut hist_b = [0u32; 256];
    let mut count = 0u32;

    let mut push_pixel = |rx: u32, ry: u32| {
        let idx = (ry as usize * width + rx as usize) * 4;
        if let Some([r, g, b, _]) = frame.rgba.get(idx..idx + 4) {
            hist_r[usize::from(*r)] = hist_r[usize::from(*r)].saturating_add(1);
            hist_g[usize::from(*g)] = hist_g[usize::from(*g)].saturating_add(1);
            hist_b[usize::from(*b)] = hist_b[usize::from(*b)].saturating_add(1);
            count = count.saturating_add(1);
        }
    };

    for offset in [1u32, 2u32] {
        let left = x0.saturating_sub(offset);
        let right = x1.saturating_add(offset).min(max_x);
        let top = y0.saturating_sub(offset);
        let bottom = y1.saturating_add(offset).min(max_y);

        for x in left..=right {
            push_pixel(x, top);
            push_pixel(x, bottom);
        }
        for y in y0..=y1 {
            push_pixel(left, y);
            push_pixel(right, y);
        }
    }

    if count == 0 {
        return FALLBACK_BG_RGBA;
    }

    [
        histogram_median(&hist_r, count),
        histogram_median(&hist_g, count),
        histogram_median(&hist_b, count),
        255,
    ]
}

/// Selects high-contrast dark (`[17, 24, 39, 255]`) or light (`[248, 250, 252, 255]`) RGBA text
/// color against `bg_rgba` using relative luminance (`L = 0.2126*R + 0.7152*G + 0.0722*B`).
#[must_use]
pub fn contrasting_text_color(bg_rgba: [u8; 4]) -> [u8; 4] {
    let luminance = 0.2126 * f32::from(bg_rgba[0])
        + 0.7152 * f32::from(bg_rgba[1])
        + 0.0722 * f32::from(bg_rgba[2]);
    if luminance >= 140.0 {
        DARK_TEXT_RGBA
    } else {
        LIGHT_TEXT_RGBA
    }
}

/// Computes the largest font size in `min_font_px..=base_font_px` estimated to fit wrapped `text`
/// inside `(logical_w, logical_h)`, guarding against NaN, infinity, zero, or negative inputs.
#[must_use]
pub fn compute_fitted_font_size(
    text: &str,
    logical_w: f32,
    logical_h: f32,
    base_font_px: f32,
    min_font_px: f32,
) -> f32 {
    let min_px = if min_font_px.is_finite() {
        min_font_px.max(1.0)
    } else {
        1.0
    };
    let base_px = if base_font_px.is_finite() {
        base_font_px.max(min_px)
    } else {
        min_px
    };

    if !logical_w.is_finite() || !logical_h.is_finite() || logical_w <= 0.0 || logical_h <= 0.0 {
        return min_px;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return base_px;
    }

    let pad = (logical_w.min(logical_h) * 0.08).clamp(2.0, 8.0);
    let inner_w = (logical_w - 2.0 * pad).max(1.0);
    let inner_h = (logical_h - 2.0 * pad).max(1.0);

    let fits = |font: f32| -> bool {
        let chars_per_line = (inner_w / (font * 0.56)).max(1.0);
        let line_height = font * 1.28;
        let lines = estimate_wrapped_lines(trimmed, chars_per_line) as f32;
        lines * line_height <= inner_h
    };

    if fits(base_px) {
        return base_px;
    }
    if !fits(min_px) {
        return min_px;
    }

    let mut low = min_px;
    let mut high = base_px;
    for _ in 0..16 {
        let mid = 0.5 * (low + high);
        if fits(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }

    low.clamp(min_px, base_px)
}

struct DisjointSet {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl DisjointSet {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            let grand = self.parent[self.parent[x]];
            self.parent[x] = grand;
            x = grand;
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            Ordering::Less => self.parent[ra] = rb,
            Ordering::Greater => self.parent[rb] = ra,
            Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] = self.rank[ra].saturating_add(1);
            }
        }
    }
}

fn interval_gap(a0: u32, a1: u32, b0: u32, b1: u32) -> u32 {
    b0.saturating_sub(a1).max(a0.saturating_sub(b1))
}

fn should_merge(a: &OcrLine, b: &OcrLine, h_gap_factor: f32, v_gap_factor: f32) -> bool {
    if a.is_vertical != b.is_vertical {
        return false;
    }

    let h_gap = interval_gap(
        a.bbox.x,
        a.bbox.x.saturating_add(a.bbox.width),
        b.bbox.x,
        b.bbox.x.saturating_add(b.bbox.width),
    );
    let v_gap = interval_gap(
        a.bbox.y,
        a.bbox.y.saturating_add(a.bbox.height),
        b.bbox.y,
        b.bbox.y.saturating_add(b.bbox.height),
    );

    if a.is_vertical {
        let avg_col_w = 0.5 * (a.bbox.width as f32 + b.bbox.width as f32);
        let max_w = a.bbox.width.max(b.bbox.width) as f32;
        (v_gap as f32) <= 0.4 * avg_col_w && (h_gap as f32) <= h_gap_factor * max_w
    } else {
        let cx_a = rect_center_x(a.bbox);
        let cx_b = rect_center_x(b.bbox);
        let max_w = a.bbox.width.max(b.bbox.width) as f32;
        let max_h = a.bbox.height.max(b.bbox.height) as f32;
        (h_gap == 0 || (cx_a - cx_b).abs() <= 0.6 * max_w) && (v_gap as f32) <= v_gap_factor * max_h
    }
}

fn build_text_block(mut members: Vec<&OcrLine>) -> Option<TextBlock> {
    let first = *members.first()?;
    let is_vertical = first.is_vertical;

    let mut min_x = first.bbox.x;
    let mut min_y = first.bbox.y;
    let mut max_x = first.bbox.x.saturating_add(first.bbox.width);
    let mut max_y = first.bbox.y.saturating_add(first.bbox.height);
    for line in &members[1..] {
        min_x = min_x.min(line.bbox.x);
        min_y = min_y.min(line.bbox.y);
        max_x = max_x.max(line.bbox.x.saturating_add(line.bbox.width));
        max_y = max_y.max(line.bbox.y.saturating_add(line.bbox.height));
    }

    if is_vertical {
        sort_vertical_members(&mut members);
    } else {
        sort_horizontal_members(&mut members);
    }

    let text = join_member_texts(&members, is_vertical);
    if text.is_empty() {
        return None;
    }

    Some(TextBlock {
        bbox: PhysicalRect {
            x: min_x,
            y: min_y,
            width: max_x.saturating_sub(min_x),
            height: max_y.saturating_sub(min_y),
        },
        text,
        is_vertical,
    })
}

fn sort_vertical_members(members: &mut [&OcrLine]) {
    if members.len() <= 1 {
        return;
    }

    members.sort_by(|a, b| {
        rect_center_x(b.bbox)
            .total_cmp(&rect_center_x(a.bbox))
            .then_with(|| a.bbox.y.cmp(&b.bbox.y))
            .then_with(|| b.bbox.x.cmp(&a.bbox.x))
    });

    let avg_w = members
        .iter()
        .map(|line| line.bbox.width as f32)
        .sum::<f32>()
        / members.len() as f32;
    let col_tol = 0.5 * avg_w;

    let mut start = 0usize;
    while start < members.len() {
        let anchor_cx = rect_center_x(members[start].bbox);
        let mut end = start + 1;
        while end < members.len() && (anchor_cx - rect_center_x(members[end].bbox)) <= col_tol {
            end += 1;
        }
        members[start..end].sort_by(|a, b| {
            a.bbox
                .y
                .cmp(&b.bbox.y)
                .then_with(|| b.bbox.x.cmp(&a.bbox.x))
        });
        start = end;
    }
}

fn sort_horizontal_members(members: &mut [&OcrLine]) {
    if members.len() <= 1 {
        return;
    }

    members.sort_by(|a, b| {
        rect_center_y(a.bbox)
            .total_cmp(&rect_center_y(b.bbox))
            .then_with(|| a.bbox.x.cmp(&b.bbox.x))
            .then_with(|| a.bbox.y.cmp(&b.bbox.y))
    });

    let avg_h = members
        .iter()
        .map(|line| line.bbox.height as f32)
        .sum::<f32>()
        / members.len() as f32;
    let row_tol = 0.5 * avg_h;

    let mut start = 0usize;
    while start < members.len() {
        let anchor_cy = rect_center_y(members[start].bbox);
        let mut end = start + 1;
        while end < members.len() && (rect_center_y(members[end].bbox) - anchor_cy) <= row_tol {
            end += 1;
        }
        members[start..end].sort_by(|a, b| {
            a.bbox
                .x
                .cmp(&b.bbox.x)
                .then_with(|| a.bbox.y.cmp(&b.bbox.y))
        });
        start = end;
    }
}

fn join_member_texts(members: &[&OcrLine], is_vertical: bool) -> String {
    let mut out = String::new();
    for line in members {
        let part = line.text.trim();
        if part.is_empty() {
            continue;
        }
        if !out.is_empty() && needs_join_space(&out, part, is_vertical) {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

fn needs_join_space(prev: &str, next: &str, is_vertical: bool) -> bool {
    let (Some(last_ch), Some(first_ch)) = (prev.chars().last(), next.chars().next()) else {
        return false;
    };

    if is_vertical {
        last_ch.is_ascii_alphanumeric() && first_ch.is_ascii_alphanumeric()
    } else {
        !(is_cjk_no_space(last_ch) && is_cjk_no_space(first_ch))
    }
}

fn is_cjk_no_space(c: char) -> bool {
    matches!(
        c,
        '\u{3000}'..='\u{303F}'
            | '\u{3040}'..='\u{309F}'
            | '\u{30A0}'..='\u{30FF}'
            | '\u{3100}'..='\u{312F}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{FF00}'..='\u{FFEF}'
            | '\u{2026}'
            | '\u{2014}'
    )
}

fn resolve_reading_direction(
    direction: ReadingDirection,
    blocks: &[TextBlock],
) -> ReadingDirection {
    match direction {
        ReadingDirection::Auto => {
            let vertical_count = blocks.iter().filter(|b| b.is_vertical).count();
            let horizontal_count = blocks.len().saturating_sub(vertical_count);
            if vertical_count > horizontal_count {
                ReadingDirection::RightToLeft
            } else {
                ReadingDirection::LeftToRight
            }
        }
        other => other,
    }
}

fn sort_blocks_in_reading_order(
    blocks: &mut [TextBlock],
    direction: ReadingDirection,
    band_tolerance_factor: f32,
) {
    if blocks.len() <= 1 {
        return;
    }

    let mut heights: Vec<u32> = blocks.iter().map(|b| b.bbox.height).collect();
    heights.sort_unstable();
    let mid = heights.len() / 2;
    let median_height = if heights.len().is_multiple_of(2) {
        0.5 * (heights[mid - 1] as f32 + heights[mid] as f32)
    } else {
        heights[mid] as f32
    };
    let band_tolerance = band_tolerance_factor * median_height;

    blocks.sort_by(|a, b| {
        rect_center_y(a.bbox)
            .total_cmp(&rect_center_y(b.bbox))
            .then_with(|| a.bbox.y.cmp(&b.bbox.y))
            .then_with(|| compare_x_by_direction(a.bbox, b.bbox, direction))
    });

    let mut start = 0usize;
    while start < blocks.len() {
        let anchor_cy = rect_center_y(blocks[start].bbox);
        let mut end = start + 1;
        while end < blocks.len() && (rect_center_y(blocks[end].bbox) - anchor_cy) <= band_tolerance
        {
            end += 1;
        }
        blocks[start..end].sort_by(|a, b| {
            compare_x_by_direction(a.bbox, b.bbox, direction).then_with(|| a.bbox.y.cmp(&b.bbox.y))
        });
        start = end;
    }
}

fn compare_x_by_direction(
    a: PhysicalRect,
    b: PhysicalRect,
    direction: ReadingDirection,
) -> Ordering {
    match direction {
        ReadingDirection::RightToLeft => rect_center_x(b).total_cmp(&rect_center_x(a)),
        ReadingDirection::LeftToRight | ReadingDirection::Auto => {
            rect_center_x(a).total_cmp(&rect_center_x(b))
        }
    }
}

fn rect_center_x(rect: PhysicalRect) -> f32 {
    rect.x as f32 + 0.5 * rect.width as f32
}

fn rect_center_y(rect: PhysicalRect) -> f32 {
    rect.y as f32 + 0.5 * rect.height as f32
}

fn histogram_median(hist: &[u32; 256], total: u32) -> u8 {
    let target_lo = (total.saturating_sub(1)) / 2;
    let target_hi = total / 2;
    let mut cumulative = 0u32;
    let mut val_lo = None;

    for (value, &freq) in hist.iter().enumerate() {
        cumulative = cumulative.saturating_add(freq);
        if val_lo.is_none() && cumulative > target_lo {
            val_lo = Some(value as u32);
        }
        if cumulative > target_hi {
            let lo = val_lo.unwrap_or(value as u32);
            return ((lo + value as u32) / 2) as u8;
        }
    }

    255
}

fn char_width_units(c: char) -> f32 {
    if ('\u{0300}'..='\u{036F}').contains(&c) {
        0.0
    } else if is_cjk_no_space(c) {
        1.75
    } else {
        1.0
    }
}

fn estimate_wrapped_lines(text: &str, chars_per_line: f32) -> usize {
    let capacity = chars_per_line.max(1.0);
    let mut total_lines = 0usize;

    for paragraph in text.split('\n') {
        let mut line_units = 0.0f32;
        total_lines += 1;

        for word in paragraph.split_whitespace() {
            let word_units = word.chars().map(char_width_units).sum::<f32>().max(1.0);

            if line_units > 0.0 && line_units + 1.0 + word_units <= capacity {
                line_units += 1.0 + word_units;
                continue;
            }
            if line_units > 0.0 {
                total_lines += 1;
            }

            if word_units <= capacity {
                line_units = word_units;
            } else {
                let wrapped = (word_units / capacity).ceil() as usize;
                let extra = wrapped.saturating_sub(1);
                total_lines += extra;
                line_units = (word_units - (extra as f32) * capacity).max(0.0);
            }
        }
    }

    total_lines.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_line(
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        text: &str,
        confidence: f32,
        is_vertical: bool,
    ) -> OcrLine {
        OcrLine {
            bbox: PhysicalRect {
                x,
                y,
                width,
                height,
            },
            text: text.to_owned(),
            confidence,
            is_vertical,
        }
    }

    fn solid_frame(width: u32, height: u32, rgba: [u8; 4]) -> CaptureFrame {
        let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for _ in 0..(width as usize) * (height as usize) {
            buf.extend_from_slice(&rgba);
        }
        CaptureFrame {
            width,
            height,
            scale_factor: 1.0,
            rgba: buf,
        }
    }

    #[test]
    fn clusters_vertical_japanese_bubble_right_to_left_without_merging_adjacent_bubble() {
        // Review Focus #3:
        // Bubble A (right side, x=400..470): 2 vertical columns at x=440 ("危ない") and x=405 ("逃げろ！")
        // Bubble B (left side, x=80..150): 2 vertical columns at x=120 ("大丈夫") and x=85 ("か？")
        let lines = vec![
            make_line(85, 65, 28, 80, "か？", 0.94, true),
            make_line(405, 55, 28, 120, "逃げろ！", 0.93, true),
            make_line(120, 60, 28, 100, "大丈夫", 0.96, true),
            make_line(440, 50, 28, 110, "危ない", 0.95, true),
        ];

        let blocks = cluster_ocr_lines(&lines, ReadingDirection::RightToLeft);
        assert_eq!(blocks.len(), 2);

        // Bubble A first (right-most in RTL order), columns merged right-to-left with no stray spaces.
        assert_eq!(blocks[0].text, "危ない逃げろ！");
        assert!(blocks[0].is_vertical);
        assert_eq!(
            blocks[0].bbox,
            PhysicalRect {
                x: 405,
                y: 50,
                width: 63,
                height: 125,
            }
        );

        // Bubble B second.
        assert_eq!(blocks[1].text, "大丈夫か？");
        assert!(blocks[1].is_vertical);
        assert_eq!(
            blocks[1].bbox,
            PhysicalRect {
                x: 85,
                y: 60,
                width: 63,
                height: 100,
            }
        );
    }

    #[test]
    fn clusters_vertical_split_column_top_to_bottom_and_orders_multi_row_manga_pages() {
        // Upper row: Bubble A (right) with a split column + Bubble B (left).
        // Lower row: Bubble C (right) + Bubble D (left).
        let lines = vec![
            // Upper-right split into top/bottom fragments on the same column + left column.
            make_line(441, 106, 28, 45, "ない", 0.92, true),
            make_line(440, 50, 28, 50, "危", 0.95, true),
            make_line(405, 52, 28, 100, "逃げろ", 0.94, true),
            // Upper-left
            make_line(90, 58, 28, 95, "了解", 0.95, true),
            // Lower-right
            make_line(420, 320, 28, 100, "下右", 0.95, true),
            // Lower-left
            make_line(100, 325, 28, 100, "下左", 0.95, true),
        ];

        let blocks = cluster_ocr_lines(&lines, ReadingDirection::Auto);
        let texts: Vec<&str> = blocks.iter().map(|b| b.text.as_str()).collect();
        assert_eq!(texts, vec!["危ない逃げろ", "了解", "下右", "下左"]);
    }

    #[test]
    fn clusters_horizontal_comic_bubble_top_to_bottom_in_ltr_order() {
        let lines = vec![
            make_line(320, 45, 70, 20, "Run!", 0.95, false),
            make_line(52, 66, 70, 20, "out!", 0.92, false),
            make_line(50, 40, 80, 20, "Watch", 0.96, false),
        ];

        let blocks = cluster_ocr_lines(&lines, ReadingDirection::LeftToRight);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Watch out!");
        assert!(!blocks[0].is_vertical);
        assert_eq!(blocks[1].text, "Run!");
        assert!(!blocks[1].is_vertical);
    }

    #[test]
    fn horizontal_joining_omits_space_between_cjk_and_keeps_space_for_korean_and_latin() {
        let cjk_horizontal = vec![
            make_line(50, 40, 90, 22, "你好", 0.95, false),
            make_line(50, 68, 90, 22, "世界！", 0.95, false),
        ];
        let cjk_blocks = cluster_ocr_lines(&cjk_horizontal, ReadingDirection::LeftToRight);
        assert_eq!(cjk_blocks.len(), 1);
        assert_eq!(cjk_blocks[0].text, "你好世界！");

        let korean_horizontal = vec![
            make_line(50, 40, 90, 22, "안녕", 0.95, false),
            make_line(50, 68, 90, 22, "하세요", 0.95, false),
        ];
        let ko_blocks = cluster_ocr_lines(&korean_horizontal, ReadingDirection::LeftToRight);
        assert_eq!(ko_blocks.len(), 1);
        assert_eq!(ko_blocks[0].text, "안녕 하세요");

        let vertical_ascii = vec![
            make_line(200, 40, 24, 60, "GO", 0.95, true),
            make_line(170, 40, 24, 60, "NOW", 0.95, true),
        ];
        let ascii_blocks = cluster_ocr_lines(&vertical_ascii, ReadingDirection::RightToLeft);
        assert_eq!(ascii_blocks.len(), 1);
        assert_eq!(ascii_blocks[0].text, "GO NOW");
    }

    #[test]
    fn filters_empty_and_low_confidence_lines_and_auto_infers_reading_direction() {
        // Majority vertical -> Auto infers RightToLeft; low confidence and whitespace dropped.
        let vertical_lines = vec![
            make_line(440, 50, 28, 100, "右", 0.90, true),
            make_line(100, 52, 28, 100, "左", 0.90, true),
            make_line(260, 50, 28, 100, "ノイズ", 0.10, true),
            make_line(260, 50, 28, 100, "   \t ", 0.99, true),
            make_line(260, 50, 0, 100, "空矩形", 0.99, true),
            make_line(260, 50, 28, 100, "NaN", f32::NAN, true),
        ];
        let v_blocks = cluster_ocr_lines(&vertical_lines, ReadingDirection::Auto);
        assert_eq!(v_blocks.len(), 2);
        assert_eq!(v_blocks[0].text, "右");
        assert_eq!(v_blocks[1].text, "左");

        // Majority horizontal -> Auto infers LeftToRight.
        let horizontal_lines = vec![
            make_line(320, 40, 80, 20, "Second", 0.90, false),
            make_line(40, 42, 80, 20, "First", 0.90, false),
            make_line(180, 40, 80, 20, "bad", 0.10, false),
        ];
        let h_blocks = cluster_ocr_lines(&horizontal_lines, ReadingDirection::Auto);
        assert_eq!(h_blocks.len(), 2);
        assert_eq!(h_blocks[0].text, "First");
        assert_eq!(h_blocks[1].text, "Second");

        // Empty input yields empty output cleanly.
        assert!(cluster_ocr_lines(&[], ReadingDirection::Auto).is_empty());
    }

    #[test]
    fn custom_cluster_config_controls_merge_distance_and_confidence() {
        let lines = vec![
            make_line(50, 40, 80, 20, "Top", 0.50, false),
            make_line(50, 75, 80, 20, "Bottom", 0.50, false),
        ];
        // Vertical gap = 15, height = 20 -> 15 <= 0.9*20 (18) merges by default.
        assert_eq!(
            cluster_ocr_lines(&lines, ReadingDirection::LeftToRight).len(),
            1
        );

        // Tighter vertical gap factor (0.5 -> 10px) keeps them separate.
        let tight = ClusterConfig {
            vertical_gap_factor: 0.5,
            ..ClusterConfig::default()
        };
        assert_eq!(
            cluster_ocr_lines_with_config(&lines, ReadingDirection::LeftToRight, &tight).len(),
            2
        );

        // Raising min_confidence above 0.5 drops both.
        let strict = ClusterConfig {
            min_confidence: 0.8,
            ..ClusterConfig::default()
        };
        assert!(
            cluster_ocr_lines_with_config(&lines, ReadingDirection::LeftToRight, &strict)
                .is_empty()
        );
    }

    #[test]
    fn samples_adaptive_bubble_bg_selects_wcag_contrast_text_and_fits_long_vietnamese() {
        // Review Focus #4:
        // White frame border around bbox -> bg near [255,255,255,255] -> dark text [17,24,39,255]
        let mut white_frame = solid_frame(120, 120, [255, 255, 255, 255]);
        // Put dark ink INSIDE bbox to verify perimeter sampling ignores interior ink.
        for y in 30..70u32 {
            for x in 30..70u32 {
                let idx = ((y * 120 + x) * 4) as usize;
                white_frame.rgba[idx..idx + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        let bbox = PhysicalRect {
            x: 30,
            y: 30,
            width: 40,
            height: 40,
        };
        let white_bg = sample_bubble_background(&white_frame, bbox);
        assert_eq!(white_bg, [255, 255, 255, 255]);
        assert_eq!(contrasting_text_color(white_bg), [17, 24, 39, 255]);

        // Dark frame border around bbox -> bg near [20,20,24,255] -> light text [248,250,252,255]
        let dark_frame = solid_frame(120, 120, [20, 20, 24, 255]);
        let dark_bg = sample_bubble_background(&dark_frame, bbox);
        assert_eq!(dark_bg, [20, 20, 24, 255]);
        assert_eq!(contrasting_text_color(dark_bg), [248, 250, 252, 255]);

        // Noisy cream border with minority black ink spikes still yields median cream backdrop.
        let mut cream_frame = solid_frame(120, 120, [242, 236, 224, 255]);
        for x in 28..42u32 {
            let idx = ((29 * 120 + x) * 4) as usize;
            cream_frame.rgba[idx..idx + 4].copy_from_slice(&[0, 0, 0, 255]);
        }
        let cream_bg = sample_bubble_background(&cream_frame, bbox);
        assert_eq!(cream_bg, [242, 236, 224, 255]);
        assert_eq!(contrasting_text_color(cream_bg), [17, 24, 39, 255]);

        // Bbox flush against frame edges clamps cleanly without panicking.
        let edge_bbox = PhysicalRect {
            x: 0,
            y: 0,
            width: 120,
            height: 120,
        };
        assert_eq!(
            sample_bubble_background(&dark_frame, edge_bbox),
            [20, 20, 24, 255]
        );

        // Invalid frame falls back to white safely.
        let invalid_frame = CaptureFrame {
            width: 0,
            height: 0,
            scale_factor: 1.0,
            rgba: Vec::new(),
        };
        assert_eq!(
            sample_bubble_background(&invalid_frame, bbox),
            [255, 255, 255, 255]
        );

        // Long 12-word Vietnamese sentence in narrow 70x140 logical box with base=16.0, min=9.0
        let vietnamese = "Nguy hiểm quá, mau chạy đi đừng có đứng yên ở đó!";
        let fitted = compute_fitted_font_size(vietnamese, 70.0, 140.0, 16.0, 9.0);
        assert!(fitted.is_finite());
        assert!(fitted >= 9.0, "expected fitted >= 9.0, got {fitted}");
        assert!(fitted < 16.0, "expected fitted < 16.0, got {fitted}");

        // Short text stays at base_font_px; extreme overflow clamps cleanly to min_font_px.
        assert_eq!(
            compute_fitted_font_size("Chạy!", 70.0, 140.0, 16.0, 9.0),
            16.0
        );
        let huge = "Đoạn hội thoại tiếng Việt cực kỳ dài lặp đi lặp lại ".repeat(8);
        assert_eq!(compute_fitted_font_size(&huge, 70.0, 140.0, 16.0, 9.0), 9.0);

        // Zero / negative / NaN / Inf dimensions safely return min_font_px (no NaN/Inf).
        assert_eq!(
            compute_fitted_font_size(vietnamese, 0.0, 140.0, 16.0, 9.0),
            9.0
        );
        assert_eq!(
            compute_fitted_font_size(vietnamese, 70.0, -5.0, 16.0, 9.0),
            9.0
        );
        assert_eq!(
            compute_fitted_font_size(vietnamese, f32::NAN, 140.0, 16.0, 9.0),
            9.0
        );
        assert_eq!(
            compute_fitted_font_size(vietnamese, f32::INFINITY, 140.0, 16.0, 9.0),
            9.0
        );
        // Non-finite font bounds also return a finite positive value.
        let safe_bounds = compute_fitted_font_size(vietnamese, 70.0, 140.0, f32::NAN, -2.0);
        assert!(safe_bounds.is_finite() && safe_bounds >= 1.0);
    }
}
