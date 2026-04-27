//! DBus wire-format primitives for the in-process broker.
//!
//! - [`sasl`] scans the SASL auth handshake (client → bus direction) so
//!   the broker knows when to switch from raw byte forwarding to message
//!   parsing.
//! - [`frame`] is the single type for every direction: parse incoming
//!   bytes into a [`frame::Frame`], inspect typed headers, lazily
//!   decode the body into Rust values, and build replies / signals via
//!   [`frame::FrameBuilder`]. Both encode and decode go through `zvariant`.
//!
//! References:
//!   - DBus spec §"Message Protocol"
//!     <https://dbus.freedesktop.org/doc/dbus-specification.html#message-protocol>
//!   - `xdg-dbus-proxy` (flatpak) `flatpak-proxy.c` — the transparent
//!     broker shape this crate borrows from.

pub mod frame;
pub mod sasl;
