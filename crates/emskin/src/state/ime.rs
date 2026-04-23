//! IME (input method) bridge between the host compositor and embedded
//! Wayland clients via `text_input_v3`.
//!
//! Three smithay-imposed constraints drive the design — see
//! `crates/emskin/CLAUDE.md` → IME for the full "why":
//!
//! - `set_ime_allowed` must be toggled per-focused-client (registering `TextInputManagerState` makes fcitx5-gtk abandon its DBus path for text_input_v3, so enabling host IME for a GTK/Qt client that handles its own IM breaks input).
//! - `text_input.enter()/leave()` must be called by hand from `focus_changed` (smithay gates them on `input_method.has_instance()` and emskin implements no input_method protocol).
//! - The `set_ime_allowed` decision is deferred via `ime_enabled` + [`ImeBridge::take_ime_enabled`] (`focus_changed` has no access to the winit backend).

use std::time::{Duration, Instant};

use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::text_input::{TextInputHandle, TextInputManagerState, TextInputSeat};

use crate::apps::AppManager;
use crate::EmskinState;

/// Debounce window for `CursorRect` events following a `FocusIn`.
///
/// pgtk Emacs's GTK IM module fires a burst of `SetCursorRectV2`
/// messages on FocusIn, and at least one of them carries a
/// nonsense position like `[0, 700, 0, 20]` before the real caret
/// coord arrives ~280ms later. Taking the last-one-wins in a tick
/// made the candidate popup flicker to the bottom of the window
/// on every focus change — visible as "飘" when the user switches
/// away and comes back.
///
/// 100ms is long enough to cover the burst (in our logs it was
/// all within ~10ms of FocusIn but the corrective value came back
/// ~300ms later, meaning the bad values linger visibly), and short
/// enough not to notice in normal typing (keystroke intervals are
/// typically > 150ms).
const FOCUS_IN_CURSOR_RECT_SETTLE: Duration = Duration::from_millis(300);

/// `(-1, -1)` sentinel per text_input_v3 for "no cursor position".
const NO_CURSOR: (i32, i32) = (-1, -1);

/// Identifier for an fcitx5 input context the broker has allocated,
/// plus the client's emskin-space origin captured at `FocusIn` time.
///
/// Pinning the origin here (instead of re-reading it per event from
/// `keyboard.current_focus()`) avoids two race bugs:
///
/// 1. `CursorRect` events that arrive in the same tick as a focus
///    switch would otherwise pick up the *new* focus's origin, not
///    the IC that actually sent them. That made Emacs's popup "drift"
///    after switching windows and back — the CursorRect events from
///    Emacs's IC were being translated with WeChat's (or whatever)
///    origin.
/// 2. Multiple DBus clients' events in one tick all used to share
///    the single origin computed at drain time.
///
/// Origin is refreshed on the next `FocusIn` (a FocusOut+FocusIn
/// cycle re-captures the latest position), so drag-during-typing on
/// a single IC is the only remaining stale case — acceptable.
#[derive(Debug, Clone)]
pub struct ActiveFcitxIc {
    pub conn: crate::dbus_broker::ConnId,
    pub ic_path: String,
    pub origin: [i32; 2],
    /// When `FocusIn` staged this IC. Used to suppress the burst of
    /// `CursorRect` events GTK IM fires immediately after focusing —
    /// see [`FOCUS_IN_CURSOR_RECT_SETTLE`].
    pub activated_at: Instant,
}

impl PartialEq for ActiveFcitxIc {
    fn eq(&self, other: &Self) -> bool {
        // `activated_at` is an Instant — excluded from equality so
        // test fixtures comparing paired (conn, ic_path) via `==`
        // don't have to construct matching timestamps.
        self.conn == other.conn && self.ic_path == other.ic_path && self.origin == other.origin
    }
}

impl Eq for ActiveFcitxIc {}

pub struct ImeBridge {
    focused_surface: Option<WlSurface>,
    /// True iff the currently focused surface has `zwp_text_input_v3`
    /// bound (Chrome with `--enable-wayland-ime`, alacritty, …).
    /// Written by [`Self::on_focus_changed`].
    tip_wants_ime: bool,
    /// True iff a fcitx5 DBus IC is currently focused (the broker saw
    /// `InputContext1.FocusIn` and no matching `FocusOut` yet).
    /// Written by [`Self::on_fcitx_event`].
    dbus_wants_ime: bool,
    /// Staged `set_ime_allowed` decision for the render loop. `None`
    /// when the combined (`tip || dbus`) state hasn't changed since
    /// last drain. Before M3 this was a single `Option<bool>` written
    /// by both paths — the two writers raced and whichever wrote last
    /// clobbered the other, so e.g. the DBus path's `Some(false)` on
    /// FocusOut would disable IME even though text_input_v3 still
    /// wanted it on for alacritty.
    pending_ime_enabled: Option<bool>,
    /// Last combined state we staged, used to decide whether to
    /// re-stage. The `pending_ime_enabled` gets taken every render
    /// frame, so without this we'd thrash — re-staging the same value
    /// every event.
    last_staged_ime_enabled: bool,
    /// Fcitx5 IC currently focused (via broker-observed `FocusIn`).
    /// When winit emits an IME event (`Preedit` / `Commit`) we look up
    /// this IC to decide which DBus client to forward the result to.
    /// At most one IC is active at a time — `FocusIn` on a new IC
    /// evicts the previous one.
    active_fcitx_ic: Option<ActiveFcitxIc>,
    /// Cursor area waiting for the render loop to call
    /// `window.set_ime_cursor_area`. Drained by
    /// [`ImeBridge::take_pending_cursor_area`]. Coords are
    /// **emskin-winit-local** (`active.origin + client_rect`) so
    /// the render loop can hand them straight to winit.
    pending_cursor_area: Option<([i32; 2], [i32; 2])>,
}

impl ImeBridge {
    pub fn new(dh: &DisplayHandle) -> Self {
        // The global is owned by `Display` after registration; the
        // returned `TextInputManagerState` (a bare `GlobalId` wrapper)
        // has no Drop impl that unregisters, so dropping it is a no-op.
        let _ = TextInputManagerState::new::<EmskinState>(dh);
        Self {
            focused_surface: None,
            tip_wants_ime: false,
            dbus_wants_ime: false,
            pending_ime_enabled: None,
            last_staged_ime_enabled: false,
            active_fcitx_ic: None,
            pending_cursor_area: None,
        }
    }

    /// Recompute the combined IME-enabled decision from the two
    /// independent sources. Stages a pending value only when the
    /// combined `tip || dbus` state actually changed — prevents
    /// thrashing winit with redundant `set_ime_allowed` calls.
    fn refresh_ime_enabled(&mut self) {
        let want = self.tip_wants_ime || self.dbus_wants_ime;
        if want != self.last_staged_ime_enabled {
            self.pending_ime_enabled = Some(want);
            self.last_staged_ime_enabled = want;
            tracing::debug!(
                tip = self.tip_wants_ime,
                dbus = self.dbus_wants_ime,
                "IME: set_ime_allowed({want}) staged"
            );
        }
    }

    /// Current fcitx5 IC, if any, that winit IME events should be
    /// forwarded to.
    pub fn active_fcitx_ic(&self) -> Option<&ActiveFcitxIc> {
        self.active_fcitx_ic.as_ref()
    }

    /// Drain the pending `set_ime_cursor_area` call. Called by the
    /// winit render loop where the backend is accessible. `(position,
    /// size)` in emskin-winit-local coords (`i32` × 2 + `i32` × 2).
    pub fn take_pending_cursor_area(&mut self) -> Option<([i32; 2], [i32; 2])> {
        self.pending_cursor_area.take()
    }

    /// Process a [`crate::dbus_broker::FcitxEvent`] observed by the
    /// broker. Updates `active_fcitx_ic` + pins its origin, stages a
    /// `set_ime_cursor_area` and `set_ime_allowed` change for the
    /// winit render loop to apply.
    ///
    /// `app_origin` is the emskin-space origin of whichever app is
    /// focused at the tick this event was drained — used only for
    /// `FocusChanged { focused: true }` events, where it's snapshot
    /// onto the `ActiveFcitxIc` so subsequent `CursorRect` events on
    /// this IC translate against the *right* origin even if the user
    /// switches to a different app (whose DBus events land in the
    /// same tick).
    pub fn on_fcitx_event(
        &mut self,
        event: crate::dbus_broker::FcitxEvent,
        app_origin: Option<[i32; 2]>,
    ) {
        use crate::dbus_broker::FcitxEvent;

        match event {
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: true,
                rect,
            } => {
                let origin = app_origin.unwrap_or([0, 0]);
                tracing::info!(
                    ?conn,
                    ?ic_path,
                    ?origin,
                    ?rect,
                    "fcitx IC FocusIn → activating winit IME"
                );
                self.active_fcitx_ic = Some(ActiveFcitxIc {
                    conn,
                    ic_path,
                    origin,
                    activated_at: Instant::now(),
                });
                self.dbus_wants_ime = true;
                self.refresh_ime_enabled();
                if let Some(r) = rect {
                    self.pending_cursor_area = Some((
                        [origin[0] + r[0], origin[1] + r[1]],
                        [r[2].max(1), r[3].max(1)],
                    ));
                }
            }
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: false,
                ..
            } => {
                // Only clear if the unfocused IC is the active one.
                // Spurious FocusOut on a stale IC mustn't kick out the
                // currently-active client.
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    tracing::debug!(?conn, ?ic_path, "fcitx IC FocusOut → deactivating winit IME");
                    self.active_fcitx_ic = None;
                    self.dbus_wants_ime = false;
                    self.refresh_ime_enabled();
                }
            }
            FcitxEvent::CursorRect {
                conn,
                ic_path,
                rect,
            } => {
                let Some(active) = self.active_fcitx_ic.as_ref() else {
                    tracing::debug!(?conn, ?ic_path, "CursorRect ignored: no active IC");
                    return;
                };
                if active.conn != conn || active.ic_path != ic_path {
                    tracing::debug!(
                        ?conn,
                        ?ic_path,
                        active_conn = ?active.conn,
                        active_ic = active.ic_path,
                        "CursorRect ignored: not the active IC"
                    );
                    return;
                }
                // Debounce the initial burst of CursorRect events
                // GTK IM fires on FocusIn — see FOCUS_IN_CURSOR_RECT_SETTLE.
                let since_focus = active.activated_at.elapsed();
                if since_focus < FOCUS_IN_CURSOR_RECT_SETTLE {
                    tracing::debug!(
                        ?conn,
                        ?ic_path,
                        client_rect = ?rect,
                        since_focus_ms = since_focus.as_millis(),
                        "CursorRect ignored: within FocusIn settle window"
                    );
                    return;
                }
                // Use the origin captured at FocusIn — NOT the current
                // keyboard focus's origin. See the `ActiveFcitxIc` doc.
                let pos = [active.origin[0] + rect[0], active.origin[1] + rect[1]];
                let size = [rect[2].max(1), rect[3].max(1)];
                tracing::info!(
                    ?conn,
                    ?ic_path,
                    client_rect = ?rect,
                    origin = ?active.origin,
                    ?pos,
                    ?size,
                    "fcitx IC CursorRect → staging winit set_ime_cursor_area"
                );
                self.pending_cursor_area = Some((pos, size));
            }
            FcitxEvent::IcDestroyed { conn, ic_path } => {
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    self.active_fcitx_ic = None;
                    self.dbus_wants_ime = false;
                    self.refresh_ime_enabled();
                }
            }
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
        let has_tip = focused_client_has_text_input(ti);
        tracing::debug!(
            "IME focus_changed: has_focus={} tip_wants_ime={has_tip}",
            new_focus.is_some()
        );
        self.focused_surface = new_focus;
        self.tip_wants_ime = has_tip;
        self.refresh_ime_enabled();
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
        let taken = self.pending_ime_enabled.take();
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
        self.tip_wants_ime = false;
        self.dbus_wants_ime = false;
        self.active_fcitx_ic = None;
        self.pending_cursor_area = None;
        // Force re-stage of `false` even if last_staged was already
        // false — workspace-switch expects an explicit IME off on the
        // winit side.
        self.pending_ime_enabled = Some(false);
        self.last_staged_ime_enabled = false;
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
