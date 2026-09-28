//! Software rasterizer over a wl_shm XRGB8888 buffer: solid rects and 1-bit glyphs.

use crate::font::{CELL_H, CELL_W, Face};

pub struct Canvas<'a> {
    px: &'a mut [u32],
    w: usize,
    h: usize,
}

impl<'a> Canvas<'a> {
    pub fn new(bytes: &'a mut [u8], w: usize, h: usize) -> Self {
        // SAFETY: shm buffers are mmap'd, so always 4-byte aligned; u32 has no invalid bit patterns.
        let (pre, px, _) = unsafe { bytes.align_to_mut::<u32>() };
        debug_assert!(pre.is_empty());
        Self {
            px: &mut px[..w * h],
            w,
            h,
        }
    }

    pub fn fill(&mut self, color: u32) {
        self.px.fill(color);
    }

    pub fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x1 = (x + w).min(self.w);
        for row in y..(y + h).min(self.h) {
            self.px[row * self.w + x.min(x1)..row * self.w + x1].fill(color);
        }
    }

    /// Draws a premultiplied-ARGB image with its top-left at (x, y), blending over what's
    /// there. Clips at the canvas edge.
    pub fn blit(&mut self, x: usize, y: usize, w: usize, h: usize, px: &[u32]) {
        for row in 0..h.min(self.h.saturating_sub(y)) {
            for col in 0..w.min(self.w.saturating_sub(x)) {
                let s = px[row * w + col];
                let a = s >> 24;
                let d = &mut self.px[(y + row) * self.w + x + col];
                if a == 255 {
                    *d = s & 0xff_ffff;
                } else if a > 0 {
                    // Premultiplied "over": src + dst * (1 - src alpha).
                    let (dv, keep) = (*d, 255 - a);
                    let ch = |shift: u32| {
                        let (sc, dc) = ((s >> shift) & 0xff, (dv >> shift) & 0xff);
                        (sc + (dc * keep + 127) / 255).min(255) << shift
                    };
                    *d = ch(16) | ch(8) | ch(0);
                }
            }
        }
    }

    /// Draws a premultiplied-ARGB image (`pw` x `ph`) stretched over the screen rect
    /// (sx, sy, sw, sh), which may extend past the canvas; nearest-neighbor, over black.
    pub fn stretch(&mut self, px: &[u32], pw: usize, ph: usize, rect: (f64, f64, f64, f64)) {
        let (sx, sy, sw, sh) = rect;
        if sw <= 0.0 || sh <= 0.0 {
            return;
        }
        let x0 = sx.max(0.0).floor() as usize;
        let x1 = ((sx + sw).min(self.w as f64)).ceil().max(0.0) as usize;
        let y0 = sy.max(0.0).floor() as usize;
        let y1 = ((sy + sh).min(self.h as f64)).ceil().max(0.0) as usize;
        // Source column for each screen column, sampled at pixel centers.
        let cols: Vec<Option<usize>> = (x0..x1)
            .map(|x| {
                let t = ((x as f64 + 0.5 - sx) / sw * pw as f64).floor();
                (t >= 0.0 && t < pw as f64).then_some(t as usize)
            })
            .collect();
        for y in y0..y1 {
            let t = ((y as f64 + 0.5 - sy) / sh * ph as f64).floor();
            if t < 0.0 || t >= ph as f64 {
                continue;
            }
            let src = &px[t as usize * pw..][..pw];
            let dst = &mut self.px[y * self.w..][..self.w];
            for (x, col) in (x0..x1).zip(&cols) {
                if let Some(c) = col {
                    // Premultiplied over black is the color itself.
                    dst[x] = src[*c] & 0xff_ffff;
                }
            }
        }
    }

    /// Copies opaque XRGB rows (`stride` pixels apart in `src`) to (x, y), clipped.
    pub fn copy(&mut self, x: usize, y: usize, w: usize, h: usize, src: &[u32], stride: usize) {
        let w = w.min(self.w.saturating_sub(x));
        for row in 0..h.min(self.h.saturating_sub(y)) {
            let d = &mut self.px[(y + row) * self.w + x..][..w];
            for (d, s) in d.iter_mut().zip(&src[row * stride..][..w]) {
                *d = s & 0xff_ffff;
            }
        }
    }

    /// Halves every pixel's brightness.
    pub fn dim(&mut self) {
        for p in self.px.iter_mut() {
            *p = (*p >> 1) & 0x7f_7f7f;
        }
    }

    /// 1px outline just inside the rect.
    pub fn frame(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.rect(x, y, w, 1, color);
        self.rect(x, y + h - 1, w, 1, color);
        self.rect(x, y, 1, h, color);
        self.rect(x + w - 1, y, 1, h, color);
    }

    /// Draws `s` with its cell's top-left at (x, y), dropping glyphs that would cross
    /// `max_x` or the canvas edge. Returns the x just past the last glyph.
    pub fn text(
        &mut self,
        mut x: usize,
        y: usize,
        s: &str,
        face: &Face,
        color: u32,
        max_x: usize,
    ) -> usize {
        let max_x = max_x.min(self.w);
        let rows = CELL_H.min(self.h.saturating_sub(y));
        for c in s.chars() {
            if x + CELL_W > max_x {
                break;
            }
            let glyph = face.glyph(c);
            for (r, &bits) in glyph[..rows].iter().enumerate() {
                if bits == 0 {
                    continue;
                }
                let line = &mut self.px[(y + r) * self.w + x..][..CELL_W];
                for (b, p) in line.iter_mut().enumerate() {
                    if bits & (0x80 >> b) != 0 {
                        *p = color;
                    }
                }
            }
            x += CELL_W;
        }
        x
    }
}
