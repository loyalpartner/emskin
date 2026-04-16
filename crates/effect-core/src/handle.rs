//! `EffectHandle<T>` — `Rc<RefCell<T>>` wrapper that also implements `Effect`.
//!
//! The host (emskin) keeps a typed `Rc<RefCell<MyOverlay>>` so it can call
//! concrete setter methods (`set_enabled`, `set_rects`, `click_at`, …) while
//! registering the same instance into the `EffectChain` via this wrapper so
//! the render pipeline can drive its visual methods.

use std::{cell::RefCell, rc::Rc};

use smithay::backend::renderer::gles::GlesRenderer;

use crate::{CustomElement, Effect, EffectCtx};

pub struct EffectHandle<T>(Rc<RefCell<T>>);

impl<T> EffectHandle<T> {
    pub fn new(inner: Rc<RefCell<T>>) -> Self {
        Self(inner)
    }
}

impl<T: Effect + 'static> Effect for EffectHandle<T> {
    fn name(&self) -> &'static str {
        self.0.borrow().name()
    }

    fn is_active(&self) -> bool {
        self.0.borrow().is_active()
    }

    fn chain_position(&self) -> u8 {
        self.0.borrow().chain_position()
    }

    fn pre_paint(&mut self, ctx: &EffectCtx) {
        self.0.borrow_mut().pre_paint(ctx);
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &EffectCtx,
    ) -> Vec<CustomElement<GlesRenderer>> {
        self.0.borrow_mut().paint(renderer, ctx)
    }

    fn post_paint(&mut self) -> bool {
        self.0.borrow_mut().post_paint()
    }

    fn should_remove(&self) -> bool {
        self.0.borrow().should_remove()
    }
}
