//! Embedded 5×7 pixel font used by overlays that need tiny readable
//! labels without dragging in cosmic-text. Column-major encoding, bit 0
//! is the top row of each column.
//!
//! The glyph set covers digits plus a few punctuation marks — enough for
//! coordinate readouts (`measure`: `"123, 456"`) and MM:SS timers
//! (`recorder`: `"01:23"`). Add characters here rather than in per-plugin
//! copies so new overlays pick them up for free.

use smithay::utils::{Buffer, Point, Size};

pub const GLYPH_W: i32 = 5;
pub const GLYPH_H: i32 = 7;
pub const GLYPH_SPACING: i32 = 1;

/// Column bitmap for `ch`, or `None` when the character isn't in the
/// embedded set. Blitters should silently skip unknown characters so new
/// glyphs can be added without touching every call site.
pub fn glyph(ch: char) -> Option<[u8; 5]> {
    Some(match ch {
        '0' => [0x3E, 0x51, 0x49, 0x45, 0x3E],
        '1' => [0x00, 0x42, 0x7F, 0x40, 0x00],
        '2' => [0x42, 0x61, 0x51, 0x49, 0x46],
        '3' => [0x21, 0x41, 0x45, 0x4B, 0x31],
        '4' => [0x18, 0x14, 0x12, 0x7F, 0x10],
        '5' => [0x27, 0x45, 0x45, 0x45, 0x39],
        '6' => [0x3C, 0x4A, 0x49, 0x49, 0x30],
        '7' => [0x01, 0x71, 0x09, 0x05, 0x03],
        '8' => [0x36, 0x49, 0x49, 0x49, 0x36],
        '9' => [0x06, 0x49, 0x49, 0x29, 0x1E],
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00],
        ',' => [0x00, 0x50, 0x30, 0x00, 0x00],
        ':' => [0x00, 0x00, 0x36, 0x00, 0x00],
        '(' => [0x00, 0x1C, 0x22, 0x41, 0x00],
        ')' => [0x00, 0x41, 0x22, 0x1C, 0x00],
        '-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        _ => return None,
    })
}

/// Pixel width of `text` in the bitmap font, excluding trailing spacing.
pub fn label_width(text: &str) -> i32 {
    let n = text.chars().count() as i32;
    if n == 0 {
        0
    } else {
        n * (GLYPH_W + GLYPH_SPACING) - GLYPH_SPACING
    }
}

/// Blit `text` into a BGRA `data` buffer. `pos` is the top-left anchor
/// of the first glyph in buffer coordinates. Clipping against `buf_size`
/// is automatic.
pub fn draw_text(
    data: &mut [u8],
    buf_size: Size<i32, Buffer>,
    pos: Point<i32, Buffer>,
    text: &str,
    color: &[u8; 4],
) {
    let stride = buf_size.w * 4;
    let mut cursor_x = pos.x;
    for ch in text.chars() {
        if let Some(cols) = glyph(ch) {
            for (gc, &col_bits) in cols.iter().enumerate() {
                for gr in 0..GLYPH_H {
                    if col_bits & (1 << gr) == 0 {
                        continue;
                    }
                    let px = cursor_x + gc as i32;
                    let py = pos.y + gr;
                    if px >= 0 && px < buf_size.w && py >= 0 && py < buf_size.h {
                        let off = (py * stride + px * 4) as usize;
                        data[off..off + 4].copy_from_slice(color);
                    }
                }
            }
        }
        cursor_x += GLYPH_W + GLYPH_SPACING;
    }
}
