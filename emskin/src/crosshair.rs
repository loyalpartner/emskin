use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                Id, Kind,
            },
            gles::GlesRenderer,
            utils::CommitCounter,
        },
    },
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

// ---------------------------------------------------------------------------
// Embedded 5×7 bitmap font (column-major, bit 0 = top row)
// ---------------------------------------------------------------------------

const GLYPH_W: i32 = 5;
const GLYPH_H: i32 = 7;
const GLYPH_SPACING: i32 = 1;
const LABEL_PAD: i32 = 3;

/// Returns the 5-column bitmap for a character, or None if unsupported.
fn glyph(ch: char) -> Option<[u8; 5]> {
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
        '(' => [0x00, 0x1C, 0x22, 0x41, 0x00],
        ')' => [0x00, 0x41, 0x22, 0x1C, 0x00],
        '-' => [0x08, 0x08, 0x08, 0x08, 0x08],
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Crosshair overlay
// ---------------------------------------------------------------------------

/// Line color: semi-transparent pink/magenta (Figma-style).
const LINE_COLOR: [f32; 4] = [1.0, 0.25, 0.5, 0.6];
/// Label background: near-black.
const BG_COLOR: [u8; 4] = [35, 25, 25, 220]; // BGRA
/// Label text: pink.
const FG_COLOR: [u8; 4] = [180, 100, 255, 255]; // BGRA

pub struct CrosshairOverlay {
    pub enabled: bool,
    h_line_id: Id,
    v_line_id: Id,
    h_line_commit: CommitCounter,
    v_line_commit: CommitCounter,
    label_buf: MemoryRenderBuffer,
    /// Last rendered cursor position (logical integer), for change detection.
    last_pos: Option<(i32, i32)>,
}

impl CrosshairOverlay {
    pub fn new() -> Self {
        Self {
            enabled: false,
            h_line_id: Id::new(),
            v_line_id: Id::new(),
            h_line_commit: CommitCounter::default(),
            v_line_commit: CommitCounter::default(),
            // Start with a 1×1 buffer; resized on first render.
            label_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            last_pos: None,
        }
    }

    /// Build render elements for the crosshair overlay.
    ///
    /// Returns (solid_elements, label_element).  The label needs the renderer
    /// for texture upload so it's returned separately.
    pub fn build_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        cursor_logical: Point<f64, Logical>,
        output_size_phys: Size<i32, Physical>,
        scale: f64,
    ) -> (
        Vec<SolidColorRenderElement>,
        Option<MemoryRenderBufferRenderElement<GlesRenderer>>,
    ) {
        if !self.enabled {
            return (Vec::new(), None);
        }

        let s: Scale<f64> = Scale::from(scale);
        let cursor_log: Point<i32, Logical> = cursor_logical.to_i32_round();
        let cursor_phys: Point<i32, Physical> = cursor_logical.to_physical(s).to_i32_round();
        let output_log: Size<i32, Logical> = output_size_phys.to_f64().to_logical(s).to_i32_round();

        // Increment commit counters only when position actually changes.
        let pos = (cursor_log.x, cursor_log.y);
        if self.last_pos != Some(pos) {
            self.last_pos = Some(pos);
            self.h_line_commit.increment();
            self.v_line_commit.increment();
            self.render_label(cursor_log.x, cursor_log.y);
        }

        let h_line = SolidColorRenderElement::new(
            self.h_line_id.clone(),
            Rectangle::new((0, cursor_phys.y).into(), (output_size_phys.w, 1).into()),
            self.h_line_commit,
            LINE_COLOR,
            Kind::Unspecified,
        );
        let v_line = SolidColorRenderElement::new(
            self.v_line_id.clone(),
            Rectangle::new((cursor_phys.x, 0).into(), (1, output_size_phys.h).into()),
            self.v_line_commit,
            LINE_COLOR,
            Kind::Unspecified,
        );

        // Flip label direction when cursor is near the output edge.
        let label_offset = 8;
        let label_size: Size<i32, Logical> = {
            let text = format!("{}, {}", cursor_log.x, cursor_log.y);
            let cc = text.chars().count() as i32;
            (
                LABEL_PAD * 2 + cc * (GLYPH_W + GLYPH_SPACING) - GLYPH_SPACING,
                LABEL_PAD * 2 + GLYPH_H,
            )
                .into()
        };
        let fits_right = cursor_log.x + label_offset + label_size.w <= output_log.w;
        let fits_below = cursor_log.y + label_offset + label_size.h <= output_log.h;
        let lx = if fits_right {
            cursor_log.x + label_offset
        } else {
            cursor_log.x - label_offset - label_size.w
        };
        let ly = if fits_below {
            cursor_log.y + label_offset
        } else {
            cursor_log.y - label_offset - label_size.h
        };
        let label_loc = Point::<f64, Logical>::from((lx as f64, ly as f64)).to_physical(s);

        let label = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            label_loc,
            &self.label_buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok();

        (vec![h_line, v_line], label)
    }

    /// Render "(x, y)" into the label buffer.
    fn render_label(&mut self, x: i32, y: i32) {
        let text = format!("{x}, {y}");
        let char_count = text.chars().count() as i32;
        let buf_w = LABEL_PAD * 2 + char_count * (GLYPH_W + GLYPH_SPACING) - GLYPH_SPACING;
        let buf_h = LABEL_PAD * 2 + GLYPH_H;

        let mut ctx = self.label_buf.render();
        ctx.resize((buf_w, buf_h));

        ctx.draw(|data| {
            let stride = buf_w * 4;
            debug_assert_eq!(data.len(), (buf_w * buf_h * 4) as usize);
            // Fill background.
            data.chunks_exact_mut(4)
                .for_each(|chunk| chunk.copy_from_slice(&BG_COLOR));
            // Draw glyphs.
            let mut cursor_x = LABEL_PAD;
            for ch in text.chars() {
                if let Some(cols) = glyph(ch) {
                    for (gc, &col_bits) in cols.iter().enumerate() {
                        for gr in 0..GLYPH_H {
                            if col_bits & (1 << gr) != 0 {
                                let px = cursor_x + gc as i32;
                                let py = LABEL_PAD + gr;
                                let off = (py * stride + px * 4) as usize;
                                data[off..off + 4].copy_from_slice(&FG_COLOR);
                            }
                        }
                    }
                }
                cursor_x += GLYPH_W + GLYPH_SPACING;
            }
            Ok::<_, std::convert::Infallible>(vec![Rectangle::from_size(
                Size::<i32, Buffer>::from((buf_w, buf_h)),
            )])
        })
        .unwrap();
    }
}
