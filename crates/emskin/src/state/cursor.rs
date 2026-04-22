//! Cursor image tracking + relative-pointer delta synthesis.
//!
//! Two unrelated concerns live here because both flow through the same
//! pipe (host winit → compositor → embedded clients) and both are
//! driven by the pointer:
//!
//! 1. **Image** (`status` + `changed`). Clients set cursors via
//!    `wp_cursor_shape_v1` (Named, forwarded to winit's `CursorIcon`)
//!    or `wl_pointer.set_cursor` with a Surface argument (GTK3 /
//!    Emacs — software-rendered each frame because winit cannot
//!    forward arbitrary buffers to the host). The `changed` flag is
//!    drained by the render loop in the next frame.
//!
//! 2. **Raw pointer tracking** (`last_raw_loc`). smithay's winit
//!    backend only emits `PointerMotionAbsolute` — there is no
//!    raw-motion source — so `zwp_relative_pointer_v1` (required by
//!    Minecraft, Blender, browser Pointer Lock) is synthesised by
//!    diffing consecutive host-reported absolutes. This delta stays
//!    correct under a pointer lock, while `pointer.current_location()`
//!    would freeze.

use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Point};

pub struct CursorState {
    status: CursorImageStatus,
    /// Set when `status` changes; cleared by the render loop in the
    /// frame that applies the change to the host winit window.
    changed: bool,
    /// Last raw absolute pointer location from the host, in compositor
    /// coords. `None` on first event.
    last_raw_loc: Option<Point<f64, Logical>>,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            status: CursorImageStatus::default_named(),
            changed: false,
            last_raw_loc: None,
        }
    }
}

impl CursorState {
    /// Replace the cursor image and mark it dirty. Called from
    /// `SeatHandler::cursor_image` whenever a client updates its
    /// cursor via wp_cursor_shape_v1 or wl_pointer.set_cursor.
    pub fn set_image(&mut self, image: CursorImageStatus) {
        self.status = image;
        self.changed = true;
    }

    /// Peek at the current image without touching the dirty flag.
    /// Used by the software cursor path in the render loop, which
    /// draws every frame regardless of whether the image changed.
    pub fn status(&self) -> &CursorImageStatus {
        &self.status
    }

    /// Drain the dirty flag. `Some(&status)` means the host winit
    /// window should update `set_cursor` / `set_cursor_visible`
    /// this frame; `None` means no change pending.
    pub fn take_changed(&mut self) -> Option<&CursorImageStatus> {
        if self.changed {
            self.changed = false;
            Some(&self.status)
        } else {
            None
        }
    }

    /// If the currently set Surface cursor has been destroyed by its
    /// client, fall back to the default named cursor. Called once per
    /// render frame before the software-cursor walk — otherwise the
    /// render path would chase a dead `WlSurface`.
    pub fn ensure_alive(&mut self) {
        if let CursorImageStatus::Surface(ref s) = self.status {
            if !s.is_alive() {
                self.status = CursorImageStatus::default_named();
                self.changed = true;
            }
        }
    }

    /// On workspace switch, drop any Surface cursor that referenced
    /// the departing workspace's clients so it isn't left rendering
    /// against a stale surface tree. Named cursors survive — they're
    /// owned by the host, not the clients.
    pub fn reset_on_workspace_switch(&mut self) {
        if matches!(self.status, CursorImageStatus::Surface(_)) {
            self.status = CursorImageStatus::default_named();
            self.changed = true;
        }
    }

    /// Absorb a new raw absolute pointer location and return the
    /// delta from the previous one (zero on first call). Drives
    /// `zwp_relative_pointer_v1`.
    pub fn consume_raw_location(&mut self, new_abs: Point<f64, Logical>) -> Point<f64, Logical> {
        match self.last_raw_loc.replace(new_abs) {
            Some(prev) => new_abs - prev,
            None => Point::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::reexports::winit::window::CursorIcon;

    // Surface-path tests (ensure_alive with a dead surface,
    // reset_on_workspace_switch dropping a Surface) require a live
    // Wayland `Display` + `Client` to instantiate a `WlSurface`, which
    // exceeds the scope of unit tests. Those paths are exercised by the
    // e2e suite under `tests/e2e_*.rs`.

    #[test]
    fn default_is_named_not_changed_no_last_loc() {
        let c = CursorState::default();
        assert!(matches!(c.status, CursorImageStatus::Named(_)));
        assert!(!c.changed);
        assert!(c.last_raw_loc.is_none());
    }

    #[test]
    fn status_returns_current_without_touching_changed() {
        let c = CursorState::default();
        let _ = c.status();
        assert!(!c.changed, "read-only peek must not dirty the flag");
    }

    #[test]
    fn set_image_writes_status_and_marks_changed() {
        let mut c = CursorState::default();
        c.set_image(CursorImageStatus::Hidden);
        assert!(matches!(c.status, CursorImageStatus::Hidden));
        assert!(c.changed);
    }

    #[test]
    fn set_image_named_overrides_default_named() {
        let mut c = CursorState::default();
        c.set_image(CursorImageStatus::Named(CursorIcon::Crosshair));
        assert!(
            matches!(c.status, CursorImageStatus::Named(CursorIcon::Crosshair)),
            "set_image did not replace the Named variant"
        );
        assert!(c.changed);
    }

    #[test]
    fn take_changed_returns_none_when_clean() {
        let mut c = CursorState::default();
        assert!(c.take_changed().is_none());
        assert!(!c.changed);
    }

    #[test]
    fn take_changed_returns_some_once_then_clears() {
        let mut c = CursorState::default();
        c.set_image(CursorImageStatus::Hidden);
        {
            let drained = c.take_changed().expect("first drain returns Some");
            assert!(matches!(drained, CursorImageStatus::Hidden));
        }
        assert!(
            !c.changed,
            "take_changed must clear the dirty flag after handing out the value"
        );
        assert!(c.take_changed().is_none());
    }

    #[test]
    fn ensure_alive_is_no_op_on_named() {
        let mut c = CursorState::default();
        c.ensure_alive();
        assert!(matches!(c.status, CursorImageStatus::Named(_)));
        assert!(
            !c.changed,
            "ensure_alive must not dirty when status is not Surface"
        );
    }

    #[test]
    fn ensure_alive_is_no_op_on_hidden() {
        let mut c = CursorState::default();
        c.set_image(CursorImageStatus::Hidden);
        let _ = c.take_changed();
        c.ensure_alive();
        assert!(matches!(c.status, CursorImageStatus::Hidden));
        assert!(!c.changed);
    }

    #[test]
    fn reset_on_workspace_switch_is_no_op_on_named() {
        let mut c = CursorState::default();
        c.reset_on_workspace_switch();
        assert!(matches!(c.status, CursorImageStatus::Named(_)));
        assert!(
            !c.changed,
            "reset must not dirty when nothing was torn down"
        );
    }

    #[test]
    fn reset_on_workspace_switch_is_no_op_on_hidden() {
        let mut c = CursorState::default();
        c.set_image(CursorImageStatus::Hidden);
        let _ = c.take_changed();
        c.reset_on_workspace_switch();
        assert!(matches!(c.status, CursorImageStatus::Hidden));
        assert!(!c.changed);
    }

    #[test]
    fn consume_raw_location_first_call_returns_zero() {
        let mut c = CursorState::default();
        let delta = c.consume_raw_location((10.0, 20.0).into());
        assert_eq!(delta, Point::<f64, Logical>::from((0.0, 0.0)));
    }

    #[test]
    fn consume_raw_location_returns_delta_after_first() {
        let mut c = CursorState::default();
        let _ = c.consume_raw_location((10.0, 20.0).into());
        let delta = c.consume_raw_location((30.0, 25.0).into());
        assert_eq!(delta, Point::<f64, Logical>::from((20.0, 5.0)));
    }

    #[test]
    fn consume_raw_location_updates_last_for_subsequent_calls() {
        let mut c = CursorState::default();
        let _ = c.consume_raw_location((10.0, 20.0).into());
        let _ = c.consume_raw_location((30.0, 25.0).into());
        let delta = c.consume_raw_location((35.0, 30.0).into());
        assert_eq!(
            delta,
            Point::<f64, Logical>::from((5.0, 5.0)),
            "third call must diff against the second, not the first"
        );
    }

    #[test]
    fn consume_raw_location_handles_negative_delta() {
        let mut c = CursorState::default();
        let _ = c.consume_raw_location((50.0, 50.0).into());
        let delta = c.consume_raw_location((30.0, 40.0).into());
        assert_eq!(delta, Point::<f64, Logical>::from((-20.0, -10.0)));
    }
}
