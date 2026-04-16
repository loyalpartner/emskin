//! The `Effect` trait — the minimal contract a visual overlay implements.
//!
//! An Effect is a pure *data → pixels* transformer. It does not know about
//! windows, workspaces, Emacs, or IPC. Those concerns belong to the host
//! compositor (emskin); it interacts with plugins via the struct's own typed
//! methods, not through this trait.

use smithay::backend::renderer::gles::GlesRenderer;

use crate::{CustomElement, EffectCtx};

pub trait Effect {
    /// Stable identifier — used for debug logging.
    fn name(&self) -> &'static str;

    /// Per-frame filter. When `false`, the chain skips `pre_paint` / `paint`
    /// / `post_paint` for this frame. Mirrors KWin's `Effect::isActive`.
    fn is_active(&self) -> bool;

    /// 0..=100, higher = painted on top. Higher values appear earlier in the
    /// chain's output Vec (which is the topmost slot in `custom_elements`).
    fn chain_position(&self) -> u8 {
        50
    }

    /// Animation tick / state update before `paint`. Default: no-op.
    fn pre_paint(&mut self, _ctx: &EffectCtx) {}

    /// Produce this effect's render elements for the frame. Intra-effect
    /// z-order is the Vec order (index 0 is the topmost of this effect).
    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &EffectCtx,
    ) -> Vec<CustomElement<GlesRenderer>>;

    /// Post-paint housekeeping. Return `true` to request another frame
    /// (e.g. to drive an animation).
    fn post_paint(&mut self) -> bool {
        false
    }

    /// When `true`, the chain removes this effect after `post_paint`. Used by
    /// one-shot effects like `SplashScreen`.
    fn should_remove(&self) -> bool {
        false
    }
}
