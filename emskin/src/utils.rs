use cosmic_text::{Buffer as CtBuffer, Color as CtColor, FontSystem, SwashCache};
use smithay::utils::{Coordinate, Size};

pub(crate) trait SizeExt<N: Coordinate, Kind> {
    fn at_least(self, min: impl Into<Size<N, Kind>>) -> Size<N, Kind>;
}

impl<N: Coordinate, Kind> SizeExt<N, Kind> for Size<N, Kind> {
    fn at_least(self, min: impl Into<Size<N, Kind>>) -> Size<N, Kind> {
        let min = min.into();
        (self.w.max(min.w), self.h.max(min.h)).into()
    }
}

/// Alpha-blend cosmic-text glyphs onto a BGRA pixel buffer (Fourcc::Argb8888
/// on little-endian).  `fg` is BGRA-ordered.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text_onto(
    data: &mut [u8],
    buf_w: i32,
    buf_h: i32,
    offset_x: i32,
    offset_y: i32,
    fg: &[u8; 4],
    ct_buffer: &mut CtBuffer,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) {
    let stride = buf_w * 4;
    let ct_color = CtColor::rgba(fg[2], fg[1], fg[0], fg[3]);
    ct_buffer.draw(
        font_system,
        swash_cache,
        ct_color,
        |gx, gy, gw, gh, color| {
            let a = color.a() as u32;
            if a == 0 {
                return;
            }
            let (pr, pg, pb) = (color.r() as u32, color.g() as u32, color.b() as u32);
            for dy in 0..gh as i32 {
                for dx in 0..gw as i32 {
                    let x = gx + dx + offset_x;
                    let y = gy + dy + offset_y;
                    if x < 0 || x >= buf_w || y < 0 || y >= buf_h {
                        continue;
                    }
                    let off = (y * stride + x * 4) as usize;
                    if off + 3 >= data.len() {
                        continue;
                    }
                    let inv = 255 - a;
                    data[off] = ((data[off] as u32 * inv + pb * a) / 255) as u8;
                    data[off + 1] = ((data[off + 1] as u32 * inv + pg * a) / 255) as u8;
                    data[off + 2] = ((data[off + 2] as u32 * inv + pr * a) / 255) as u8;
                    data[off + 3] =
                        Ord::min((data[off + 3] as u32 * inv + 255 * a) / 255, 255) as u8;
                }
            }
        },
    );
}
