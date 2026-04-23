# emskin-dbus — DBus session-bus protocol primitives

Zero smithay deps. Provides the SASL handshake scanner, DBus v1 header
parser + encoder, per-connection state machine, and fcitx5 frontend
classifier / reply synthesis used by emskin's in-process broker.

History: started out as a subprocess (`emskin-dbus-proxy` binary) +
JSON ctl socket for cursor-coord rewrite. M1 pulled the broker
in-process. M2 replaced the cursor-rewrite hack with a full fcitx5
DBus frontend intercept (B1 — see `emskin/src/dbus_broker.rs` header
comment + this crate's `fcitx/` module). The lib still has no smithay
dep but in practice is emskin-specific at this point; "reusable by any
nested compositor" is retained as a *possibility* rather than a design
constraint.

## Scope matrix

| Feature | Phase 1 | Phase 2 |
|---|---|---|
| SASL handshake scanner (`dbus/sasl.rs`) | ✅ | |
| DBus v1 header parser (`dbus/message.rs`) | ✅ | |
| DBus v1 message encoder (`dbus/encode.rs`) — method_return / signal / error | ✅ | |
| Per-connection state machine (`broker/state.rs`) | ✅ | |
| Fcitx5 method_call classifier (`fcitx/classify.rs`) | ✅ | |
| Per-connection fcitx5 IC registry (`fcitx/ic.rs`) | ✅ | |
| Fcitx5 method_return synthesis (`fcitx/reply.rs`) | ✅ | |
| `RequestName` local-own interception → closes emskin#60 | | ✅ |
| `ListNames` / `NameOwnerChanged` merging for policy | | ✅ |

## Architecture

```
embedded app (WeChat / Emacs pgtk / Electron)
       │
       │ DBus (bus.sock injected via DBUS_SESSION_BUS_ADDRESS)
       ▼
┌──────────────── emskin process ───────────────┐
│                                               │
│  dbus_broker.rs (calloop-driven)              │
│    ├─ state::ConnectionState      ← this lib   │
│    │   parses client → bus bytes              │
│    │                                          │
│    ├─ fcitx::classify             ← this lib   │
│    │   matches InputMethod1 / InputContext1   │
│    │                                          │
│    ├─ fcitx::build_reply          ← this lib   │
│    │   synthesizes method_return bytes        │
│    │                                          │
│    └─ dbus::encode::Signal        ← this lib   │
│        (CommitString / UpdateFormattedPreedit)│
│                                               │
└──────┬────────────────────────────────────────┘
       │ non-fcitx5 methods pass through
       ▼
  upstream host session bus (real fcitx5 stays untouched)
```

The socket-level I/O (calloop sources, read / write buffers, accept
loop, upstream dialing) lives in `emskin/src/dbus_broker.rs`. This
crate is pure enough that every test runs without an event loop —
socketpair-style end-to-end coverage is in the broker's tests.

## Invariants

- **Parser is append-only.** `ConnectionState.client_feed(chunk)` must
  be called with successive socket reads; internally buffers partial
  messages. The returned `Output.forward` is the *exact* byte sequence
  to write to the other side — intercept sites filter it, not mutate
  it in place.
- **Encoder is little-endian only.** The parser still accepts
  big-endian input for messages the broker forwards verbatim; anything
  the broker synthesizes itself is LE because every modern Linux DBus
  client is LE and there's no value in the extra path.
- **Signals need a unique-name sender.** `emskin_dbus::fcitx::reply`
  does not set sender — the broker owns that (the caller tracks the
  real fcitx5 unique name via GetNameOwner-reply parsing +
  NameOwnerChanged refresh) and stamps it on the `Signal { sender,
  .. }` before encoding.
- **IC paths are per-connection.** `IcRegistry::allocate` returns
  monotonically-increasing ids scoped to one client — fcitx5 works the
  same way, and sharing ids across clients would let one client's
  DestroyIC trample another's IC.
- **No fd passing yet.** DBus methods that transfer unix fds
  (`SCM_RIGHTS` — portals, notification image payloads) forward
  without the fds; those calls fail. Documented phase-1 limitation in
  emskin#55.

## Non-goals

- No high-level `Proxy` / `ObjectServer` API. This is raw-byte
  primitives for a broker, not a DBus service library.
- No activation fork-exec logic — all activation stays on the host bus.
- No policy / sandbox filtering. xdg-dbus-proxy's security model is
  out of scope; we use the same DBus-parsing techniques but the
  "what's allowed" question is fully answered by "emskin only
  intercepts fcitx5 interfaces, forwards everything else verbatim".
