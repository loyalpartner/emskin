//! IME (input method) bridge — unified state store for both the
//! text_input_v3 path (Wayland-native clients) and the DBus fcitx5
//! frontend path (`GTK_IM_MODULE=fcitx` clients).
//!
//! # Design
//!
//! Three smithay-imposed constraints from the text_input_v3 side:
//!
//! - `set_ime_allowed` must be toggled per-focused-client (registering
//!   `TextInputManagerState` makes fcitx5-gtk abandon its DBus path for
//!   text_input_v3, so enabling host IME for a GTK/Qt client that
//!   handles its own IM breaks input).
//! - `text_input.enter()/leave()` must be called by hand from
//!   `focus_changed` (smithay gates them on `input_method.has_instance()`
//!   and emskin implements no input_method protocol).
//! - `focus_changed` cannot access the winit backend, so any winit
//!   side-effect has to be deferred to the render loop.
//!
//! The bridge stores **state**, not **events**. Specifically:
//!
//! ```text
//! desired winit state  =  f(active_fcitx_ic, tip_wants_ime)
//! ```
//!
//! Every render frame, [`ImeBridge::sync_to_winit`] recomputes `f` and
//! pushes only the diff to winit. This is robust to event
//! interleaving: e.g. when a client loses focus, its `ActiveFcitxIc`
//! is dropped, which naturally means "no cursor area" without an
//! explicit clear — the next `sync_to_winit` observes the state has
//! changed and acts accordingly.
//!
//! The older design accumulated `pending_ime_enabled: Option<bool>` /
//! `pending_cursor_area: Option<([i32;2], [i32;2])>` independently per
//! event. That made it easy to leak one IC's cursor position into
//! another's activation (the "popup appears at the previous app's
//! offset when switching back" bug) because the pending field stayed
//! populated across IC changes.
//!
//! # Two-sources-of-IME-demand gotcha
//!
//! `set_ime_allowed(true)` is wanted iff **either** path has demand:
//! `tip_wants_ime || active_fcitx_ic.is_some()`. Writing this as the
//! logical OR of two independent flags stops the DBus path's
//! `FocusOut` from disabling IME while an alacritty window (via
//! text_input_v3) still wants it on, which caused "alacritty
//! Ctrl+Space can't toggle IME" when both paths fought over a single
//! `Option<bool>` slot.

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
/// messages on FocusIn, some of which carry stale / nonsense
/// positions before the real caret coord arrives ~280ms later. We
/// accept the *first* CursorRect in the burst (which agrees with
/// FocusIn's rect, if any — both describe the same caret moment) and
/// debounce the rest until the settle window closes; that stops the
/// tail-of-burst garbage from being the last value winit sees.
const FOCUS_IN_CURSOR_RECT_SETTLE: Duration = Duration::from_millis(300);

/// `(-1, -1)` sentinel per text_input_v3 for "no cursor position".
const NO_CURSOR: (i32, i32) = (-1, -1);

/// State of the fcitx5 input context currently driving the DBus IME
/// path. Present iff we've seen a `FocusIn` that hasn't been matched
/// by a `FocusOut` / `DestroyIC` / workspace switch reset.
#[derive(Debug, Clone)]
pub struct ActiveFcitxIc {
    pub conn: crate::dbus_broker::ConnId,
    pub ic_path: String,
    /// Emskin-space origin of the app that owns this IC. Captured
    /// once at `FocusIn` and preserved across events — even if
    /// emskin's keyboard focus later drifts to another surface, the
    /// IC's own CursorRect events still translate against the
    /// original origin.
    pub origin: [i32; 2],
    /// Last client-reported caret rect in **client-surface-local**
    /// coordinates. `None` until the first `CursorRect` event (or the
    /// `rect` field of `FocusIn`) arrives.
    pub current_rect: Option<[i32; 4]>,
    /// When `FocusIn` staged this IC. Used by the CursorRect debounce
    /// — see [`FOCUS_IN_CURSOR_RECT_SETTLE`].
    pub activated_at: Instant,
    /// `true` once we've accepted at least one `CursorRect` for this
    /// IC. Lets the *first* CursorRect bypass the debounce window
    /// (needed when `FocusIn.rect` was `None`) while later ones in
    /// the same burst still get dropped.
    pub cursor_rect_received: bool,
}

pub struct ImeBridge {
    /// Currently keyboard-focused Wayland surface, from smithay's
    /// seat. Used by [`Self::on_focus_changed`] to decide text_input
    /// enter/leave semantics.
    focused_surface: Option<WlSurface>,
    /// True iff `focused_surface` has `zwp_text_input_v3` bound.
    tip_wants_ime: bool,
    /// DBus fcitx5 frontend state. `None` = no IC active.
    active_fcitx_ic: Option<ActiveFcitxIc>,
    /// Cursor area the text_input_v3 path wants pushed to winit,
    /// computed from `ti.cursor_rectangle()` + the focused client's
    /// emskin-space origin. Refreshed in [`Self::on_host_ime_event`]
    /// and cleared on every focus change so a previous
    /// text_input_v3 client's rect never lingers past its focus —
    /// the critical fix for "alacritty → Emacs, popup jumps to
    /// alacritty's relative cursor position".
    tip_cursor_area: Option<([i32; 2], [i32; 2])>,

    /// What we last told winit via `set_ime_allowed`. Used to diff
    /// against the current desired state and only emit the call when
    /// it actually changed.
    last_applied_ime_allowed: bool,
    /// What we last told winit via `set_ime_cursor_area`. Same
    /// diffing purpose.
    last_applied_cursor_area: Option<([i32; 2], [i32; 2])>,
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
            active_fcitx_ic: None,
            tip_cursor_area: None,
            last_applied_ime_allowed: false,
            last_applied_cursor_area: None,
        }
    }

    /// Current fcitx5 IC, if any, that winit IME events should be
    /// forwarded to.
    pub fn active_fcitx_ic(&self) -> Option<&ActiveFcitxIc> {
        self.active_fcitx_ic.as_ref()
    }

    /// Desired `set_ime_allowed` state: logical OR of the two
    /// independent sources. See the module-level doc for why this
    /// isn't a single shared flag.
    fn desired_ime_allowed(&self) -> bool {
        self.tip_wants_ime || self.active_fcitx_ic.is_some()
    }

    /// Desired `set_ime_cursor_area` value in emskin-winit-local
    /// coords, computed from whichever IME path has demand.
    ///
    /// DBus fcitx5 (`active_fcitx_ic`) takes precedence over
    /// text_input_v3 (`tip_cursor_area`) because a DBus IC's
    /// presence means a client actively FocusIn'd via that channel —
    /// any text_input_v3 state from a now-defocused alacritty
    /// lingering in `ti.cursor_rectangle()` would otherwise override
    /// the DBus client's real caret.
    ///
    /// With a DBus IC active but `current_rect` still `None`
    /// (FocusIn came without a rect and no CursorRect has arrived
    /// yet), falls back to `(origin, 1×1)` — safer than returning
    /// `None`, which would leave winit's cache holding the previous
    /// IC's position until the first real CursorRect overwrites it.
    fn desired_cursor_area(&self) -> Option<([i32; 2], [i32; 2])> {
        if let Some(active) = self.active_fcitx_ic.as_ref() {
            return Some(match active.current_rect {
                Some(rect) => (
                    [active.origin[0] + rect[0], active.origin[1] + rect[1]],
                    [rect[2].max(1), rect[3].max(1)],
                ),
                None => (active.origin, [1, 1]),
            });
        }
        self.tip_cursor_area
    }

    /// Sync the bridge's current state to winit. Called by the render
    /// loop's `apply_pending_state` every frame.
    ///
    /// Two ordering rules matter:
    ///
    /// 1. **cursor area is set before IME allowed.** When a newly
    ///    focused client activates IME, the host compositor will
    ///    position the popup based on whatever cursor rect winit
    ///    has cached at the moment `enable` fires — so we push the
    ///    fresh position *first*, then flip the activation.
    ///
    /// 2. **force-push cursor area when IME transitions off→on.**
    ///    Even if `desired_cursor_area` equals
    ///    `last_applied_cursor_area` by value, the host compositor
    ///    may have dropped its own cache during the disabled
    ///    window, or winit might batch differently. A redundant
    ///    `set_ime_cursor_area` is cheap; the alternative is the
    ///    popup showing at the previous client's position for one
    ///    frame.
    pub fn sync_to_winit(&mut self, window: &winit_crate::window::Window) {
        let want_allowed = self.desired_ime_allowed();
        let activating = want_allowed && !self.last_applied_ime_allowed;

        if let Some((pos, size)) = self.desired_cursor_area() {
            let changed = self.last_applied_cursor_area != Some((pos, size));
            if changed || activating {
                window.set_ime_cursor_area(
                    winit_crate::dpi::LogicalPosition::new(pos[0] as f64, pos[1] as f64),
                    winit_crate::dpi::LogicalSize::new(size[0] as f64, size[1] as f64),
                );
                tracing::info!(
                    reason = if activating { "activating" } else { "changed" },
                    "winit.set_ime_cursor_area({}, {}, {}, {})",
                    pos[0], pos[1], size[0], size[1]
                );
                self.last_applied_cursor_area = Some((pos, size));
            }
        }

        if want_allowed != self.last_applied_ime_allowed {
            window.set_ime_allowed(want_allowed);
            tracing::info!("winit.set_ime_allowed({want_allowed})");
            self.last_applied_ime_allowed = want_allowed;
        }
    }

    /// Process a [`crate::dbus_broker::FcitxEvent`]. Pure state
    /// mutation — the winit-facing side-effects happen on the next
    /// [`Self::sync_to_winit`] call.
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
                    current_rect: rect,
                    activated_at: Instant::now(),
                    cursor_rect_received: rect.is_some(),
                });
            }
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: false,
                ..
            } => {
                // Only clear if the unfocused IC is the active one —
                // spurious FocusOut on a stale IC mustn't kick out
                // the currently-active client.
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    tracing::debug!(?conn, ?ic_path, "fcitx IC FocusOut → deactivating winit IME");
                    self.active_fcitx_ic = None;
                }
            }
            FcitxEvent::CursorRect {
                conn,
                ic_path,
                rect,
            } => {
                let Some(active) = self.active_fcitx_ic.as_mut() else {
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
                // Always accept the first CursorRect after FocusIn —
                // that's how we pick up the real caret position when
                // FocusIn.rect was None. Subsequent CursorRects
                // within the settle window get dropped (GTK IM's
                // post-FocusIn burst contains stale / nonsense values
                // that would otherwise become last-write-wins).
                let since_focus = active.activated_at.elapsed();
                let in_settle = since_focus < FOCUS_IN_CURSOR_RECT_SETTLE;
                if active.cursor_rect_received && in_settle {
                    tracing::debug!(
                        ?conn,
                        ?ic_path,
                        client_rect = ?rect,
                        since_focus_ms = since_focus.as_millis(),
                        "CursorRect debounced: within FocusIn settle window"
                    );
                    return;
                }
                tracing::info!(
                    ?conn,
                    ?ic_path,
                    client_rect = ?rect,
                    origin = ?active.origin,
                    "fcitx IC CursorRect → updating IC state"
                );
                active.current_rect = Some(rect);
                active.cursor_rect_received = true;
            }
            FcitxEvent::IcDestroyed { conn, ic_path } => {
                if self
                    .active_fcitx_ic
                    .as_ref()
                    .is_some_and(|a| a.conn == conn && a.ic_path == ic_path)
                {
                    self.active_fcitx_ic = None;
                }
            }
        }
    }

    /// Bridge text_input enter/leave on keyboard focus change and
    /// update the `tip_wants_ime` flag that feeds into
    /// `desired_ime_allowed`.
    ///
    /// `new_focus` is the focused surface projected from
    /// `KeyboardFocusTarget` via `WaylandFocus::wl_surface()` — X
    /// clients surface here too once associated by xwayland-satellite.
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
        // Clear the text_input_v3 path's cached cursor area — the
        // previous client's rect must not linger past its focus.
        // smithay's TextInputHandle doesn't auto-clear
        // `cursor_rectangle` on focus change, so without this, a
        // stale alacritty rect would still pollute
        // `desired_cursor_area` after switching away.
        self.tip_cursor_area = None;
    }

    /// Forward a host IME event to the focused text_input_v3 client
    /// (Wayland-native path). DBus-fcitx5 side is handled by the
    /// broker's `emit_commit_string` / `emit_preedit`.
    ///
    /// Also refreshes the text_input_v3 path's `tip_cursor_area` so
    /// the next [`Self::sync_to_winit`] picks up the current rect
    /// (only when the focused client actually bound
    /// `zwp_text_input_v3`). Skipping this for non-text_input_v3
    /// clients is what stops a previously-focused alacritty's rect
    /// from overriding the DBus path's position for Emacs.
    pub fn on_host_ime_event(
        &mut self,
        event: winit_crate::event::Ime,
        seat: &Seat<EmskinState>,
        apps: &AppManager,
        _window: &winit_crate::window::Window,
    ) {
        use winit_crate::event::Ime;

        let ti = seat.text_input();
        if self.tip_wants_ime {
            self.tip_cursor_area = compute_tip_cursor_area(ti, apps);
        } else {
            // Focused client doesn't bind text_input_v3 — DBus path
            // owns cursor_area if anything does. Drop any stale value.
            self.tip_cursor_area = None;
        }

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

    /// Clear state on workspace switch — stale surface refs would
    /// otherwise route text_input events to the wrong client.
    pub fn reset_on_workspace_switch(&mut self) {
        tracing::debug!("IME: reset on workspace switch");
        self.focused_surface = None;
        self.tip_wants_ime = false;
        self.tip_cursor_area = None;
        self.active_fcitx_ic = None;
        // Don't reset `last_applied_*` — next `sync_to_winit` diffs
        // against actual state and will push whatever the new
        // workspace demands.
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

/// Compute the cursor area for a text_input_v3-bound focused client,
/// from `ti.cursor_rectangle()` (client-surface-local) + the
/// client's emskin-space origin. Returns `None` if there's no rect
/// yet or the focused surface isn't tracked as an embedded app.
///
/// Caller must only invoke this when the focused client actually
/// bound text_input_v3 — smithay's `TextInputHandle.cursor_rectangle`
/// doesn't clear on focus change, so calling this for a non-
/// text_input_v3 client returns the previous client's stale rect.
fn compute_tip_cursor_area(
    ti: &TextInputHandle,
    apps: &AppManager,
) -> Option<([i32; 2], [i32; 2])> {
    let rect = ti.cursor_rectangle()?;
    let app_loc = ti
        .focus()
        .and_then(|surface| apps.surface_geometry(&surface))
        .map(|geo| geo.loc)
        .unwrap_or_default();
    Some((
        [rect.loc.x + app_loc.x, rect.loc.y + app_loc.y],
        [rect.size.w.max(1), rect.size.h.max(1)],
    ))
}

// Allow `ActiveFcitxIc` equality in tests to ignore runtime-transient
// fields. Used by a handful of consumers that want to assert "same
// IC identity" without constructing matching Instants.
impl PartialEq for ActiveFcitxIc {
    fn eq(&self, other: &Self) -> bool {
        self.conn == other.conn && self.ic_path == other.ic_path && self.origin == other.origin
    }
}

impl Eq for ActiveFcitxIc {}

smithay::delegate_text_input_manager!(EmskinState);
