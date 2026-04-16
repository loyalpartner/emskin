//! Rendering helpers owned by effect-core.
//!
//! - `render_workspace`: the top-level "compose a frame" entry point. Drives
//!   the `EffectChain`'s pre_paint → paint → post_paint lifecycle and
//!   delegates to smithay's damage-tracked `render_output`. From this module's
//!   perspective any window state handed in by the host is **fixed**.
//! - `paint_buffer` / `draw_text_onto`: memory-buffer drawing utilities shared
//!   by overlays.

use std::convert::Infallible;

use cosmic_text::{Buffer as CtBuffer, Color as CtColor, FontSystem, SwashCache};
use smithay::{
    backend::renderer::{
        damage::{Error as DamageError, OutputDamageTracker},
        element::memory::MemoryRenderBuffer,
        gles::{GlesError, GlesRenderer},
        Color32F, RendererSuper,
    },
    desktop::{space::render_output, Space, Window},
    output::Output,
    utils::{Buffer, Rectangle, Size},
};

use crate::{CustomElement, EffectChain, EffectCtx};

/// Outcome of a render pass.
pub struct RenderWorkspaceOutcome {
    /// `true` if any active effect requested another frame (animation driver).
    pub want_redraw: bool,
}

/// Compose a workspace's frame: run the effect chain, combine its elements
/// with the caller-supplied extras, and hand everything to smithay's
/// damage-tracked renderer.
#[allow(clippy::too_many_arguments)]
///
/// `extras` are non-effect render elements (software cursor, layer-shell
/// surfaces, window mirrors) that the host has already captured as a snapshot
/// for this frame. Chain elements render **above** extras because chain
/// position 0–100 maps to the topmost slots in smithay's element vector.
pub fn render_workspace(
    output: &Output,
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    space: &Space<Window>,
    chain: &mut EffectChain,
    ctx: &EffectCtx,
    mut extras: Vec<CustomElement<GlesRenderer>>,
    damage_tracker: &mut OutputDamageTracker,
    clear_color: impl Into<Color32F>,
) -> Result<RenderWorkspaceOutcome, DamageError<GlesError>> {
    chain.pre_paint(ctx);
    let mut elements = chain.paint(renderer, ctx);
    // Chain output first (topmost) → then host-supplied extras → then space.
    elements.append(&mut extras);

    render_output::<GlesRenderer, CustomElement<GlesRenderer>, Window, _>(
        output,
        renderer,
        framebuffer,
        1.0,
        0,
        [space],
        &elements,
        damage_tracker,
        clear_color,
    )?;

    let want_redraw = chain.post_paint();
    Ok(RenderWorkspaceOutcome { want_redraw })
}

/// Resize `buf` to `size`, fill its bytes via `paint`, and emit full-buffer
/// damage. Standardizes the overlay draw pattern and hides the `Infallible`
/// error plumbing that every `MemoryRenderBuffer::draw` call needs.
pub fn paint_buffer<F>(buf: &mut MemoryRenderBuffer, size: Size<i32, Buffer>, paint: F)
where
    F: FnOnce(&mut [u8]),
{
    let mut ctx = buf.render();
    ctx.resize((size.w, size.h));
    ctx.draw(|data| {
        paint(data);
        Ok::<_, Infallible>(vec![Rectangle::from_size(size)])
    })
    .unwrap();
}

/// Alpha-blend cosmic-text glyphs onto a BGRA pixel buffer (Fourcc::Argb8888
/// on little-endian). `fg` is BGRA-ordered.
#[allow(clippy::too_many_arguments)]
pub fn draw_text_onto(
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
