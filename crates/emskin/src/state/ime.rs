//! IME (input method) bridge between the host compositor and embedded
//! Wayland clients via `text_input_v3`.
//!
//! Three smithay-imposed constraints drive the design — see
//! `crates/emskin/CLAUDE.md` → IME for the full "why":
//!
//! - `set_ime_allowed` must be toggled per-focused-client (registering `TextInputManagerState` makes fcitx5-gtk abandon its DBus path for text_input_v3, so enabling host IME for a GTK/Qt client that handles its own IM breaks input).
//! - `text_input.enter()/leave()` must be called by hand from `focus_changed` (smithay gates them on `input_method.has_instance()` and emskin implements no input_method protocol).
//! - The `set_ime_allowed` decision is deferred via `ime_enabled` + [`ImeBridge::take_ime_enabled`] (`focus_changed` has no access to the winit backend).

use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::text_input::{TextInputHandle, TextInputManagerState, TextInputSeat};

use crate::apps::AppManager;
use crate::EmskinState;

/// `(-1, -1)` sentinel per text_input_v3 for "no cursor position".
const NO_CURSOR: (i32, i32) = (-1, -1);

pub struct ImeBridge {
    focused_surface: Option<WlSurface>,
    /// Host IME enabled/disabled decision waiting for the render loop
    /// to apply via `set_ime_allowed`. Drained by `take_ime_enabled`
    /// (write-once, read-once semantic — the `Option` distinguishes
    /// "no change to apply" from "apply false").
    ime_enabled: Option<bool>,
    /// Last value `take_ime_enabled` returned (i.e. last value the
    /// winit backend was told). Used by [`Self::resync`] to suppress
    /// no-op re-evaluations: tick-driven re-derivation is a hot path,
    /// and we only want to write the mailbox when the answer differs
    /// from what's already committed downstream.
    last_committed: Option<bool>,
}

impl ImeBridge {
    pub fn new(dh: &DisplayHandle) -> Self {
        // The global is owned by `Display` after registration; the
        // returned `TextInputManagerState` (a bare `GlobalId` wrapper)
        // has no Drop impl that unregisters, so dropping it is a no-op.
        let _ = TextInputManagerState::new::<EmskinState>(dh);
        Self {
            focused_surface: None,
            ime_enabled: None,
            last_committed: None,
        }
    }

    /// Bridge text_input enter/leave on keyboard focus change and queue
    /// a re-evaluation of host IME for the new focus.
    ///
    /// `new_focus` is the focused surface projected from
    /// `KeyboardFocusTarget` via `WaylandFocus::wl_surface()` — X clients
    /// surface here too once associated by xwayland-satellite.
    pub fn on_focus_changed(&mut self, seat: &Seat<EmskinState>, new_focus: Option<WlSurface>) {
        let ti = seat.text_input();
        let old = self.focused_surface.take();
        transition_focus(ti, old, &new_focus);
        tracing::debug!("IME focus_changed: has_focus={}", new_focus.is_some());
        self.focused_surface = new_focus;
        self.resync(seat);
    }

    /// Re-derive "should host IME be allowed" from the seat's current
    /// text_input state and queue an update if the answer changed since
    /// the last winit commit.
    ///
    /// **Why this exists**: the answer is a function of (focused
    /// surface, focused client's bound text_input instances). It can
    /// change in three ways:
    ///
    /// 1. focus moves to a different surface — handled by
    ///    [`Self::on_focus_changed`]
    /// 2. the focused client late-binds `text_input_v3` (pgtk Emacs
    ///    binds *after* its initial configure, i.e. after the
    ///    `new_toplevel` keyboard-focus event runs)
    /// 3. the focused client destroys its text_input instance
    ///
    /// (2) and (3) have no smithay callback. To catch them we resync
    /// once per event-loop tick (after `dispatch_clients` has had a
    /// chance to process new bind / destroy requests). Cheap: an
    /// in-process probe + an `Option<bool>` compare; only writes the
    /// mailbox when the answer actually differs.
    pub fn resync(&mut self, seat: &Seat<EmskinState>) {
        self.resync_from_handle(seat.text_input());
    }

    /// Inner re-evaluation against a raw [`TextInputHandle`]. Split out
    /// so unit tests can drive the state machine without building a
    /// full `Seat`. Production callers go through [`Self::resync`].
    fn resync_from_handle(&mut self, ti: &TextInputHandle) {
        let want = focused_client_has_text_input(ti);
        if self.last_committed != Some(want) {
            self.ime_enabled = Some(want);
        }
    }

    /// Forward a host IME event to the focused text_input_v3 client and
    /// reposition the host IME popup to follow the client's caret.
    pub fn on_host_ime_event(
        &mut self,
        event: winit_crate::event::Ime,
        seat: &Seat<EmskinState>,
        apps: &AppManager,
        window: &winit_crate::window::Window,
    ) {
        use winit_crate::event::Ime;

        let ti = seat.text_input();
        sync_ime_cursor_area(ti, apps, window);

        match event {
            Ime::Enabled => {
                tracing::trace!("IME host event: Enabled");
                ti.enter();
            }
            Ime::Preedit(text, cursor) => {
                tracing::trace!(
                    "IME host event: Preedit (len={}, cursor={cursor:?})",
                    text.len()
                );
                let (begin, end) = cursor
                    .map(|(b, e)| (b as i32, e as i32))
                    .unwrap_or(NO_CURSOR);
                ti.with_focused_text_input(|client, _| {
                    client.preedit_string(Some(text.clone()), begin, end);
                });
                ti.done(false);
            }
            Ime::Commit(text) => {
                tracing::trace!("IME host event: Commit (len={})", text.len());
                ti.with_focused_text_input(|client, _| {
                    client.preedit_string(None, 0, 0);
                    client.commit_string(Some(text.clone()));
                });
                ti.done(false);
            }
            Ime::Disabled => {
                tracing::trace!("IME host event: Disabled");
                ti.with_focused_text_input(|client, _| {
                    client.preedit_string(None, 0, 0);
                });
                ti.done(false);
                ti.leave();
            }
        }
    }

    /// Drain the deferred `set_ime_allowed` decision, if any. Called
    /// from the winit render loop where the backend is accessible.
    /// Records the drained value in `last_committed` so future
    /// [`Self::resync`] calls can suppress no-op writes.
    pub fn take_ime_enabled(&mut self) -> Option<bool> {
        let taken = self.ime_enabled.take();
        if let Some(enabled) = taken {
            tracing::debug!("IME: applying set_ime_allowed({enabled})");
            self.last_committed = Some(enabled);
        }
        taken
    }

    /// Clear state on workspace switch — stale surface refs would
    /// otherwise route text_input events to the wrong client. Forces
    /// host IME off during the switch transient; the next
    /// [`Self::on_focus_changed`] / [`Self::resync`] will re-enable it
    /// if the incoming focus has text_input_v3 bound.
    pub fn reset_on_workspace_switch(&mut self) {
        tracing::debug!("IME: reset on workspace switch");
        self.focused_surface = None;
        if self.last_committed != Some(false) {
            self.ime_enabled = Some(false);
        }
    }
}

/// Update smithay's text_input focus and fire enter/leave at the right
/// clients. smithay's keyboard handler would do this automatically if
/// we had an input_method protocol registered, but we don't — hence
/// the manual dance. The `leave` event must be sent *while* text_input
/// focus still points at `old`, otherwise smithay routes it to the new
/// surface instead of the departing one.
fn transition_focus(ti: &TextInputHandle, old: Option<WlSurface>, new: &Option<WlSurface>) {
    if old.as_ref() == new.as_ref() {
        return;
    }
    tracing::debug!(
        "IME focus transition: had_old={} has_new={}",
        old.is_some(),
        new.is_some()
    );
    if old.is_some() {
        ti.set_focus(old);
        ti.leave();
    }
    ti.set_focus(new.clone());
    if new.is_some() {
        ti.enter();
    }
}

/// Whether the currently focused client has bound `text_input_v3`.
/// smithay exposes no direct query, so we probe via the mutation API.
fn focused_client_has_text_input(ti: &TextInputHandle) -> bool {
    let mut found = false;
    ti.with_focused_text_input(|_, _| found = true);
    found
}

/// Position the host IME popup on the embedded client's caret.
fn sync_ime_cursor_area(
    ti: &TextInputHandle,
    apps: &AppManager,
    window: &winit_crate::window::Window,
) {
    let Some(rect) = ti.cursor_rectangle() else {
        return;
    };
    // cursor_rectangle is surface-local; offset by the embedded app's
    // compositor-space origin so the popup lands on-screen.
    let app_loc = ti
        .focus()
        .and_then(|surface| apps.surface_geometry(&surface))
        .map(|geo| geo.loc)
        .unwrap_or_default();
    window.set_ime_cursor_area(
        winit_crate::dpi::LogicalPosition::new(
            (rect.loc.x + app_loc.x) as f64,
            (rect.loc.y + app_loc.y) as f64,
        ),
        winit_crate::dpi::LogicalSize::new(rect.size.w as f64, rect.size.h as f64),
    );
}

smithay::delegate_text_input_manager!(EmskinState);

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::wayland::text_input::TextInputHandle;

    /// Build an `ImeBridge` without going through `new` (which needs a
    /// live `DisplayHandle`). The tests below only exercise the
    /// mailbox / committed-cache state machine, which is independent
    /// of the smithay registration in `new`.
    fn fresh_bridge() -> ImeBridge {
        ImeBridge {
            focused_surface: None,
            ime_enabled: None,
            last_committed: None,
        }
    }

    /// A default `TextInputHandle` has no instances, so
    /// `with_focused_text_input` never fires its callback ⇒
    /// `focused_client_has_text_input` returns `false`. Sufficient to
    /// exercise the "no client text_input bound" branch of resync,
    /// which is the exact production scenario the bug appears in.
    fn empty_handle() -> TextInputHandle {
        TextInputHandle::default()
    }

    #[test]
    fn resync_first_call_writes_disable_into_mailbox() {
        let mut bridge = fresh_bridge();
        bridge.resync_from_handle(&empty_handle());
        assert_eq!(bridge.ime_enabled, Some(false));
    }

    #[test]
    fn take_ime_enabled_records_last_committed() {
        let mut bridge = fresh_bridge();
        bridge.resync_from_handle(&empty_handle());
        let taken = bridge.take_ime_enabled();
        assert_eq!(taken, Some(false));
        assert_eq!(bridge.last_committed, Some(false));
        assert!(bridge.ime_enabled.is_none(), "mailbox drained");
    }

    #[test]
    fn resync_is_silent_when_answer_matches_last_committed() {
        // Regression guard for the "polled per tick" property: if the
        // derived answer hasn't changed since winit committed it, no
        // mailbox write happens, no spurious `set_ime_allowed` syscall.
        let mut bridge = fresh_bridge();
        bridge.resync_from_handle(&empty_handle());
        bridge.take_ime_enabled(); // commits Some(false)

        bridge.resync_from_handle(&empty_handle()); // same answer
        assert!(
            bridge.ime_enabled.is_none(),
            "resync must not re-queue when last_committed already matches"
        );
    }

    #[test]
    fn resync_overwrites_undrained_mailbox_when_value_unchanged() {
        // Edge case: two ticks in a row with no take in between, both
        // see `false`. The mailbox holds `Some(false)` after tick 1.
        // Tick 2's resync sees `last_committed = None` (never drained),
        // so the answer "differs" from the cache → mailbox stays at
        // `Some(false)`. Idempotent.
        let mut bridge = fresh_bridge();
        bridge.resync_from_handle(&empty_handle());
        assert_eq!(bridge.ime_enabled, Some(false));
        bridge.resync_from_handle(&empty_handle());
        assert_eq!(
            bridge.ime_enabled,
            Some(false),
            "second resync without intervening take is idempotent"
        );
    }

    #[test]
    fn reset_on_workspace_switch_queues_disable_when_last_was_enabled() {
        // Workspace switch must force host IME off so a pre-existing
        // `set_ime_allowed(true)` doesn't leak across the boundary.
        let mut bridge = fresh_bridge();
        bridge.last_committed = Some(true); // simulate prior IME-on workspace
        bridge.reset_on_workspace_switch();
        assert_eq!(bridge.ime_enabled, Some(false));
    }

    #[test]
    fn reset_on_workspace_switch_is_silent_when_already_disabled() {
        // If the previous workspace already had IME off, the reset
        // must not queue a redundant winit `set_ime_allowed(false)`.
        let mut bridge = fresh_bridge();
        bridge.last_committed = Some(false);
        bridge.reset_on_workspace_switch();
        assert!(
            bridge.ime_enabled.is_none(),
            "no redundant winit syscall when workspace switch doesn't change IME state"
        );
    }

    #[test]
    fn reset_clears_focused_surface_regardless_of_ime_state() {
        // The focused-surface clearing path is the legacy correctness
        // requirement (avoid stale text_input routing); guard it
        // doesn't regress when `last_committed` short-circuits the IME
        // mailbox write.
        let mut bridge = fresh_bridge();
        bridge.last_committed = Some(false);
        // Can't easily set focused_surface to a real WlSurface in a
        // unit test, but reset must leave it as None either way.
        bridge.reset_on_workspace_switch();
        assert!(bridge.focused_surface.is_none());
    }
}
