//! Per-connection broker logic: a pure state machine that consumes raw
//! socket bytes and emits the bytes the driver should forward, plus
//! parsed message headers observed on the client → bus direction.
//!
//! The socket-level I/O (listening, `accept()`, per-connection
//! pumping, `SCM_RIGHTS` fd passing) lives in `emskin`'s
//! `dbus_broker.rs` — this module is intentionally pure so it can be
//! exercised end-to-end in unit tests without spinning up Unix
//! sockets.
//!
//! Shape follows `xdg-dbus-proxy`'s `flatpak-proxy.c` — auth bytes are
//! forwarded incrementally as they arrive, and the scanner runs
//! against a separate accumulator so it can still locate `BEGIN\r\n`
//! across chunk boundaries. After BEGIN, we parse DBus-wire messages
//! and forward them one at a time so the fcitx5 frontend (or any
//! future rule engine) can see headers at each boundary.

pub mod state;
