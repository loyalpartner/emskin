//! XWayland integration state — mirrors cosmic-comp's `XWaylandState`
//! in shape (single-struct home for everything XWayland-related) even
//! though it's much thinner because emskin delegates the actual X
//! server to `xwayland-satellite` rather than running `Xwayland` under
//! smithay's `X11Wm`.
//!
//! Three orthogonal pieces live here.
//!
//! **Display number** (`display`): cached `:N` for convenience —
//! exported as `DISPLAY` to the Emacs child and sent to elisp via
//! `XWaylandReady` IPC. Set once and forgotten.
//!
//! **Supervisor** (`integration`): pre-binds `/tmp/.X11-unix/X<N>` +
//! the abstract socket, arms calloop watches, and lazily spawns
//! `xwayland-satellite` on first X client connect. See the
//! `crate::xwayland_satellite` module for the state machine.
//!
//! **Pending child command** (`pending_command`): the `--command`
//! flag's value, parked until XWayland reports Ready so GTK3 /
//! Electron children spawn with a valid `DISPLAY` in env. Drained
//! exactly once by the main-loop hook that observes
//! `xwayland_satellite::ToMain::Ready`.

use crate::xwayland_satellite::XwlsIntegration;

/// Child command to spawn once XWayland is ready. None = already
/// spawned or `--no-spawn` was passed.
pub struct PendingCommand {
    pub command: String,
    pub args: Vec<String>,
    pub standalone: bool,
}

#[derive(Default)]
pub struct XwaylandState {
    display: Option<u32>,
    integration: Option<XwlsIntegration>,
    pending_command: Option<PendingCommand>,
}

impl XwaylandState {
    // -- Display number --------------------------------------------

    /// Cache the display number once XWayland reports Ready. The field
    /// is currently write-only at the public API level (no downstream
    /// reader) — retained because the stashing site exports the same
    /// value to the `DISPLAY` env var and IPC notification, so the
    /// cache exists to support a future consumer without touching
    /// main's event-loop bootstrap.
    pub fn set_display(&mut self, display: u32) {
        self.display = Some(display);
    }

    // -- Supervisor ------------------------------------------------

    /// Mutable access to the supervisor for arming calloop watches
    /// and driving the spawn state machine.
    pub fn integration_mut(&mut self) -> Option<&mut XwlsIntegration> {
        self.integration.as_mut()
    }

    /// Install the supervisor. Called from `main` after sockets bind.
    pub fn set_integration(&mut self, integration: XwlsIntegration) {
        self.integration = Some(integration);
    }

    /// Drop the supervisor on a fatal init error so the rest of the
    /// compositor keeps running without XWayland.
    pub fn clear_integration(&mut self) {
        self.integration = None;
    }

    // -- Pending child command -------------------------------------

    /// Park a command to be spawned once XWayland reports Ready.
    pub fn set_pending_command(&mut self, cmd: PendingCommand) {
        self.pending_command = Some(cmd);
    }

    /// Drain the parked command (exactly once).
    pub fn take_pending_command(&mut self) -> Option<PendingCommand> {
        self.pending_command.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `XwlsIntegration` owns pre-bound X11 sockets + a calloop
    // `channel::Sender` and has no Default — constructing one in a
    // unit test is out of scope. Its slot (set / integration_mut /
    // clear) is covered by the e2e suite. Everything else (display
    // cache, pending_command mailbox) is pure logic.

    fn sample_cmd() -> PendingCommand {
        PendingCommand {
            command: "emacs".into(),
            args: vec!["-nw".into()],
            standalone: false,
        }
    }

    #[test]
    fn default_is_empty_on_all_three_slots() {
        let s = XwaylandState::default();
        assert!(s.display.is_none());
        assert!(s.pending_command.is_none());
        assert!(s.integration.is_none());
    }

    #[test]
    fn set_display_caches_the_number() {
        let mut s = XwaylandState::default();
        s.set_display(42);
        assert_eq!(s.display, Some(42));
    }

    #[test]
    fn set_display_overwrites_previous_value() {
        // Guards against a future where XWayland restarts mid-session —
        // the latest display wins.
        let mut s = XwaylandState::default();
        s.set_display(1);
        s.set_display(2);
        assert_eq!(s.display, Some(2));
    }

    #[test]
    fn pending_command_set_then_take() {
        let mut s = XwaylandState::default();
        assert!(s.take_pending_command().is_none());

        s.set_pending_command(sample_cmd());
        assert!(
            s.pending_command.is_some(),
            "set_pending_command parks the value"
        );

        let taken = s.take_pending_command().expect("take returns what was set");
        assert_eq!(taken.command, "emacs");
        assert_eq!(taken.args, vec!["-nw".to_string()]);
        assert!(!taken.standalone);

        assert!(
            s.pending_command.is_none(),
            "take drains — second read is None"
        );
        assert!(s.take_pending_command().is_none());
    }

    #[test]
    fn set_pending_command_overwrites_previous() {
        // If the user somehow re-arms a command before the first is
        // drained, the second wins. Mirrors the flat-assignment
        // semantics that existed before extraction.
        let mut s = XwaylandState::default();
        s.set_pending_command(PendingCommand {
            command: "old".into(),
            args: vec![],
            standalone: true,
        });
        s.set_pending_command(PendingCommand {
            command: "new".into(),
            args: vec!["-flag".into()],
            standalone: false,
        });
        let taken = s.take_pending_command().unwrap();
        assert_eq!(taken.command, "new");
        assert_eq!(taken.args, vec!["-flag".to_string()]);
        assert!(!taken.standalone);
    }

    #[test]
    fn display_and_pending_command_are_independent() {
        let mut s = XwaylandState::default();
        s.set_display(7);
        s.set_pending_command(sample_cmd());

        assert_eq!(s.display, Some(7));
        assert!(s.pending_command.is_some());

        let _ = s.take_pending_command();
        assert_eq!(
            s.display,
            Some(7),
            "draining the command must not touch the display cache"
        );
    }

    #[test]
    fn integration_mut_none_by_default() {
        let mut s = XwaylandState::default();
        assert!(s.integration_mut().is_none());
    }

    #[test]
    fn clear_integration_is_no_op_when_already_empty() {
        let mut s = XwaylandState::default();
        s.clear_integration();
        assert!(s.integration_mut().is_none());
    }
}
