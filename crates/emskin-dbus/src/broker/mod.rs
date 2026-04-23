//! Per-connection broker logic: a pure state machine that consumes raw socket
//! bytes and emits the bytes the proxy should forward, plus parsed message
//! headers observed on the client → bus direction.
//!
//! The socket-level I/O (listening, `accept()`, `SCM_RIGHTS` fd passing,
//! `poll()`) lives in the binary crate; this module is intentionally pure
//! so it can be exercised end-to-end in unit tests without spinning up
//! Unix sockets.
//!
//! Shape follows `xdg-dbus-proxy`'s `flatpak-proxy.c` — auth bytes are
//! forwarded incrementally as they arrive, and the scanner runs against a
//! separate accumulator so it can still locate `BEGIN\r\n` across chunk
//! boundaries. After BEGIN, we parse DBus-wire messages and forward them
//! one at a time so the Task #5 rule engine can see headers at each
//! boundary.

pub mod io;
pub mod state;

use crate::dbus::message::Endian;
use crate::rules::cursor::{self, CursorMethod};
use state::Output;

/// Walk every observed message in `out`, classify any that look like IME
/// cursor-coord methods, and add `delta` to the leading `(x, y)` bytes
/// of their bodies in place. Messages we don't classify (or whose bodies
/// are shorter than the classified method requires) are left untouched.
///
/// Extracted so both the subprocess broker (`io.rs`, threaded, uses
/// `SharedOffset`) and the in-process broker (in emskin, calloop-driven,
/// borrows the offset directly) can share the same rewrite pass. Split
/// borrow pattern: we snapshot `(method, endian, body_start, body_end)`
/// for each classified message first, then mutate `out.forward[..]` —
/// because both live on the same `Output` struct.
pub fn apply_cursor_rewrites(out: &mut Output, delta: (i32, i32)) {
    if out.messages.is_empty() {
        return;
    }
    let specs: Vec<(CursorMethod, Endian, usize, usize)> = out
        .messages
        .iter()
        .filter_map(|msg| {
            let method = cursor::classify(&msg.header)?;
            let body_len = msg.header.body_len as usize;
            if body_len < method.expected_body_len() {
                return None;
            }
            let body_start = msg.offset + msg.length - body_len;
            let body_end = msg.offset + msg.length;
            Some((method, msg.header.endian, body_start, body_end))
        })
        .collect();

    for (method, endian, start, end) in specs {
        if let Err(e) = cursor::apply_offset(method, endian, &mut out.forward[start..end], delta) {
            tracing::warn!(error = %e, "cursor rewrite skipped");
            continue;
        }
        tracing::info!(
            ?method,
            dx = delta.0,
            dy = delta.1,
            "cursor rewrite applied"
        );
    }
}
