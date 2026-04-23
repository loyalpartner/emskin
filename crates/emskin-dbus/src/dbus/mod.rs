//! DBus wire-format primitives for the pass-through proxy.
//!
//! - [`sasl`] scans the SASL auth handshake (client → bus direction) and
//!   reports where `BEGIN\r\n` ends so the broker can switch from raw byte
//!   forwarding to message parsing.
//! - [`message`] decodes enough of the fixed 16-byte header + header fields
//!   to identify the `member` / `path` / `interface` we care about (notably
//!   `SetCursorRect` / `SetCursorLocation`) without touching the body. The
//!   body is always treated as opaque bytes and re-emitted verbatim unless a
//!   rule rewrites it.
//!
//! References:
//!   - DBus spec §"Message Protocol"
//!     <https://dbus.freedesktop.org/doc/dbus-specification.html#message-protocol>
//!   - `xdg-dbus-proxy` (flatpak) `flatpak-proxy.c` — the transparent-broker
//!     shape this crate borrows from.

pub mod encode;
pub mod message;
pub mod sasl;
