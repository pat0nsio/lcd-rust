// SPDX-License-Identifier: GPL-3.0-or-later
//! TrueType text rendering with a glyph cache.
//!
//! A system monitor redraws the same handful of digits every second, so every
//! rasterized glyph is kept and reused; steady-state rendering does no
//! rasterization at all.

use std::collections::HashMap;

use crate::canvas::{Canvas, Rgb};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VAlign {
    /// `y` is the top of the em box (PIL's "a" anchor).
    Top,
    /// `y` is the visual middle of the line.
    Middle,
    /// `y` is the baseline.
    Baseline,
}

struct Glyph {
    w: usize,
    h: usize,
    /// Offset from the pen position to the top-left of the mask.
    left: i32,
    top: i32,
    advance: f32,
    mask: Vec<u8>,
}

pub struct Font {
    inner: fontdue::Font,
    cache: HashMap<(char, u16), Glyph>,
}

impl Font {
    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let inner = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
            .map_err(|e| format!("cannot parse font: {e}"))?;
        Ok(Font {
            inner,
            cache: HashMap::new(),
        })
    }

    pub fn load(path: &str) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        Font::from_bytes(&data)
    }

    /// Distance from the top of the em box down to the baseline.
    pub fn ascent(&self, size: u16) -> f32 {
        self.inner
            .horizontal_line_metrics(size as f32)
            .map(|m| m.ascent)
            .unwrap_or(size as f32 * 0.8)
    }

    /// Natural distance between consecutive baselines.
    pub fn line_height(&self, size: u16) -> f32 {
        self.inner
            .horizontal_line_metrics(size as f32)
            .map(|m| m.new_line_size)
            .unwrap_or(size as f32 * 1.2)
    }

    fn glyph(&mut self, ch: char, size: u16) -> &Glyph {
        self.cache.entry((ch, size)).or_insert_with(|| {
            let (m, mask) = self.inner.rasterize(ch, size as f32);
            Glyph {
                w: m.width,
                h: m.height,
                left: m.xmin,
                // fontdue reports ymin as the offset of the mask's *bottom*
                // above the baseline, so the top sits height + ymin above it.
                top: -(m.height as i32 + m.ymin),
                advance: m.advance_width,
                mask,
            }
        })
    }

    /// Advance width of `text` in pixels, kerning included.
    pub fn measure(&mut self, text: &str, size: u16) -> f32 {
        let mut w = 0.0f32;
        let mut prev: Option<char> = None;
        for ch in text.chars() {
            if let Some(p) = prev {
                w += self.inner.horizontal_kern(p, ch, size as f32).unwrap_or(0.0);
            }
            w += self.glyph(ch, size).advance;
            prev = Some(ch);
        }
        w
    }

    /// Draw `text` and return the bounding box actually touched, so the caller
    /// knows what to invalidate.
    pub fn draw(
        &mut self,
        canvas: &mut Canvas,
        text: &str,
        x: i32,
        y: i32,
        size: u16,
        color: Rgb,
        align: Align,
        valign: VAlign,
    ) {
        if text.is_empty() {
            return;
        }
        let width = self.measure(text, size);
        let mut pen = match align {
            Align::Left => x as f32,
            Align::Center => x as f32 - width / 2.0,
            Align::Right => x as f32 - width,
        };
        let ascent = self.ascent(size);
        let baseline = match valign {
            VAlign::Top => y as f32 + ascent,
            VAlign::Middle => y as f32 + ascent / 2.0,
            VAlign::Baseline => y as f32,
        };

        let mut prev: Option<char> = None;
        for ch in text.chars() {
            if let Some(p) = prev {
                pen += self.inner.horizontal_kern(p, ch, size as f32).unwrap_or(0.0);
            }
            let g = self.glyph(ch, size);
            let (gw, gh, gl, gt, adv) = (g.w, g.h, g.left, g.top, g.advance);
            if gw > 0 && gh > 0 {
                let gx = pen.round() as i32 + gl;
                let gy = baseline.round() as i32 + gt;
                // Borrow ends here so the canvas can be mutated.
                let mask = &self.cache[&(ch, size)].mask;
                canvas.blit_mask(gx, gy, gw, gh, mask, color);
            }
            pen += adv;
            prev = Some(ch);
        }
    }
}
