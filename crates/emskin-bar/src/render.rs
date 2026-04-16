//! SHM rendering for the bar.
//!
//! One argb8888 buffer per frame, drawn on the CPU. Each workspace becomes
//! a flat rect ("pill") with its name centred inside; the active workspace
//! gets an accent background. Hit-test rectangles are stashed back on each
//! `WorkspaceEntry` so `state::BarState::handle_click` can reuse them.
//!
//! Colours: Catppuccin Mocha, same palette the compositor's splash uses.

use cosmic_text::{Attrs, Family, Metrics, Shaping, Weight};
use smithay_client_toolkit::shell::WaylandSurface;
use wayland_client::protocol::wl_shm;

use crate::state::BarState;

// --- Layout constants --------------------------------------------------------

pub const BAR_HEIGHT: u32 = 28;
pub const PILL_H_PAD: i32 = 10;
/// Gap between pills.
const PILL_GAP: i32 = 6;
/// Left/right padding inside the bar.
const BAR_PAD: i32 = 8;
/// Inner text padding inside a pill.
const PILL_TEXT_PAD: i32 = 12;

// --- Palette (BGRA, u8) ------------------------------------------------------

/// Bar background: Catppuccin Mocha base.
const BG: [u8; 4] = [0x2e, 0x1e, 0x1e, 0xff];
/// Inactive pill background: surface0.
const PILL_BG: [u8; 4] = [0x45, 0x34, 0x31, 0xff];
/// Active pill background: blue accent.
const PILL_BG_ACTIVE: [u8; 4] = [0xfa, 0xb4, 0x89, 0xff];
/// Inactive pill text.
const PILL_FG: [u8; 4] = [0xf5, 0xe0, 0xdc, 0xff];
/// Active pill text: base (so it pops against accent bg).
const PILL_FG_ACTIVE: [u8; 4] = [0x2e, 0x1e, 0x1e, 0xff];

// --- Font --------------------------------------------------------------------

const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = 16.0;

impl BarState {
    pub(crate) fn draw(&mut self) {
        let Some(layer) = self.layer.clone() else {
            return;
        };
        let Some((width, height)) = self.surface_size else {
            return;
        };

        let stride = width as i32 * 4;
        let Ok((buffer, canvas)) = self.pool.create_buffer(
            width as i32,
            height as i32,
            stride,
            wl_shm::Format::Argb8888,
        ) else {
            tracing::warn!("SlotPool::create_buffer failed");
            return;
        };

        // --- Background fill ---
        for chunk in canvas.chunks_exact_mut(4) {
            chunk.copy_from_slice(&BG);
        }

        // --- Lay out pills + draw ---
        let display_text = display_names(&self.workspaces);
        let pill_widths: Vec<i32> = display_text
            .iter()
            .map(|t| measure_text(&mut self.font_system, &mut self.swash_cache, t))
            .map(|w| w + PILL_TEXT_PAD * 2)
            .collect();

        let mut cursor_x = BAR_PAD;
        for (i, ws) in self.workspaces.iter_mut().enumerate() {
            let pw = pill_widths[i];
            let ph = height as i32 - 2 * PILL_H_PAD / 3;
            let py = (height as i32 - ph) / 2;
            let bg = if ws.active { PILL_BG_ACTIVE } else { PILL_BG };
            let fg = if ws.active { PILL_FG_ACTIVE } else { PILL_FG };

            fill_rect(
                canvas,
                width as i32,
                height as i32,
                cursor_x,
                py,
                pw,
                ph,
                bg,
            );

            // Centre the text vertically/horizontally in the pill.
            draw_text(
                &mut self.font_system,
                &mut self.swash_cache,
                canvas,
                width as i32,
                height as i32,
                cursor_x + PILL_TEXT_PAD,
                py,
                ph,
                &display_text[i],
                fg,
            );

            ws.hit_rect = (cursor_x, py, pw, ph);
            cursor_x += pw + PILL_GAP;
        }

        let surface = layer.wl_surface();
        surface.damage_buffer(0, 0, width as i32, height as i32);
        if buffer.attach_to(surface).is_ok() {
            layer.commit();
        }
    }
}

// -----------------------------------------------------------------------------
// Helpers — buffer fills + cosmic-text blitting
// -----------------------------------------------------------------------------

fn display_names(workspaces: &[crate::workspace::WorkspaceEntry]) -> Vec<String> {
    workspaces
        .iter()
        .map(|w| {
            if w.name.is_empty() {
                format!("WS {}", w.id)
            } else {
                w.name.clone()
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    canvas: &mut [u8],
    buf_w: i32,
    buf_h: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: [u8; 4],
) {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(buf_w);
    let y1 = (y + h).min(buf_h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for row in y0..y1 {
        let row_off = (row * buf_w * 4) as usize;
        for col in x0..x1 {
            let off = row_off + (col * 4) as usize;
            canvas[off..off + 4].copy_from_slice(&color);
        }
    }
}

/// Measure the unshaped pixel width of a string at the bar's font size.
fn measure_text(
    font_system: &mut cosmic_text::FontSystem,
    _cache: &mut cosmic_text::SwashCache,
    text: &str,
) -> i32 {
    let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
    let mut buf = cosmic_text::Buffer::new(font_system, metrics);
    buf.set_size(font_system, Some(f32::INFINITY), Some(f32::INFINITY));
    let attrs = Attrs::new()
        .family(Family::SansSerif)
        .weight(Weight::MEDIUM);
    buf.set_text(font_system, text, &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    let mut w = 0.0f32;
    for run in buf.layout_runs() {
        w = w.max(run.line_w);
    }
    w.ceil() as i32
}

/// Blit a string into the argb8888 `canvas` using cosmic-text + swash.
#[allow(clippy::too_many_arguments)]
fn draw_text(
    font_system: &mut cosmic_text::FontSystem,
    cache: &mut cosmic_text::SwashCache,
    canvas: &mut [u8],
    buf_w: i32,
    buf_h: i32,
    dst_x: i32,
    dst_y: i32,
    dst_h: i32,
    text: &str,
    color: [u8; 4], // BGRA
) {
    let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
    let mut buf = cosmic_text::Buffer::new(font_system, metrics);
    buf.set_size(font_system, Some(f32::INFINITY), Some(f32::INFINITY));
    let attrs = Attrs::new()
        .family(Family::SansSerif)
        .weight(Weight::MEDIUM);
    buf.set_text(font_system, text, &attrs, Shaping::Advanced, None);
    buf.shape_until_scroll(font_system, false);

    // Vertical centring: cosmic-text draws from baseline; use line_top +
    // (line_height - ascent) heuristic — simple midline works well enough.
    let baseline_y = dst_y + dst_h / 2 + (LINE_HEIGHT as i32) / 3;

    buf.draw(
        font_system,
        cache,
        cosmic_text::Color::rgba(color[2], color[1], color[0], color[3]),
        |px, py, _pw, _ph, glyph_color| {
            let x = dst_x + px;
            let y = baseline_y + py - LINE_HEIGHT as i32;
            if x < 0 || x >= buf_w || y < 0 || y >= buf_h {
                return;
            }
            let a = glyph_color.a();
            if a == 0 {
                return;
            }
            let off = ((y * buf_w + x) * 4) as usize;
            // Manual alpha blend onto existing background.
            let sb = glyph_color.b() as u32;
            let sg = glyph_color.g() as u32;
            let sr = glyph_color.r() as u32;
            let sa = a as u32;
            let inv = 255 - sa;
            let db = canvas[off] as u32;
            let dg = canvas[off + 1] as u32;
            let dr = canvas[off + 2] as u32;
            canvas[off] = ((sb * sa + db * inv) / 255) as u8;
            canvas[off + 1] = ((sg * sa + dg * inv) / 255) as u8;
            canvas[off + 2] = ((sr * sa + dr * inv) / 255) as u8;
            canvas[off + 3] = 0xff;
        },
    );
}
