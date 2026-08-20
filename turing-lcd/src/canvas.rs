// SPDX-License-Identifier: GPL-3.0-or-later
//! CPU framebuffer in RGB888 plus the drawing primitives the display needs.
//!
//! The Python original leans on PIL: every widget allocates a fresh `Image`,
//! draws into it and throws it away. Here there is exactly one buffer for the
//! whole screen, reused forever, and drawing writes straight into it.

pub type Rgb = [u8; 3];

/// Half-open rectangle: covers `x..x + w`, `y..y + h`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Rect { x, y, w, h }
    }

    pub const fn right(&self) -> u16 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> u16 {
        self.y + self.h
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Clip against a `w * h` surface anchored at the origin.
    pub fn clip(&self, w: u16, h: u16) -> Rect {
        let x = self.x.min(w);
        let y = self.y.min(h);
        Rect {
            x,
            y,
            w: self.right().min(w).saturating_sub(x),
            h: self.bottom().min(h).saturating_sub(y),
        }
    }
}

pub struct Canvas {
    w: u16,
    h: u16,
    px: Vec<u8>,
}

impl Canvas {
    pub fn new(w: u16, h: u16, fill: Rgb) -> Self {
        let mut c = Canvas {
            w,
            h,
            px: vec![0; w as usize * h as usize * 3],
        };
        c.clear(fill);
        c
    }

    pub const fn width(&self) -> u16 {
        self.w
    }

    pub const fn height(&self) -> u16 {
        self.h
    }

    /// Raw RGB888 bytes, row-major.
    pub fn bytes(&self) -> &[u8] {
        &self.px
    }

    fn row_range(&self, y: u16, x0: u16, x1: u16) -> std::ops::Range<usize> {
        let base = (y as usize * self.w as usize + x0 as usize) * 3;
        base..base + (x1 - x0) as usize * 3
    }

    pub fn clear(&mut self, color: Rgb) {
        for p in self.px.chunks_exact_mut(3) {
            p.copy_from_slice(&color);
        }
    }

    pub fn fill_rect(&mut self, r: Rect, color: Rgb) {
        let r = r.clip(self.w, self.h);
        if r.is_empty() {
            return;
        }
        // Paint the first row, then memcpy it down the rectangle: much cheaper
        // than a per-pixel write once the rectangle is more than a few rows.
        let first = self.row_range(r.y, r.x, r.right());
        for p in self.px[first.clone()].chunks_exact_mut(3) {
            p.copy_from_slice(&color);
        }
        let stride = self.w as usize * 3;
        let len = first.len();
        for y in (r.y + 1)..r.bottom() {
            let dst = first.start + (y - r.y) as usize * stride;
            self.px.copy_within(first.clone(), dst);
            debug_assert_eq!(len, r.w as usize * 3);
        }
    }

    /// Blend `color` over the pixel at (x, y) with 8-bit coverage.
    #[inline]
    pub fn blend_pixel(&mut self, x: u16, y: u16, color: Rgb, alpha: u8) {
        if x >= self.w || y >= self.h || alpha == 0 {
            return;
        }
        let i = (y as usize * self.w as usize + x as usize) * 3;
        if alpha == 255 {
            self.px[i..i + 3].copy_from_slice(&color);
            return;
        }
        let a = alpha as u32;
        let inv = 255 - a;
        for c in 0..3 {
            let dst = self.px[i + c] as u32;
            // +127 rounds to nearest instead of truncating, which keeps thin
            // antialiased strokes from drifting darker every redraw.
            self.px[i + c] = ((color[c] as u32 * a + dst * inv + 127) / 255) as u8;
        }
    }

    /// Blit an 8-bit coverage mask (as produced by the glyph rasterizer).
    pub fn blit_mask(&mut self, x: i32, y: i32, mw: usize, mh: usize, mask: &[u8], color: Rgb) {
        for my in 0..mh {
            let py = y + my as i32;
            if py < 0 || py >= self.h as i32 {
                continue;
            }
            for mx in 0..mw {
                let px = x + mx as i32;
                if px < 0 || px >= self.w as i32 {
                    continue;
                }
                self.blend_pixel(px as u16, py as u16, color, mask[my * mw + mx]);
            }
        }
    }

    /// Copy a sub-rectangle out as RGB565 little-endian, the wire format the
    /// panel expects. Writes into `out` so the caller can reuse one buffer.
    pub fn encode_rgb565(&self, r: Rect, out: &mut Vec<u8>) {
        let r = r.clip(self.w, self.h);
        out.clear();
        out.reserve(r.w as usize * r.h as usize * 2);
        for y in r.y..r.bottom() {
            let row = &self.px[self.row_range(y, r.x, r.right())];
            for p in row.chunks_exact(3) {
                let v = ((p[0] as u16 & 0xf8) << 8)
                    | ((p[1] as u16 & 0xfc) << 3)
                    | (p[2] as u16 >> 3);
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
    }

    /// Row slice used by the diffing code in `display`.
    pub(crate) fn row(&self, y: u16) -> &[u8] {
        let stride = self.w as usize * 3;
        let base = y as usize * stride;
        &self.px[base..base + stride]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb565_encoding_matches_reference() {
        let mut c = Canvas::new(2, 1, [0, 0, 0]);
        c.fill_rect(Rect::new(0, 0, 1, 1), [255, 0, 0]);
        c.fill_rect(Rect::new(1, 0, 1, 1), [0, 255, 0]);
        let mut out = Vec::new();
        c.encode_rgb565(Rect::new(0, 0, 2, 1), &mut out);
        // 0xf800 red and 0x07e0 green, little-endian.
        assert_eq!(out, vec![0x00, 0xf8, 0xe0, 0x07]);
    }

    #[test]
    fn fill_rect_clips_to_canvas() {
        let mut c = Canvas::new(4, 4, [0, 0, 0]);
        // Overflows on both axes; must not panic and must not wrap around.
        c.fill_rect(Rect::new(2, 2, 10, 10), [255, 255, 255]);
        assert_eq!(&c.row(0)[0..3], &[0, 0, 0]);
        assert_eq!(&c.row(3)[6..9], &[255, 255, 255]);
        assert_eq!(&c.row(3)[3..6], &[0, 0, 0]);
    }

    #[test]
    fn fill_rect_covers_every_row() {
        let mut c = Canvas::new(3, 3, [0, 0, 0]);
        c.fill_rect(Rect::new(0, 0, 3, 3), [1, 2, 3]);
        for y in 0..3 {
            assert!(c.row(y).chunks_exact(3).all(|p| p == [1, 2, 3]));
        }
    }

    #[test]
    fn blend_pixel_endpoints_are_exact() {
        let mut c = Canvas::new(1, 1, [0, 0, 0]);
        c.blend_pixel(0, 0, [200, 100, 50], 0);
        assert_eq!(&c.bytes()[0..3], &[0, 0, 0]);
        c.blend_pixel(0, 0, [200, 100, 50], 255);
        assert_eq!(&c.bytes()[0..3], &[200, 100, 50]);
    }

    #[test]
    fn rect_clip_never_underflows() {
        let r = Rect::new(50, 60, 10, 10).clip(20, 20);
        assert!(r.is_empty());
    }
}
