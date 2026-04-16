//! `EffectChain` — registers effects and drives them per frame.

use std::cmp::Reverse;

use smithay::backend::renderer::gles::GlesRenderer;

use crate::{CustomElement, Effect, EffectCtx};

#[derive(Default)]
pub struct EffectChain {
    effects: Vec<Box<dyn Effect>>,
}

impl EffectChain {
    pub fn register<E: Effect + 'static>(&mut self, e: E) {
        self.effects.push(Box::new(e));
        // Stable sort by `chain_position` descending so Vec[0] = topmost.
        self.effects.sort_by_key(|e| Reverse(e.chain_position()));
    }

    pub fn pre_paint(&mut self, ctx: &EffectCtx) {
        for effect in self.effects.iter_mut().filter(|e| e.is_active()) {
            effect.pre_paint(ctx);
        }
    }

    pub fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &EffectCtx,
    ) -> Vec<CustomElement<GlesRenderer>> {
        let mut out = Vec::new();
        for effect in self.effects.iter_mut().filter(|e| e.is_active()) {
            out.extend(effect.paint(renderer, ctx));
        }
        out
    }

    /// Returns `true` if any active effect requested another frame.
    pub fn post_paint(&mut self) -> bool {
        let mut want_redraw = false;
        for effect in self.effects.iter_mut().filter(|e| e.is_active()) {
            want_redraw |= effect.post_paint();
        }
        self.effects.retain(|e| !e.should_remove());
        want_redraw
    }
}
