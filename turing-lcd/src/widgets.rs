// SPDX-License-Identifier: GPL-3.0-or-later
//! Small reusable drawing helpers built on top of [`Canvas`].

use crate::canvas::{Canvas, Rect, Rgb};

/// Horizontal bar filled left-to-right by `fraction` (clamped to 0..=1).
pub fn progress_bar(
    canvas: &mut Canvas,
    r: Rect,
    fraction: f32,
    fill: Rgb,
    background: Rgb,
    outline: Option<Rgb>,
) {
    if r.is_empty() {
        return;
    }
    canvas.fill_rect(r, background);

    let inner = match outline {
        Some(c) if r.w > 2 && r.h > 2 => {
            canvas.stroke_rect(r, c);
            Rect::new(r.x + 1, r.y + 1, r.w - 2, r.h - 2)
        }
        _ => r,
    };

    let f = fraction.clamp(0.0, 1.0);
    let filled = (inner.w as f32 * f).round() as u16;
    if filled > 0 {
        canvas.fill_rect(Rect::new(inner.x, inner.y, filled, inner.h), fill);
    }
}

/// Interpolate between two colours; `t` is clamped to 0..=1.
pub fn lerp_color(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mut out = [0u8; 3];
    for i in 0..3 {
        out[i] = (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
    }
    out
}

/// Filled history plot. `values` are 0..=1, oldest first; the newest sample
/// sits at the right edge. Fewer samples than columns leaves the left blank.
pub fn sparkline(
    canvas: &mut Canvas,
    r: Rect,
    values: &[f32],
    line: Rgb,
    area: Rgb,
    background: Rgb,
) {
    if r.is_empty() {
        return;
    }
    canvas.fill_rect(r, background);
    if values.is_empty() || r.h < 2 {
        return;
    }

    // One column per pixel, right-aligned so the newest sample is rightmost.
    let cols = r.w as usize;
    let start = values.len().saturating_sub(cols);
    let shown = &values[start..];
    let offset = cols - shown.len();

    let mut prev_top: Option<u16> = None;
    for (i, v) in shown.iter().enumerate() {
        let x = r.x + (offset + i) as u16;
        let hgt = (v.clamp(0.0, 1.0) * (r.h - 1) as f32).round() as u16;
        let top = r.bottom() - 1 - hgt;

        if hgt > 0 {
            canvas.fill_rect(Rect::new(x, top, 1, hgt), area);
        }
        canvas.fill_rect(Rect::new(x, top, 1, 1), line);

        // Bridge vertical gaps so a steep change stays a continuous line.
        if let Some(p) = prev_top {
            let (lo, hi) = if p < top { (p, top) } else { (top, p) };
            if hi - lo > 1 {
                canvas.fill_rect(Rect::new(x, lo, 1, hi - lo), line);
            }
        }
        prev_top = Some(top);
    }
}
