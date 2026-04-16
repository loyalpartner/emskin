//! effect-core — the rendering framework and effect contract for emskin.
//!
//! This crate owns three things:
//!
//! 1. **The `Effect` trait** — a minimal "visual transformer" contract.
//!    Plugins in `effect-plugins` implement it. The trait is intentionally
//!    free of any window-management, IPC or configuration concepts; those
//!    belong to the host compositor (emskin). The host interacts with each
//!    plugin via the plugin's own typed methods, not through the trait.
//!
//! 2. **The `EffectChain`** — registers effects and drives their per-frame
//!    lifecycle (`pre_paint` / `paint` / `post_paint`).
//!
//! 3. **`render_workspace`** — a thin wrapper around smithay's damage-tracked
//!    `render_output` that combines the effect chain's output with
//!    host-supplied "extra" elements (cursor, layer shell surfaces, window
//!    mirrors) and composes the final frame.
//!
//! From effect-core's perspective, any window information supplied by the
//! host (the `&Space<Window>` reference) is already fixed for the frame —
//! this crate never mutates window state.

mod chain;
mod ctx;
mod effect;
mod element;
mod handle;
mod render;

pub use chain::EffectChain;
pub use ctx::EffectCtx;
pub use effect::Effect;
pub use element::{CustomElement, EmskinRenderer};
pub use handle::EffectHandle;
pub use render::{draw_text_onto, paint_buffer, render_workspace, RenderWorkspaceOutcome};
