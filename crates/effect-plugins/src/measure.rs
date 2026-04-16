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

use effect_core::paint_buffer;

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

/// Blit a string into a BGRA buffer using the 5×7 bitmap font.
///
/// `pos` is the top-left anchor of the first glyph in buffer coordinates.
fn draw_text(
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

// ---------------------------------------------------------------------------
// Measure overlay — Figma-style pixel inspector: crosshair lines, coordinate
// label at the cursor, and ruler strips on the top and left edges.
// ---------------------------------------------------------------------------

/// Line color: semi-transparent pink/magenta (Figma-style).
const LINE_COLOR: [f32; 4] = [1.0, 0.25, 0.5, 0.6];
/// Label background: near-black.
const BG_COLOR: [u8; 4] = [35, 25, 25, 220]; // BGRA
/// Label text: pink.
const FG_COLOR: [u8; 4] = [180, 100, 255, 255]; // BGRA

/// Ruler dimensions and tick spacing (logical pixels).
const RULER_SIZE: i32 = 22;
const MAJOR_TICK: i32 = 100;
const MID_TICK: i32 = 50;
const MINOR_TICK: i32 = 10;
const MAJOR_TICK_LEN: i32 = 10;
const MID_TICK_LEN: i32 = 6;
const MINOR_TICK_LEN: i32 = 3;
/// Padding between a tick mark and its numeric label.
const LABEL_GAP: i32 = 2;
/// Distance between the cursor and its floating coordinate label.
const CURSOR_LABEL_OFFSET: i32 = 8;

/// Tick length for a position along a ruler axis.
fn tick_length(pos: i32) -> i32 {
    if pos % MAJOR_TICK == 0 {
        MAJOR_TICK_LEN
    } else if pos % MID_TICK == 0 {
        MID_TICK_LEN
    } else {
        MINOR_TICK_LEN
    }
}

/// Pixel width a bitmap-font string occupies (excluding trailing spacing).
fn label_width(text: &str) -> i32 {
    let cc = text.chars().count() as i32;
    if cc == 0 {
        0
    } else {
        cc * (GLYPH_W + GLYPH_SPACING) - GLYPH_SPACING
    }
}

/// Render elements produced by [`MeasureOverlay::build_elements`].
///
/// Intended push order in the custom-element stack (front-to-back):
/// `cursor_label` → `lines` → `rulers`.
pub struct MeasureElements {
    pub lines: Vec<SolidColorRenderElement>,
    pub cursor_label: Option<MemoryRenderBufferRenderElement<GlesRenderer>>,
    pub rulers: Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
}

pub struct MeasureOverlay {
    pub enabled: bool,
    h_line_id: Id,
    v_line_id: Id,
    /// Shared commit counter for both crosshair lines — they always damage together.
    lines_commit: CommitCounter,
    label_buf: MemoryRenderBuffer,
    /// Cached size of the rendered cursor label, updated whenever `last_pos` changes.
    label_size: Size<i32, Buffer>,
    /// Last rendered cursor position (logical integer), for change detection.
    last_pos: Option<(i32, i32)>,
    top_ruler_buf: MemoryRenderBuffer,
    left_ruler_buf: MemoryRenderBuffer,
    /// Last output size used to build rulers; re-render on change.
    last_output_log: Option<(i32, i32)>,
}

impl Default for MeasureOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl MeasureOverlay {
    pub fn new() -> Self {
        Self {
            enabled: false,
            h_line_id: Id::new(),
            v_line_id: Id::new(),
            lines_commit: CommitCounter::default(),
            // Start with a 1×1 buffer; resized on first render.
            label_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            label_size: (0, 0).into(),
            top_ruler_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            left_ruler_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            last_pos: None,
            last_output_log: None,
        }
    }

    /// Build render elements for the measure overlay.
    pub fn build_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        cursor_logical: Point<f64, Logical>,
        output_size_log: Size<i32, Logical>,
        scale: f64,
    ) -> MeasureElements {
        if !self.enabled {
            return MeasureElements {
                lines: Vec::new(),
                cursor_label: None,
                rulers: Vec::new(),
            };
        }

        let s: Scale<f64> = Scale::from(scale);
        let cursor_log: Point<i32, Logical> = cursor_logical.to_i32_round();
        let cursor_phys: Point<i32, Physical> = cursor_logical.to_physical(s).to_i32_round();
        let output_phys: Size<i32, Physical> =
            output_size_log.to_f64().to_physical(s).to_i32_round();

        // Rebuild rulers only when output size changes.
        let size_key = (output_size_log.w, output_size_log.h);
        if self.last_output_log != Some(size_key) {
            self.last_output_log = Some(size_key);
            self.render_top_ruler(output_size_log.w);
            self.render_left_ruler(output_size_log.h);
        }

        // Rebuild the cursor label (and cache its size) only when the cursor moved.
        let pos = (cursor_log.x, cursor_log.y);
        if self.last_pos != Some(pos) {
            self.last_pos = Some(pos);
            self.lines_commit.increment();
            self.render_label(cursor_log.x, cursor_log.y);
        }

        let h_line = SolidColorRenderElement::new(
            self.h_line_id.clone(),
            Rectangle::new((0, cursor_phys.y).into(), (output_phys.w, 1).into()),
            self.lines_commit,
            LINE_COLOR,
            Kind::Unspecified,
        );
        let v_line = SolidColorRenderElement::new(
            self.v_line_id.clone(),
            Rectangle::new((cursor_phys.x, 0).into(), (1, output_phys.h).into()),
            self.lines_commit,
            LINE_COLOR,
            Kind::Unspecified,
        );

        // Flip label direction when cursor is near the output edge.
        let label_w = self.label_size.w;
        let label_h = self.label_size.h;
        let fits_right = cursor_log.x + CURSOR_LABEL_OFFSET + label_w <= output_size_log.w;
        let fits_below = cursor_log.y + CURSOR_LABEL_OFFSET + label_h <= output_size_log.h;
        let lx = if fits_right {
            cursor_log.x + CURSOR_LABEL_OFFSET
        } else {
            cursor_log.x - CURSOR_LABEL_OFFSET - label_w
        };
        let ly = if fits_below {
            cursor_log.y + CURSOR_LABEL_OFFSET
        } else {
            cursor_log.y - CURSOR_LABEL_OFFSET - label_h
        };
        let label_loc = Point::<f64, Logical>::from((lx as f64, ly as f64)).to_physical(s);

        let cursor_label = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            label_loc,
            &self.label_buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok();

        let origin_phys = Point::<f64, Logical>::from((0.0, 0.0)).to_physical(s);
        let mut rulers = Vec::with_capacity(2);
        if let Ok(r) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            origin_phys,
            &self.top_ruler_buf,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            rulers.push(r);
        }
        if let Ok(r) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            origin_phys,
            &self.left_ruler_buf,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            rulers.push(r);
        }

        MeasureElements {
            lines: vec![h_line, v_line],
            cursor_label,
            rulers,
        }
    }

    /// Render "(x, y)" into the label buffer and cache its buffer size.
    fn render_label(&mut self, x: i32, y: i32) {
        let text = format!("{x}, {y}");
        let buf_size: Size<i32, Buffer> =
            (LABEL_PAD * 2 + label_width(&text), LABEL_PAD * 2 + GLYPH_H).into();
        self.label_size = buf_size;

        paint_buffer(&mut self.label_buf, buf_size, |data| {
            debug_assert_eq!(data.len(), (buf_size.w * buf_size.h * 4) as usize);
            data.chunks_exact_mut(4)
                .for_each(|chunk| chunk.copy_from_slice(&BG_COLOR));
            draw_text(
                data,
                buf_size,
                (LABEL_PAD, LABEL_PAD).into(),
                &text,
                &FG_COLOR,
            );
        });
    }

    /// Render the horizontal (top) ruler for an output of the given width.
    fn render_top_ruler(&mut self, width: i32) {
        let buf_size: Size<i32, Buffer> = (width.max(1), RULER_SIZE).into();
        paint_buffer(&mut self.top_ruler_buf, buf_size, |data| {
            let stride = buf_size.w * 4;
            data.chunks_exact_mut(4)
                .for_each(|chunk| chunk.copy_from_slice(&BG_COLOR));

            // Border on the inner edge of the ruler.
            for xi in 0..buf_size.w {
                let off = ((buf_size.h - 1) * stride + xi * 4) as usize;
                data[off..off + 4].copy_from_slice(&FG_COLOR);
            }

            // Tick marks grow upward from the inner edge; labels sit near outer.
            for x in (0..buf_size.w).step_by(MINOR_TICK as usize) {
                let len = tick_length(x);
                for ty in (buf_size.h - 1 - len).max(0)..(buf_size.h - 1) {
                    let off = (ty * stride + x * 4) as usize;
                    data[off..off + 4].copy_from_slice(&FG_COLOR);
                }

                if x % MAJOR_TICK == 0 && x > 0 {
                    let text = format!("{x}");
                    let label_w = label_width(&text);
                    let pos = Point::<i32, Buffer>::from((x + LABEL_GAP, LABEL_GAP));
                    if pos.x + label_w < buf_size.w {
                        draw_text(data, buf_size, pos, &text, &FG_COLOR);
                    }
                }
            }
        });
    }

    /// Render the vertical (left) ruler for an output of the given height.
    fn render_left_ruler(&mut self, height: i32) {
        let buf_size: Size<i32, Buffer> = (RULER_SIZE, height.max(1)).into();
        paint_buffer(&mut self.left_ruler_buf, buf_size, |data| {
            let stride = buf_size.w * 4;
            data.chunks_exact_mut(4)
                .for_each(|chunk| chunk.copy_from_slice(&BG_COLOR));

            // Border on the inner edge of the ruler.
            for yi in 0..buf_size.h {
                let off = (yi * stride + (buf_size.w - 1) * 4) as usize;
                data[off..off + 4].copy_from_slice(&FG_COLOR);
            }

            // Tick marks grow leftward from the inner edge; labels sit near outer.
            for y in (0..buf_size.h).step_by(MINOR_TICK as usize) {
                let len = tick_length(y);
                for tx in (buf_size.w - 1 - len).max(0)..(buf_size.w - 1) {
                    let off = (y * stride + tx * 4) as usize;
                    data[off..off + 4].copy_from_slice(&FG_COLOR);
                }

                if y % MAJOR_TICK == 0 && y > 0 {
                    let text = format!("{y}");
                    let label_w = label_width(&text);
                    let pos = Point::<i32, Buffer>::from((LABEL_GAP, y + LABEL_GAP));
                    if pos.x + label_w < buf_size.w && pos.y + GLYPH_H < buf_size.h {
                        draw_text(data, buf_size, pos, &text, &FG_COLOR);
                    }
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl MeasureOverlay {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl effect_core::Effect for MeasureOverlay {
    fn name(&self) -> &'static str {
        "measure"
    }
    fn is_active(&self) -> bool {
        self.enabled
    }
    fn chain_position(&self) -> u8 {
        80
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        use effect_core::CustomElement;

        let Some(cursor) = ctx.cursor_pos else {
            return Vec::new();
        };
        let elements = self.build_elements(renderer, cursor, ctx.output_size, ctx.scale);

        // Intra-effect z-order: cursor_label → lines → rulers (topmost → bottom).
        let mut out = Vec::with_capacity(
            elements.lines.len() + elements.rulers.len() + elements.cursor_label.is_some() as usize,
        );
        if let Some(label) = elements.cursor_label {
            out.push(CustomElement::Label(label));
        }
        for line in elements.lines {
            out.push(CustomElement::Solid(line));
        }
        for ruler in elements.rulers {
            out.push(CustomElement::Label(ruler));
        }
        out
    }
}
