// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas + device, with differential flushing.
//!
//! The reference implementation re-sends every widget's bitmap on every
//! refresh, whether or not the pixels changed. Here the canvas is diffed
//! against a shadow copy of what the panel already shows, and only the rows
//! that actually differ go over the 115200-baud link — which is the real
//! bottleneck, not CPU.

use crate::canvas::{Canvas, Rect, Rgb};
use crate::device::{Device, Orientation};

/// Marker for "this row has no differences yet".
const CLEAN: (u16, u16) = (u16::MAX, 0);

pub struct Display {
    dev: Device,
    canvas: Canvas,
    /// RGB888 copy of what the panel is currently showing.
    shadow: Vec<u8>,
    /// False until a full frame has been pushed, e.g. right after a clear.
    shadow_valid: bool,
    encoded: Vec<u8>,
}

/// What one `flush` actually did.
#[derive(Clone, Copy, Debug, Default)]
pub struct FlushStats {
    pub rects: usize,
    pub pixels: usize,
    pub bytes: usize,
}

impl Display {
    pub fn open(port: Option<&str>, orientation: Orientation) -> Result<Self, String> {
        let mut dev = Device::open(port)?;
        dev.set_orientation(orientation)?;
        let canvas = Canvas::new(dev.width(), dev.height(), [0, 0, 0]);
        let shadow = vec![0u8; canvas.bytes().len()];
        Ok(Display {
            dev,
            canvas,
            shadow,
            shadow_valid: false,
            encoded: Vec::new(),
        })
    }

    pub fn device(&self) -> &Device {
        &self.dev
    }

    pub fn device_mut(&mut self) -> &mut Device {
        &mut self.dev
    }

    pub fn canvas(&mut self) -> &mut Canvas {
        &mut self.canvas
    }

    pub const fn width(&self) -> u16 {
        self.canvas.width()
    }

    pub const fn height(&self) -> u16 {
        self.canvas.height()
    }

    /// Force the next flush to repaint everything.
    pub fn invalidate(&mut self) {
        self.shadow_valid = false;
    }

    pub fn clear(&mut self, color: Rgb) -> Result<(), String> {
        self.canvas.clear(color);
        self.invalidate();
        Ok(())
    }

    /// Send whatever changed since the last flush.
    pub fn flush(&mut self) -> Result<FlushStats, String> {
        let (w, h) = (self.canvas.width(), self.canvas.height());
        let mut stats = FlushStats::default();

        if !self.shadow_valid {
            self.push_rect(Rect::new(0, 0, w, h), &mut stats)?;
            self.shadow.copy_from_slice(self.canvas.bytes());
            self.shadow_valid = true;
            return Ok(stats);
        }

        let stride = w as usize * 3;
        let mut group: Option<(u16, u16, u16, u16)> = None; // y0, y1_excl, x0, x1_excl

        for y in 0..h {
            let span = self.dirty_span(y, stride);
            match (&mut group, span) {
                (None, s) if s != CLEAN => group = Some((y, y + 1, s.0, s.1)),
                (None, _) => {}
                (Some(g), s) if s == CLEAN => {
                    let g = *g;
                    self.push_rect(Rect::new(g.2, g.0, g.3 - g.2, g.1 - g.0), &mut stats)?;
                    group = None;
                }
                (Some(g), s) => {
                    let (ux0, ux1) = (g.2.min(s.0), g.3.max(s.1));
                    // Merge only when one wider rectangle is cheaper than two
                    // separate ones: each command costs 6 bytes of header and
                    // every pixel costs 2 bytes.
                    let merged = 2 * (ux1 - ux0) as usize * (g.1 - g.0 + 1) as usize + 6;
                    let split = 2 * (g.3 - g.2) as usize * (g.1 - g.0) as usize
                        + 6
                        + 2 * (s.1 - s.0) as usize
                        + 6;
                    if merged <= split {
                        g.1 = y + 1;
                        g.2 = ux0;
                        g.3 = ux1;
                    } else {
                        let old = *g;
                        self.push_rect(
                            Rect::new(old.2, old.0, old.3 - old.2, old.1 - old.0),
                            &mut stats,
                        )?;
                        group = Some((y, y + 1, s.0, s.1));
                    }
                }
            }
        }
        if let Some(g) = group {
            self.push_rect(Rect::new(g.2, g.0, g.3 - g.2, g.1 - g.0), &mut stats)?;
        }

        if stats.rects > 0 {
            self.shadow.copy_from_slice(self.canvas.bytes());
        }
        Ok(stats)
    }

    /// Pixel range of row `y` that differs from the shadow, or `CLEAN`.
    fn dirty_span(&self, y: u16, stride: usize) -> (u16, u16) {
        let base = y as usize * stride;
        let cur = self.canvas.row(y);
        let old = &self.shadow[base..base + stride];
        if cur == old {
            return CLEAN;
        }
        let first = cur
            .iter()
            .zip(old)
            .position(|(a, b)| a != b)
            .expect("rows differ");
        let last = cur
            .iter()
            .zip(old)
            .rposition(|(a, b)| a != b)
            .expect("rows differ");
        ((first / 3) as u16, (last / 3) as u16 + 1)
    }

    fn push_rect(&mut self, r: Rect, stats: &mut FlushStats) -> Result<(), String> {
        if r.is_empty() {
            return Ok(());
        }
        self.canvas.encode_rgb565(r, &mut self.encoded);
        self.dev
            .display_bitmap(r.x, r.y, r.w, r.h, &self.encoded)?;
        stats.rects += 1;
        stats.pixels += r.w as usize * r.h as usize;
        stats.bytes += self.encoded.len() + 6;
        Ok(())
    }
}
