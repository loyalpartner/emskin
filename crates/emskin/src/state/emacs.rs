//! Emacs host-process state — the main surface, the spawned `emacs`
//! child, title / app-id metadata forwarded to the host toplevel, and
//! the two latches (`detect`, `initial_size_settled`) that encode the
//! "first toplevel == Emacs" heuristic and the associated end-of-life
//! detection.
//!
//! The detection heuristic exists because emskin deliberately does not
//! run a separate bootstrap handshake with Emacs — the first
//! `xdg_toplevel` that maps is claimed as the main surface, the frame
//! is resized to cover the host window, and the initial configure is
//! acked. After that latch, subsequent toplevels are plain embedded
//! apps. This is disabled via `EMSKIN_DISABLE_EMACS_DETECTION=1` for
//! e2e tests that spawn transient Wayland clients (wl-copy, xclip …)
//! without a real Emacs ever attaching — otherwise the first test
//! client would be mis-tagged as Emacs and its exit would trip the
//! "last Emacs died → shutdown" path.

use smithay::reexports::wayland_server::{protocol::wl_surface::WlSurface, Resource};

pub struct EmacsState {
    surface: Option<WlSurface>,
    child: Option<std::process::Child>,
    title: Option<String>,
    /// Currently write-only — stored on `xdg_toplevel.set_app_id`
    /// but never forwarded. Kept on the struct so the wire-level
    /// handler has somewhere to park the value; retiring it is a
    /// separate decision.
    app_id: Option<String>,
    detect: bool,
    initial_size_settled: bool,
    /// Deferred `set_fullscreen` request — produced by
    /// `xdg_toplevel.set_fullscreen` on the Emacs surface, drained by
    /// `apply_pending_state` on the next render tick. The CLI
    /// `--fullscreen` startup flag piggybacks on the same mailbox.
    pending_fullscreen: Option<bool>,
    /// Deferred `set_maximized` request — mirror of
    /// `pending_fullscreen` for the maximize toplevel state.
    pending_maximize: Option<bool>,
}

impl EmacsState {
    /// Construct with detection toggled from the `EMSKIN_DISABLE_EMACS_DETECTION`
    /// env var (absent → `true`; present → `false`). The caller reads
    /// the env itself so tests can construct `EmacsState { detect:
    /// false, .. }` without touching the process environment.
    pub fn new(detect: bool) -> Self {
        Self {
            surface: None,
            child: None,
            title: None,
            app_id: None,
            detect,
            initial_size_settled: false,
            pending_fullscreen: None,
            pending_maximize: None,
        }
    }

    // -- Main surface ------------------------------------------------

    /// The main Emacs surface, or `None` if unset. Stable across the
    /// lifetime of an Emacs instance — reassigned only on workspace
    /// switch (which swaps in a different workspace's Emacs surface).
    pub fn surface(&self) -> Option<&WlSurface> {
        self.surface.as_ref()
    }

    /// Assign the main surface. Overwrites any prior value.
    pub fn set_surface(&mut self, surface: Option<WlSurface>) {
        self.surface = surface;
    }

    /// Remove and return the main surface. Used by workspace-switch
    /// to hand the current Emacs frame over to the inactive workspace
    /// store before loading the incoming workspace's surface.
    pub fn take_surface(&mut self) -> Option<WlSurface> {
        self.surface.take()
    }

    /// True when `candidate` is the current main Emacs surface.
    pub fn is_main_surface(&self, candidate: &WlSurface) -> bool {
        self.surface.as_ref() == Some(candidate)
    }

    /// True when the main-surface slot is set.
    pub fn has_main_surface(&self) -> bool {
        self.surface.is_some()
    }

    // -- Process handle ---------------------------------------------

    /// Stash the spawned `emacs` child so the shutdown path can reap
    /// it and the tick loop can poll liveness.
    pub fn set_child(&mut self, child: std::process::Child) {
        self.child = Some(child);
    }

    /// Remove and return the child handle (shutdown path).
    pub fn take_child(&mut self) -> Option<std::process::Child> {
        self.child.take()
    }

    /// Mutable access to the child, for `try_wait` polling.
    pub fn child_mut(&mut self) -> Option<&mut std::process::Child> {
        self.child.as_mut()
    }

    // -- Title / app_id metadata ------------------------------------

    /// Record an Emacs-supplied window title. The render loop drains
    /// it via `take_title` once per frame to forward to the host
    /// winit window.
    pub fn set_title(&mut self, title: String) {
        self.title = Some(title);
    }

    pub fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    /// Record an Emacs-supplied app_id. See struct field note —
    /// currently write-only.
    pub fn set_app_id(&mut self, app_id: String) {
        self.app_id = Some(app_id);
    }

    // -- Pending host-window-state requests -------------------------

    /// Request the host winit window enter/leave fullscreen. Drained
    /// by `apply_pending_state` in the next render tick.
    pub fn request_fullscreen(&mut self, fullscreen: bool) {
        self.pending_fullscreen = Some(fullscreen);
    }

    pub fn take_pending_fullscreen(&mut self) -> Option<bool> {
        self.pending_fullscreen.take()
    }

    /// Request the host winit window enter/leave maximize.
    pub fn request_maximize(&mut self, maximize: bool) {
        self.pending_maximize = Some(maximize);
    }

    pub fn take_pending_maximize(&mut self) -> Option<bool> {
        self.pending_maximize.take()
    }

    // -- Lifecycle latches ------------------------------------------

    /// Flip once the first `initial_configure` reply from Emacs has
    /// arrived, sizing the main surface to the host window.
    pub fn mark_size_settled(&mut self) {
        self.initial_size_settled = true;
    }

    /// Whether the main-frame size-settle event has happened. Gates
    /// host `Resized` → Emacs propagation and the shutdown check.
    pub fn size_settled(&self) -> bool {
        self.initial_size_settled
    }

    pub fn detection_enabled(&self) -> bool {
        self.detect
    }

    // -- Composite predicates that encapsulate the detection rules --

    /// A new `xdg_toplevel` should be claimed as the main Emacs
    /// surface: detection is on, nothing has been claimed yet, and
    /// the size-settle latch has not fired.
    pub fn should_claim_main(&self) -> bool {
        self.detect && self.surface.is_none() && !self.initial_size_settled
    }

    /// The main Emacs surface has died after having been established
    /// (i.e. `initial_size_settled` was reached) — the compositor
    /// should shut down. Detection-off means never shut down via this
    /// path; that mirrors the intent of `EMSKIN_DISABLE_EMACS_DETECTION`
    /// for tests that don't want a dying wl-copy to kill the harness.
    pub fn main_died(&self) -> bool {
        self.detect
            && self.initial_size_settled
            && self.surface.as_ref().is_some_and(|s| !s.is_alive())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `WlSurface` requires a live Display + Client to instantiate, so
    // all surface-related predicates (`is_main_surface`, the live/dead
    // branch of `main_died`, the `set_surface(Some(..))` path) are
    // covered by the e2e suite — only the `None` / default-false
    // branches are exercised here. Everything else (child, title,
    // app_id, latches, composite predicates with surface=None) is
    // pure logic and fair game.

    fn short_lived_child() -> std::process::Child {
        // `true` exits successfully with no output; the test asserts
        // presence/handover of the `Child` handle itself, not runtime
        // behavior.
        std::process::Command::new("true")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn /bin/true for test")
    }

    #[test]
    fn new_with_detection_on_initialises_empty_and_unsettled() {
        let e = EmacsState::new(true);
        assert!(e.surface().is_none());
        assert!(!e.has_main_surface());
        assert!(!e.size_settled());
        assert!(e.detection_enabled());
        // No child, no title, no app_id right after construction.
        assert!(e.title.is_none());
        assert!(e.app_id.is_none());
    }

    #[test]
    fn new_with_detection_off_disables_detection_flag() {
        let e = EmacsState::new(false);
        assert!(!e.detection_enabled());
        // Other fields still default.
        assert!(e.surface().is_none());
        assert!(!e.size_settled());
    }

    #[test]
    fn set_surface_none_is_idempotent_from_default() {
        let mut e = EmacsState::new(true);
        e.set_surface(None);
        assert!(e.surface().is_none());
        assert!(!e.has_main_surface());
    }

    #[test]
    fn take_surface_from_empty_returns_none() {
        let mut e = EmacsState::new(true);
        assert!(e.take_surface().is_none());
    }

    #[test]
    fn set_and_take_child_handoff() {
        let mut e = EmacsState::new(true);
        assert!(e.child_mut().is_none());

        e.set_child(short_lived_child());
        assert!(e.child_mut().is_some(), "set_child must store the handle");

        let mut taken = e.take_child().expect("take_child returns what was set");
        assert!(e.take_child().is_none(), "take_child must clear the slot");
        // Reap so the test doesn't leave a zombie if `true` hasn't
        // been polled yet.
        let _ = taken.wait();
    }

    #[test]
    fn set_child_overwrites_previous_without_panic() {
        let mut e = EmacsState::new(true);
        e.set_child(short_lived_child());
        e.set_child(short_lived_child());
        // Both handles end up in the slot sequentially; we only care
        // that the second call doesn't panic and leaves a handle.
        let mut taken = e.take_child().expect("second set_child took effect");
        let _ = taken.wait();
    }

    #[test]
    fn title_set_then_take() {
        let mut e = EmacsState::new(true);
        assert!(e.take_title().is_none());

        e.set_title("GNU Emacs".into());
        assert_eq!(e.take_title().as_deref(), Some("GNU Emacs"));
        assert!(
            e.take_title().is_none(),
            "take drains — second read returns None"
        );
    }

    #[test]
    fn title_set_overwrites_previous() {
        let mut e = EmacsState::new(true);
        e.set_title("old".into());
        e.set_title("new".into());
        assert_eq!(e.take_title().as_deref(), Some("new"));
    }

    #[test]
    fn app_id_stored_but_not_readable_publicly() {
        // The struct's field note says app_id is write-only at the
        // public API level. The test asserts via the private field
        // that `set_app_id` writes through.
        let mut e = EmacsState::new(true);
        e.set_app_id("emacs".into());
        assert_eq!(e.app_id.as_deref(), Some("emacs"));
        e.set_app_id("Emacs".into());
        assert_eq!(
            e.app_id.as_deref(),
            Some("Emacs"),
            "set_app_id overwrites previous value"
        );
    }

    #[test]
    fn mark_size_settled_flips_latch() {
        let mut e = EmacsState::new(true);
        assert!(!e.size_settled());
        e.mark_size_settled();
        assert!(e.size_settled());
        // Idempotent.
        e.mark_size_settled();
        assert!(e.size_settled());
    }

    #[test]
    fn should_claim_main_true_initially_when_detection_on() {
        let e = EmacsState::new(true);
        assert!(e.should_claim_main());
    }

    #[test]
    fn should_claim_main_false_when_detection_off() {
        let e = EmacsState::new(false);
        assert!(!e.should_claim_main());
    }

    #[test]
    fn should_claim_main_false_once_size_settled() {
        let mut e = EmacsState::new(true);
        e.mark_size_settled();
        assert!(
            !e.should_claim_main(),
            "claim window closes after initial size settles"
        );
    }

    #[test]
    fn main_died_false_when_detection_off() {
        let mut e = EmacsState::new(false);
        // Even if we forced size_settled and cleared the surface, the
        // detection-off shortcut keeps this false so the harness
        // doesn't shut down on transient clients dying.
        e.mark_size_settled();
        assert!(!e.main_died());
    }

    #[test]
    fn main_died_false_before_initial_size_settled() {
        let e = EmacsState::new(true);
        // No surface ever set, size-settle never fired — not a "died"
        // condition (Emacs never mapped in the first place).
        assert!(!e.main_died());
    }

    #[test]
    fn main_died_false_when_surface_absent_even_after_settle() {
        // This guards against a bug where clearing the surface via
        // workspace-switch would trip main_died. main_died's Surface
        // branch is e2e-covered; this test asserts the surface=None
        // branch returns false regardless of size_settled.
        let mut e = EmacsState::new(true);
        e.mark_size_settled();
        assert!(!e.main_died());
    }

    // -- pending_fullscreen / pending_maximize ----------------------

    #[test]
    fn pending_fullscreen_empty_by_default() {
        let mut e = EmacsState::new(true);
        assert!(e.take_pending_fullscreen().is_none());
    }

    #[test]
    fn request_fullscreen_true_roundtrips_via_take() {
        let mut e = EmacsState::new(true);
        e.request_fullscreen(true);
        assert_eq!(e.take_pending_fullscreen(), Some(true));
        assert!(
            e.take_pending_fullscreen().is_none(),
            "take drains — a second read returns None"
        );
    }

    #[test]
    fn request_fullscreen_false_roundtrips_via_take() {
        let mut e = EmacsState::new(true);
        e.request_fullscreen(false);
        assert_eq!(e.take_pending_fullscreen(), Some(false));
    }

    #[test]
    fn request_fullscreen_overwrites_previous_pending_value() {
        // If two requests arrive before the render tick drains, the
        // later one wins — that matches the existing flat-assignment
        // semantics (`state.pending_fullscreen = Some(true)` etc.).
        let mut e = EmacsState::new(true);
        e.request_fullscreen(false);
        e.request_fullscreen(true);
        assert_eq!(e.take_pending_fullscreen(), Some(true));
    }

    #[test]
    fn pending_maximize_empty_by_default() {
        let mut e = EmacsState::new(true);
        assert!(e.take_pending_maximize().is_none());
    }

    #[test]
    fn request_maximize_true_roundtrips_via_take() {
        let mut e = EmacsState::new(true);
        e.request_maximize(true);
        assert_eq!(e.take_pending_maximize(), Some(true));
        assert!(e.take_pending_maximize().is_none());
    }

    #[test]
    fn request_maximize_overwrites_previous_pending_value() {
        let mut e = EmacsState::new(true);
        e.request_maximize(false);
        e.request_maximize(true);
        assert_eq!(e.take_pending_maximize(), Some(true));
    }

    #[test]
    fn pending_fullscreen_and_maximize_are_independent() {
        // Setting one must not drain or contaminate the other.
        let mut e = EmacsState::new(true);
        e.request_fullscreen(true);
        assert!(e.take_pending_maximize().is_none());
        e.request_maximize(false);
        assert_eq!(e.take_pending_fullscreen(), Some(true));
        assert_eq!(e.take_pending_maximize(), Some(false));
    }
}
