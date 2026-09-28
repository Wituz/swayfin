//! Tamzen 8x16, baked in at compile time by build.rs.

include!(concat!(env!("OUT_DIR"), "/font_gen.rs"));

pub struct Face {
    codepoints: &'static [u32],
    bitmaps: &'static [[u8; CELL_H]],
}

impl Face {
    /// Glyph rows for `c` (MSB = leftmost pixel). Falls back to the font's default char.
    #[inline]
    pub fn glyph(&self, c: char) -> &'static [u8; CELL_H] {
        let i = self.codepoints.binary_search(&(c as u32)).unwrap_or(0);
        &self.bitmaps[i]
    }
}
