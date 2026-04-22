//! Built-in visual overlays and their window-manager-side bookkeeping.
//!
//! Each overlay is owned as `Rc<RefCell<T>>` and registered into
//! `chain` through `effect_core::EffectHandle`, which keeps a clone of
//! the same `Rc`. That lets the window manager call typed setters
//! (`set_enabled`, `set_rects`, `click_at`, …) directly while the
//! render loop iterates the trait-erased chain — see
//! `../../effect-plugins/CLAUDE.md` for the plugin-side half of this
//! contract.
//!
//! The three `*_click_absorbed` / `last_*` flags are **not** effect
//! state: they are edge-detect latches kept on the host so a pure
//! `Effect` never has to peek at higher-level state (IPC connection,
//! recorder state machine, …). Grouping them with the handles keeps
//! the whole "overlays" concern in one place.

use std::cell::RefCell;
use std::rc::Rc;

use effect_core::{EffectChain, EffectHandle};
use effect_plugins::{
    cursor_trail::CursorTrail, jelly_cursor::JellyCursor, key_cast::KeyCastOverlay,
    measure::MeasureOverlay, recorder::RecorderOverlay, skeleton::SkeletonOverlay,
    splash::SplashScreen,
};

pub struct EffectsState {
    /// Trait-erased render order. Iterated by the winit render loop via
    /// `effect_core::render_workspace`.
    pub chain: EffectChain,

    // --- Typed handles. Same instance as the entry in `chain`. ---
    pub measure: Rc<RefCell<MeasureOverlay>>,
    pub skeleton: Rc<RefCell<SkeletonOverlay>>,
    pub splash: Rc<RefCell<SplashScreen>>,
    pub cursor_trail: Rc<RefCell<CursorTrail>>,
    pub jelly_cursor: Rc<RefCell<JellyCursor>>,
    pub recorder_overlay: Rc<RefCell<RecorderOverlay>>,
    pub key_cast: Rc<RefCell<KeyCastOverlay>>,

    // --- Edge-detect latches owned by the host. ---
    /// A skeleton label-click was swallowed on press; the matching
    /// release must also be swallowed so the underlying surface never
    /// sees a lone `Released` without its `Pressed`.
    pub skeleton_click_absorbed: bool,

    /// `true` once Emacs's surface has appeared. Flipping `false → true`
    /// fires `splash.dismiss()` exactly once.
    pub last_emacs_connected: bool,

    /// Mirrors `recorder.is_recording()` from the previous frame. Flip
    /// toggles `key_cast` so screencasts always show keystrokes without
    /// the user having to enable it separately.
    pub last_recording_active: bool,
}

impl Default for EffectsState {
    fn default() -> Self {
        let mut chain = EffectChain::default();
        // Render order is the registration order: splash sits above
        // skeleton/measure (drawn later = on top). Do not reorder
        // without checking `effect-plugins/CLAUDE.md`'s chain_position
        // table.
        let splash = register(&mut chain, SplashScreen::new());
        let skeleton = register(&mut chain, SkeletonOverlay::new());
        let measure = register(&mut chain, MeasureOverlay::new());
        let cursor_trail = register(&mut chain, CursorTrail::new());
        let jelly_cursor = register(&mut chain, JellyCursor::new());
        let recorder_overlay = register(&mut chain, RecorderOverlay::new());
        let key_cast = register(&mut chain, KeyCastOverlay::new());

        Self {
            chain,
            measure,
            skeleton,
            splash,
            cursor_trail,
            jelly_cursor,
            recorder_overlay,
            key_cast,
            skeleton_click_absorbed: false,
            last_emacs_connected: false,
            last_recording_active: false,
        }
    }
}

impl EffectsState {
    /// Clear per-workspace visual state on workspace switch. The caller
    /// passes the current elapsed time so the jelly overlay can reset
    /// its caret tracking without animating from the departing
    /// workspace's last known position.
    pub fn reset_on_workspace_switch(&mut self, now: std::time::Duration) {
        let mut sk = self.skeleton.borrow_mut();
        sk.set_enabled(false);
        sk.clear();
        drop(sk);
        self.skeleton_click_absorbed = false;
        // Fresh `SetCursorRect` IPC messages will arrive from the new
        // workspace's Emacs once focus stabilises.
        self.jelly_cursor.borrow_mut().update(None, now);
    }
}

/// Register an overlay into the chain and return a typed handle to the
/// same instance. Private — construction of overlays is an
/// implementation detail of `EffectsState::default`.
fn register<T: effect_core::Effect + 'static>(chain: &mut EffectChain, value: T) -> Rc<RefCell<T>> {
    let rc = Rc::new(RefCell::new(value));
    chain.register(EffectHandle::new(rc.clone()));
    rc
}
