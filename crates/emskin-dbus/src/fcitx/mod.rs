//! Fcitx5 DBus frontend — classification + per-connection IC state.
//!
//! The broker uses this module to recognize incoming method_calls on
//! `org.fcitx.Fcitx.InputMethod1` / `InputContext1` and turn their
//! bodies into typed events. The broker then decides what to do with
//! each event (emit a stub reply, fire a callback to drive winit IME,
//! forward to real fcitx5 — depends on milestone).
//!
//! State-machine wise, each connected DBus client owns an
//! [`IcRegistry`] that allocates IC object paths and tracks per-IC
//! cursor rect / focus / capability. The registry is deliberately
//! single-connection-scoped — fcitx5 IC paths aren't shared across
//! clients so there's no reason to make them global.
//!
//! # Scope
//!
//! Phase M2 only recognizes enough of the interface to drive the
//! in-process B1 plan:
//!
//! - `InputMethod1.CreateInputContext` — factory for new ICs.
//! - `InputContext1` methods that carry state we need to read
//!   (`FocusIn`, `FocusOut`, `SetCapability`, `SetCursorRect[V2]`,
//!   `SetCursorLocation`, `SetSurroundingText[Position]`,
//!   `ProcessKeyEvent`, `Reset`, `DestroyIC`).
//!
//! Signal emission (`CommitString`, `UpdateFormattedPreedit`,
//! `ForwardKey`, …) happens from the *other* direction — emskin's
//! winit IME event handler builds those via `dbus::encode::Signal` and
//! writes them into the connection's client_out buffer.

pub mod classify;
pub mod ic;
pub mod reply;

pub use classify::{classify, FcitxMethod};
pub use ic::{IcRegistry, IcState};
pub use reply::build_reply;

/// Interfaces this module recognizes. Exposed so the broker can gate
/// "did this method_call match fcitx5?" on a cheap string compare
/// before calling `classify` (which does more work).
pub const INPUT_METHOD_IFACE: &str = "org.fcitx.Fcitx.InputMethod1";
pub const INPUT_CONTEXT_IFACE: &str = "org.fcitx.Fcitx.InputContext1";
pub const INPUT_CONTEXT_IFACE_FCITX4: &str = "org.fcitx.Fcitx.InputContext";

/// Bus names the real fcitx5 typically owns. The broker intercepts
/// method_calls with `destination` matching any of these *or* with
/// `interface` matching one of the above, so clients dialing via the
/// portal variant or directly via `org.fcitx.Fcitx5` are both caught.
pub const FCITX5_WELL_KNOWN_NAMES: &[&str] = &[
    "org.fcitx.Fcitx5",
    "org.freedesktop.portal.Fcitx",
    // fcitx4 kept here for symmetry; fcitx5 also claims it for some
    // legacy clients (WeChat / old XIM bridges).
    "org.fcitx.Fcitx",
];

/// Check whether a method_call's `interface` names one of the fcitx5
/// frontend surfaces this module handles. Cheap, no body parsing.
pub fn is_fcitx_interface(iface: &str) -> bool {
    matches!(
        iface,
        INPUT_METHOD_IFACE | INPUT_CONTEXT_IFACE | INPUT_CONTEXT_IFACE_FCITX4
    )
}

/// Check whether `name` is one of the well-known DBus service names
/// fcitx5 registers — `org.fcitx.Fcitx5`, `org.freedesktop.portal.Fcitx`,
/// or the legacy fcitx4 `org.fcitx.Fcitx`. Used to recognize
/// `GetNameOwner` lookups the client makes against fcitx5 and record
/// the answer for signal-sender bookkeeping.
pub fn is_fcitx_well_known(name: &str) -> bool {
    FCITX5_WELL_KNOWN_NAMES.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_fcitx_interface_matches_known_ifaces() {
        assert!(is_fcitx_interface("org.fcitx.Fcitx.InputMethod1"));
        assert!(is_fcitx_interface("org.fcitx.Fcitx.InputContext1"));
        assert!(is_fcitx_interface("org.fcitx.Fcitx.InputContext"));
    }

    #[test]
    fn is_fcitx_interface_rejects_unrelated() {
        assert!(!is_fcitx_interface("org.freedesktop.DBus"));
        assert!(!is_fcitx_interface("org.fcitx.Fcitx.InputMethod")); // wrong: no 1
        assert!(!is_fcitx_interface(""));
    }
}
