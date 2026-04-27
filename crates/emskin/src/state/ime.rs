//! IME (input method) bridge — unified state machine for both the
//! text_input_v3 path (Wayland-native clients) and the DBus fcitx5
//! frontend path (`GTK_IM_MODULE=fcitx` clients).
//!
//! # Mental model
//!
//! At any moment IME has **one owner** — the source currently asking
//! for IM:
//!
//! ```text
//! enum ImeOwner {
//!     None,                                  // nobody wants IM
//!     Tip  { surface }                       // a v3-bound client
//!     Dbus { conn, ic_path, origin }         // a fcitx5 DBus IC
//! }
//! ```
//!
//! Plus one **override**: `prefix_active` (Emacs C-x/C-c/M-x chord)
//! forces IME off so the chord reaches Emacs cleanly.
//!
//! Decision is trivial:
//!
//! ```text
//! desired_ime_allowed = !prefix_active && match owner {
//!     None       => false,
//!     Tip        => true,                 // fallback area OK
//!     Dbus       => cursor.is_real,       // wait for client's first SetCursorRect
//! }
//! ```
//!
//! The DBus gate avoids popup-at-host-(0,0) flicker (text_input_v3.enable
//! resets state, then we push our cursor; if no cursor, host fcitx5
//! uses default). For Tip we activate immediately so Ctrl+Space mode
//! switching works even before the client has reported its caret.
//!
//! # Cursor caching
//!
//! Per-owner `cursor_cache` (keyed by owner identity) stores the last
//! client-reported caret in **client-surface-local** coords. Restored
//! on refocus so e.g. Alacritty → Emacs → Alacritty snaps the popup
//! back to where it was, instead of flashing through the
//! surface-origin fallback.
//!
//! # Two-sources-of-IME-demand gotcha (preserved)
//!
//! Tip and DBus paths are mutually exclusive (one owner at a time).
//! Conceptually the "OR" of demands is the owner being non-None.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::text_input::{TextInputHandle, TextInputManagerState, TextInputSeat};

use crate::apps::AppManager;
use crate::dbus_broker::ConnId;
use crate::EmskinState;

/// Debounce window for `CursorRect` events following a DBus `FocusIn`.
/// pgtk Emacs's GTK IM module fires a burst of `SetCursorRectV2`
/// messages on FocusIn, some carrying stale positions before the real
/// caret coord arrives ~280ms later. The first burst entry is accepted;
/// the rest are dropped until the settle window closes.
const FOCUS_IN_CURSOR_RECT_SETTLE: Duration = Duration::from_millis(300);

/// `(-1, -1)` sentinel per text_input_v3 for "no cursor position".
const NO_CURSOR: (i32, i32) = (-1, -1);

/// Who currently wants IME, if anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeOwner {
    None,
    /// Wayland-native text_input_v3 client (Alacritty, Chromium with
    /// `--enable-wayland-ime`).
    Tip {
        surface: WlSurface,
    },
    /// fcitx5 DBus client (WeChat, Electron, pgtk Emacs). `origin` is
    /// the embedded app's emskin-space top-left, captured at FocusIn
    /// — preserved across cursor events even if keyboard focus drifts.
    Dbus {
        conn: ConnId,
        ic_path: String,
        origin: [i32; 2],
    },
}

/// Cache key derived from owner identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CursorCacheKey {
    Tip(WlSurface),
    Dbus(ConnId, String),
}

impl ImeOwner {
    /// Same identity (ignoring origin / cosmetic fields)? Used to
    /// detect "set_owner with the same owner" and skip cache reset.
    fn same_identity_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::Tip { surface: a }, Self::Tip { surface: b }) => a == b,
            (
                Self::Dbus {
                    conn: ac,
                    ic_path: ap,
                    ..
                },
                Self::Dbus {
                    conn: bc,
                    ic_path: bp,
                    ..
                },
            ) => ac == bc && ap == bp,
            _ => false,
        }
    }
}

pub struct ImeBridge {
    /// Currently keyboard-focused Wayland surface (independent of IME
    /// ownership — needed for text_input enter/leave plumbing even
    /// when no client wants IME).
    focused_surface: Option<WlSurface>,

    /// Who currently wants IME.
    owner: ImeOwner,

    /// Caret area in **emskin-winit-local** coords (already translated
    /// from client-surface-local by the owner's origin). `None` =
    /// owner is None or we haven't been able to compute one.
    cursor: Option<([i32; 2], [i32; 2])>,

    /// True iff `cursor` is from a real client report (not the
    /// surface-origin fallback). Used to gate `desired_ime_allowed`
    /// for the DBus path so we don't flicker popup at (0, 0) before
    /// the client's first SetCursorRect lands.
    cursor_is_real: bool,

    /// Per-owner cache of the last accepted client-local rect.
    /// Restored on refocus to a known owner.
    cursor_cache: HashMap<CursorCacheKey, Rectangle<i32, Logical>>,

    /// Tip-path: snapshot of `ti.cursor_rectangle()` taken at the
    /// moment we became Tip-owned. `TextInputHandle.cursor_rectangle`
    /// is per-seat (not per-surface), so when client A sets it then
    /// loses focus, client B inherits A's value. We treat ti's value
    /// as fresh only when it differs from this snapshot.
    tip_snapshot: Option<Rectangle<i32, Logical>>,

    /// DBus-path: when the current Dbus owner was set. Used to
    /// debounce GTK fcitx-gtk's post-FocusIn CursorRect burst.
    dbus_focused_at: Option<Instant>,

    /// DBus-path: have we accepted a CursorRect for the current
    /// Dbus owner yet? First one bypasses the settle window;
    /// subsequent ones inside the window are dropped.
    dbus_cursor_received: bool,

    /// Override: Emacs prefix chord (C-x/C-c/M-x) forces IME off so
    /// continuation keys reach Emacs and any half-typed preedit is
    /// cancelled.
    prefix_active: bool,

    /// What we last told winit via `set_ime_allowed`.
    last_applied_ime_allowed: bool,
    /// What we last told winit via `set_ime_cursor_area`.
    last_applied_cursor_area: Option<([i32; 2], [i32; 2])>,
}

impl ImeBridge {
    pub fn new(dh: &DisplayHandle) -> Self {
        // Global registration; the returned wrapper has no Drop, so
        // dropping it is a no-op.
        let _ = TextInputManagerState::new::<EmskinState>(dh);
        Self {
            focused_surface: None,
            owner: ImeOwner::None,
            cursor: None,
            cursor_is_real: false,
            cursor_cache: HashMap::new(),
            tip_snapshot: None,
            dbus_focused_at: None,
            dbus_cursor_received: false,
            prefix_active: false,
            last_applied_ime_allowed: false,
            last_applied_cursor_area: None,
        }
    }

    /// Public read-only view of the currently-active DBus IC, for
    /// `winit.rs` to route `Ime::Preedit` / `Ime::Commit` events back
    /// over the DBus broker.
    pub fn active_dbus_ic(&self) -> Option<(ConnId, &str)> {
        match &self.owner {
            ImeOwner::Dbus { conn, ic_path, .. } => Some((*conn, ic_path.as_str())),
            _ => None,
        }
    }

    fn desired_ime_allowed(&self) -> bool {
        if self.prefix_active {
            return false;
        }
        match &self.owner {
            ImeOwner::None => false,
            // Tip activates immediately — Ctrl+Space toggling needs
            // host fcitx5 to grab keys even before the client reports
            // its caret. Cursor area uses fallback (surface origin)
            // until the client sends a fresh value.
            ImeOwner::Tip { .. } => true,
            // DBus waits for the client's first SetCursorRect — GTK
            // fcitx-gtk reliably sends within 1-2 frames of FocusIn.
            // Without this gate, the popup would flicker at host (0, 0)
            // (text_input_v3.enable resets cursor state to default).
            ImeOwner::Dbus { .. } => self.cursor_is_real,
        }
    }

    fn desired_cursor_area(&self) -> Option<([i32; 2], [i32; 2])> {
        self.cursor
    }

    /// Sync state to winit. Called from the render loop.
    ///
    /// Order matters: `set_ime_allowed(true)` MUST come before
    /// `set_ime_cursor_area`, because `text_input_v3.enable` resets
    /// state — pushing the cursor first would have it wiped by enable.
    /// On activation (off → on) we force-push cursor even if value
    /// hasn't changed, so it lands after enable's reset.
    pub fn sync_to_winit(&mut self, window: &winit_crate::window::Window) {
        let want_allowed = self.desired_ime_allowed();
        let activating = want_allowed && !self.last_applied_ime_allowed;

        if want_allowed != self.last_applied_ime_allowed {
            window.set_ime_allowed(want_allowed);
            tracing::info!("winit.set_ime_allowed({want_allowed})");
            self.last_applied_ime_allowed = want_allowed;
        }

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
                    pos[0],
                    pos[1],
                    size[0],
                    size[1]
                );
                self.last_applied_cursor_area = Some((pos, size));
            }
        }
    }

    // ----- Owner transitions -----

    fn set_owner(&mut self, new: ImeOwner, ti: &TextInputHandle, apps: &AppManager) {
        // No-op if same identity (e.g. set_focus(SAME) from C-x b/o IPC).
        if self.owner.same_identity_as(&new) {
            return;
        }
        self.owner = new;
        self.cursor = None;
        self.cursor_is_real = false;
        self.tip_snapshot = None;
        self.dbus_focused_at = None;
        self.dbus_cursor_received = false;

        match self.owner.clone() {
            ImeOwner::None => {}
            ImeOwner::Tip { ref surface } => {
                self.tip_snapshot = ti.cursor_rectangle();
                // Try cache first; fall back to surface origin.
                let cached = self
                    .cursor_cache
                    .get(&CursorCacheKey::Tip(surface.clone()))
                    .copied();
                if let Some(rect) = cached {
                    self.cursor = Some(translate(rect, app_loc(Some(surface), apps)));
                    self.cursor_is_real = true;
                } else {
                    self.cursor = Some((app_loc(Some(surface), apps), [1, 1]));
                    // cursor_is_real stays false — fallback only.
                }
            }
            ImeOwner::Dbus {
                ref conn,
                ref ic_path,
                origin,
            } => {
                self.dbus_focused_at = Some(Instant::now());
                let cached = self
                    .cursor_cache
                    .get(&CursorCacheKey::Dbus(*conn, ic_path.clone()))
                    .copied();
                if let Some(rect) = cached {
                    self.cursor = Some((
                        [origin[0] + rect.loc.x, origin[1] + rect.loc.y],
                        [rect.size.w.max(1), rect.size.h.max(1)],
                    ));
                    self.cursor_is_real = true;
                    // First real CursorRect from client should still
                    // bypass the debounce, so don't flip
                    // `dbus_cursor_received` based on cache alone.
                }
                // No fallback for Dbus — desired_ime_allowed gates on
                // cursor_is_real, so popup stays hidden until first
                // real SetCursorRect arrives.
            }
        }
    }

    fn clear_owner(&mut self) {
        if matches!(self.owner, ImeOwner::None) {
            return;
        }
        self.owner = ImeOwner::None;
        self.cursor = None;
        self.cursor_is_real = false;
        self.tip_snapshot = None;
        self.dbus_focused_at = None;
        self.dbus_cursor_received = false;
    }

    // ----- Cursor reports -----

    fn report_dbus_cursor(&mut self, conn: ConnId, ic_path: &str, rect: [i32; 4]) {
        let ImeOwner::Dbus {
            conn: oc,
            ic_path: oi,
            origin,
        } = &self.owner
        else {
            tracing::debug!(?conn, ?ic_path, "CursorRect ignored: no DBus owner");
            return;
        };
        if *oc != conn || oi != ic_path {
            tracing::debug!(
                ?conn,
                ?ic_path,
                active_conn = ?oc,
                active_ic = oi,
                "CursorRect ignored: not the active IC"
            );
            return;
        }
        // Debounce GTK IM's post-FocusIn burst.
        let in_settle = self
            .dbus_focused_at
            .map(|t| t.elapsed() < FOCUS_IN_CURSOR_RECT_SETTLE)
            .unwrap_or(false);
        if self.dbus_cursor_received && in_settle {
            tracing::debug!(
                ?conn,
                ?ic_path,
                client_rect = ?rect,
                "CursorRect debounced: within FocusIn settle window"
            );
            return;
        }
        let area = (
            [origin[0] + rect[0], origin[1] + rect[1]],
            [rect[2].max(1), rect[3].max(1)],
        );
        tracing::info!(
            ?conn,
            ?ic_path,
            client_rect = ?rect,
            origin = ?origin,
            "DBus CursorRect → updating cursor"
        );
        self.cursor = Some(area);
        self.cursor_is_real = true;
        self.dbus_cursor_received = true;
        // Cache as a Rectangle for symmetry with tip cache.
        self.cursor_cache.insert(
            CursorCacheKey::Dbus(conn, ic_path.to_string()),
            Rectangle::new((rect[0], rect[1]).into(), (rect[2], rect[3]).into()),
        );
    }

    fn report_tip_cursor_change(&mut self, ti: &TextInputHandle, apps: &AppManager) {
        let ImeOwner::Tip { surface } = &self.owner else {
            return;
        };
        let Some(current) = ti.cursor_rectangle() else {
            return;
        };
        if Some(current) == self.tip_snapshot {
            return; // no change vs snapshot
        }
        tracing::debug!(
            ?current,
            snapshot = ?self.tip_snapshot,
            "tip cursor_rectangle update"
        );
        self.tip_snapshot = Some(current);
        let surface = surface.clone();
        self.cursor = Some(translate(current, app_loc(Some(&surface), apps)));
        self.cursor_is_real = true;
        self.cursor_cache
            .insert(CursorCacheKey::Tip(surface), current);
    }

    /// Per-tick reconciliation of tip-path state.
    ///
    /// 1. **Late binding upgrade**: clients like Alacritty bind
    ///    `text_input_v3` lazily — sometimes after our `on_focus_changed`
    ///    has already run with `focused_client_has_text_input == false`.
    ///    Without re-checking, owner stays `None` forever and Ctrl+Space
    ///    can't toggle IME on a freshly-focused tip client. So every
    ///    tick: if the focused surface now has a v3 binding and we
    ///    aren't already Tip-owned for it (and aren't Dbus-owned —
    ///    Dbus has its own event-driven channel), upgrade.
    ///
    /// 2. **Cursor freshness**: when Tip-owned, picks up the client's
    ///    `set_cursor_rectangle` requests within ≤ 1 frame even when
    ///    no host IME event fires.
    pub fn poll_tip_freshness(&mut self, seat: &Seat<EmskinState>, apps: &AppManager) {
        let ti = seat.text_input();
        // 1. Late-bind upgrade: owner None and focused surface now has tip.
        if matches!(self.owner, ImeOwner::None) {
            if let Some(surface) = self.focused_surface.clone() {
                if focused_client_has_text_input(ti) {
                    tracing::debug!("late tip binding detected → upgrading owner to Tip");
                    self.set_owner(ImeOwner::Tip { surface }, ti, apps);
                }
            }
        }
        // 2. Cursor change for current Tip owner.
        if matches!(self.owner, ImeOwner::Tip { .. }) {
            self.report_tip_cursor_change(ti, apps);
        }
    }

    // ----- Override -----

    /// Mark the start / end of an Emacs prefix chord.
    pub fn set_prefix_active(&mut self, active: bool) {
        if self.prefix_active != active {
            tracing::debug!(active, "IME: prefix_active toggled");
            self.prefix_active = active;
        }
    }

    // ----- Reset -----

    pub fn reset_on_workspace_switch(&mut self) {
        tracing::debug!("IME: reset on workspace switch");
        self.focused_surface = None;
        self.owner = ImeOwner::None;
        self.cursor = None;
        self.cursor_is_real = false;
        self.tip_snapshot = None;
        self.dbus_focused_at = None;
        self.dbus_cursor_received = false;
        // Caches reference now-inactive workspace's surfaces; clear
        // both so a new-workspace FocusIn doesn't replay a rect from
        // an unrelated app.
        self.cursor_cache.clear();
        self.prefix_active = false;
        // Don't reset `last_applied_*` — next sync_to_winit diffs.
    }

    // ----- Event entry points -----

    /// Process a fcitx event from the DBus broker.
    pub fn on_fcitx_event(
        &mut self,
        event: crate::dbus_broker::FcitxEvent,
        app_origin: Option<[i32; 2]>,
        seat: &Seat<EmskinState>,
        apps: &AppManager,
    ) {
        use crate::dbus_broker::FcitxEvent;

        match event {
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: true,
            } => {
                let origin = app_origin.unwrap_or([0, 0]);
                tracing::info!(?conn, ?ic_path, ?origin, "fcitx IC FocusIn → DBus owner");
                let ti = seat.text_input();
                self.set_owner(
                    ImeOwner::Dbus {
                        conn,
                        ic_path,
                        origin,
                    },
                    ti,
                    apps,
                );
            }
            FcitxEvent::FocusChanged {
                conn,
                ic_path,
                focused: false,
            } => {
                if let ImeOwner::Dbus {
                    conn: oc,
                    ic_path: oi,
                    ..
                } = &self.owner
                {
                    if *oc == conn && oi == &ic_path {
                        tracing::debug!(?conn, ?ic_path, "fcitx IC FocusOut → clearing owner");
                        self.clear_owner();
                    }
                }
            }
            FcitxEvent::CursorRect {
                conn,
                ic_path,
                rect,
            } => {
                self.report_dbus_cursor(conn, &ic_path, rect);
            }
            FcitxEvent::IcDestroyed { conn, ic_path } => {
                if let ImeOwner::Dbus {
                    conn: oc,
                    ic_path: oi,
                    ..
                } = &self.owner
                {
                    if *oc == conn && oi == &ic_path {
                        self.clear_owner();
                    }
                }
                self.cursor_cache
                    .remove(&CursorCacheKey::Dbus(conn, ic_path));
            }
        }
    }

    /// Bridge keyboard focus change. Updates `focused_surface` for
    /// text_input enter/leave plumbing. The DBus owner is independent
    /// of keyboard focus (driven by FcitxEvent::FocusChanged); we
    /// only update Tip ownership here.
    pub fn on_focus_changed(
        &mut self,
        seat: &Seat<EmskinState>,
        new_focus: Option<WlSurface>,
        apps: &AppManager,
    ) {
        let ti = seat.text_input();
        let old = self.focused_surface.take();
        transition_focus(ti, old, &new_focus);
        self.focused_surface = new_focus.clone();

        // Decide Tip ownership based on the new focus. DBus ownership
        // is independent (driven by FcitxEvent::FocusChanged); the
        // embedded client's GTK IM module sends FocusOut over DBus
        // when keyboard focus moves away, which clears the DBus owner
        // separately.
        let new_owner = match new_focus {
            Some(surface) if focused_client_has_text_input(ti) => ImeOwner::Tip { surface },
            _ => ImeOwner::None,
        };
        self.set_owner(new_owner, ti, apps);
    }

    /// Forward a host IME event to the focused text_input_v3 client.
    /// DBus path is handled by the broker's `emit_commit_string` /
    /// `emit_preedit` (called from winit.rs::WinitEvent::Ime).
    pub fn on_host_ime_event(
        &mut self,
        event: winit_crate::event::Ime,
        seat: &Seat<EmskinState>,
        apps: &AppManager,
        _window: &winit_crate::window::Window,
    ) {
        use winit_crate::event::Ime;

        let ti = seat.text_input();
        self.report_tip_cursor_change(ti, apps);

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
}

// ---------------- helpers ----------------

/// App-space top-left of `surface`, falling back to (0, 0) when not
/// tracked (e.g. Emacs main surface, which IS the winit window).
fn app_loc(surface: Option<&WlSurface>, apps: &AppManager) -> [i32; 2] {
    surface
        .and_then(|s| apps.surface_geometry(s))
        .map(|g| [g.loc.x, g.loc.y])
        .unwrap_or([0, 0])
}

/// Translate a client-surface-local rect into emskin-winit-local
/// `(position, size)`.
fn translate(rect: Rectangle<i32, Logical>, app_loc: [i32; 2]) -> ([i32; 2], [i32; 2]) {
    (
        [app_loc[0] + rect.loc.x, app_loc[1] + rect.loc.y],
        [rect.size.w.max(1), rect.size.h.max(1)],
    )
}

/// Update smithay's text_input focus and fire enter/leave at the right
/// clients. smithay would do this automatically with an input_method
/// protocol registered, but we don't — hence the manual dance. The
/// `leave` event must be sent *while* text_input focus still points at
/// `old`, otherwise smithay routes it to the new surface instead.
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

smithay::delegate_text_input_manager!(EmskinState);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_dbus_distinguishes_conn_and_ic() {
        let a = CursorCacheKey::Dbus(ConnId::new_for_test(1), "/ic/1".into());
        let b = CursorCacheKey::Dbus(ConnId::new_for_test(1), "/ic/2".into());
        let c = CursorCacheKey::Dbus(ConnId::new_for_test(2), "/ic/1".into());
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn ime_owner_same_identity_dbus_ignores_origin() {
        let a = ImeOwner::Dbus {
            conn: ConnId::new_for_test(1),
            ic_path: "/ic/1".into(),
            origin: [10, 20],
        };
        let b = ImeOwner::Dbus {
            conn: ConnId::new_for_test(1),
            ic_path: "/ic/1".into(),
            origin: [99, 99],
        };
        assert!(
            a.same_identity_as(&b),
            "same conn + ic_path = same identity"
        );
    }

    #[test]
    fn ime_owner_same_identity_dbus_distinguishes_ic_path() {
        let a = ImeOwner::Dbus {
            conn: ConnId::new_for_test(1),
            ic_path: "/ic/1".into(),
            origin: [0, 0],
        };
        let b = ImeOwner::Dbus {
            conn: ConnId::new_for_test(1),
            ic_path: "/ic/2".into(),
            origin: [0, 0],
        };
        assert!(!a.same_identity_as(&b));
    }
}
