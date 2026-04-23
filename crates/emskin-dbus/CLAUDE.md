# emskin-dbus — DBus session-bus protocol primitives

Zero smithay deps. Provides the SASL handshake scanner, DBus v1 header
parser, per-connection state machine, and cursor-coord rewrite rules
used by emskin's in-process broker.

This crate used to ship an out-of-process `emskin-dbus-proxy` binary
plus a JSON ctl-socket API. That split was removed when the broker
moved in-process (see `emskin/src/dbus_broker.rs`) — the subprocess
added two cross-process JSON serializations and ~200μs of focus-update
latency for no reusability we were actually using. The lib still has no
smithay dep and can be consumed by any nested compositor that wants the
parser + rewrite rules.

## Scope matrix

| Feature | Phase 1 | Phase 2 |
|---|---|---|
| SASL + DBus v1 parser (`dbus/`) | ✅ | |
| Per-connection state machine (`broker/state.rs`) | ✅ | |
| `SetCursorRect` / `SetCursorRectV2` / `SetCursorLocation` coord rewrite (`rules/cursor.rs`) | ✅ | |
| `apply_cursor_rewrites` pass over an `Output` (`broker/mod.rs`) | ✅ | |
| Fake-fcitx5 DBus frontend (B1: emskin replies in place of real fcitx5) | | ✅ |
| `RequestName` local-own interception → closes emskin#60 | | ✅ |

## Architecture

```
embedded app ──DBus──▶ emskin (in-process broker)
                         │
                         │ emskin-dbus primitives:
                         │   ConnectionState.client_feed → Output
                         │   apply_cursor_rewrites(&mut Output, delta)
                         │
                         ▼
                    upstream host session bus
```

The I/O driver, connection registration, and per-tick focus
reconciliation all live in `emskin` (`src/dbus_broker.rs` + glue in
`main.rs`). This crate stays pure enough that every test runs without
an event loop — socketpair-style end-to-end coverage is in
`emskin/src/dbus_broker.rs` tests instead.

## Invariants

- **Parser is append-only**. `ConnectionState.client_feed(chunk)` must
  be called with successive socket reads; internally buffers partial
  messages. The returned `Output.forward` is the *exact* byte sequence
  to write to the other side (after optional rewrite).
- **Rewrite is in place** on `Output.forward` — `apply_cursor_rewrites`
  mutates the cursor-coord bytes of classified messages. Unclassified
  messages pass through.
- **No fd passing yet**. DBus methods that transfer unix fds
  (`SCM_RIGHTS` — portals, notification image payloads) forward without
  the fds; those calls fail. Documented phase-1 limitation in
  emskin#55.

## Non-goals

- No high-level `Proxy` / `ObjectServer` API. This is raw-byte
  primitives for a broker, not a DBus service library.
- No activation fork-exec logic — all activation stays on the host bus.
