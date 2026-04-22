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
        }
    }

    /// Bridge text_input enter/leave on keyboard focus change and decide
    /// whether host IME should be enabled for the new focus.
    ///
    /// `new_focus` is the focused surface projected from
    /// `KeyboardFocusTarget` via `WaylandFocus::wl_surface()` — X clients
    /// surface here too once associated by xwayland-satellite.
    pub fn on_focus_changed(&mut self, seat: &Seat<EmskinState>, new_focus: Option<WlSurface>) {
        let ti = seat.text_input();
        let old = self.focused_surface.take();
        transition_focus(ti, old, &new_focus);
        let enabled = focused_client_has_text_input(ti);
        tracing::debug!(
            "IME focus_changed: has_focus={} ime_enabled={enabled}",
            new_focus.is_some()
        );
        self.focused_surface = new_focus;
        self.ime_enabled = Some(enabled);
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
    pub fn take_ime_enabled(&mut self) -> Option<bool> {
        let taken = self.ime_enabled.take();
        if let Some(enabled) = taken {
            tracing::debug!("IME: applying set_ime_allowed({enabled})");
        }
        taken
    }

    /// Clear state on workspace switch — stale surface refs would
    /// otherwise route text_input events to the wrong client. The
    /// `Some(false)` pending decision also disables host IME during the
    /// switch transient; the next `on_focus_changed` will re-enable it
    /// if the incoming focus has text_input_v3 bound.
    pub fn reset_on_workspace_switch(&mut self) {
        tracing::debug!("IME: reset on workspace switch");
        self.focused_surface = None;
        self.ime_enabled = Some(false);
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
