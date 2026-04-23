//! emskin-dbus — DBus session-bus protocol primitives for nested Wayland
//! compositors.
//!
//! Scope:
//!   - SASL handshake scanning (`dbus::sasl`).
//!   - DBus v1 header parsing (`dbus::message`).
//!   - Per-connection state machine (`broker::state::ConnectionState`).
//!   - Cursor-coord rewrite rules for fcitx4/fcitx5 IME methods
//!     (`rules::cursor`).
//!   - `apply_cursor_rewrites` pass over an `Output` in place
//!     (`broker::apply_cursor_rewrites`).
//!
//! The broker's socket-level I/O driver is not in this crate — it lives
//! in `emskin::dbus_broker`. That keeps this crate pure enough to be
//! consumed by any nested compositor that wants the parser + rewrite
//! rules without taking on calloop or smithay.

pub mod broker;
pub mod dbus;
pub mod rules;
