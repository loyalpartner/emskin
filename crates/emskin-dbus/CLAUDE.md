# emskin-dbus — DBus session-bus protocol primitives + in-process broker

Zero smithay deps. Provides the SASL handshake scanner, DBus v1 frame
parser + encoder, per-connection byte-stream state machine, fcitx5
frontend classifier / reply synthesis, **and** the full in-process
broker IO loop (listener, upstream dialing, per-connection pumps with
`SCM_RIGHTS` fd passing, fcitx5 signal emitters).

History: started out as a subprocess (`emskin-dbus-proxy` binary) +
JSON ctl socket for cursor-coord rewrite. M1 pulled the broker
in-process under `emskin/src/dbus_broker/`. M2 replaced the
cursor-rewrite hack with a full fcitx5 DBus frontend intercept (B1).
M3 added `SCM_RIGHTS` fd passing so portal.Secret / portal.FileChooser
clients work (Feishu's `RetrieveSecret` was the canary). M4 moved the
broker out of `emskin/` and into this crate's `proxy/` module, since
it has no emskin / smithay deps — just the wire primitives in this
same crate plus libc.

## Module layout

```
src/
├── lib.rs       # crate root + ergonomic re-exports
├── wire/        # DBus wire format (zero-cost over `zvariant`)
│   ├── mod.rs
│   ├── frame.rs # Frame, FrameBuilder, BodyBuilder, Headers, MessageKind,
│   │           # FieldCode, SerialCounter, FrameError
│   └── sasl.rs  # SASL handshake scanner (find_begin_end)
├── broker/      # per-connection byte-stream state machine
│   ├── mod.rs
│   └── state.rs # ConnectionState, FeedOutcome, BrokerError
├── fcitx.rs     # fcitx5 frontend: predicates + classify + IC allocator
│                # + build_reply, all in one ~700-line module since the
│                # surface is small and single-purpose.
└── proxy/       # in-process broker IO loop (listener, upstream dial,
    ├── mod.rs   # per-connection pumps, fcitx5 intercept + signal emit)
    ├── cmsg.rs  # recvmsg/sendmsg + SCM_RIGHTS fd passing
    └── signals.rs # build_preedit_chunks (UpdateFormattedPreedit chunks)
```

## Scope matrix

| Feature | Done | Future |
|---|---|---|
| SASL handshake scanner (`wire/sasl.rs`) | ✅ | |
| DBus v1 frame parser + encoder (`wire/frame.rs`) | ✅ | |
| Per-connection state machine (`broker/state.rs`) | ✅ | |
| Fcitx5 method_call classifier (`fcitx/classify.rs`) | ✅ | |
| Per-connection fcitx5 IC registry (`fcitx/ic.rs`) | ✅ | |
| Fcitx5 method_return synthesis (`fcitx/reply.rs`) | ✅ | |
| In-process broker IO loop (`proxy/mod.rs`) | ✅ | |
| `SCM_RIGHTS` fd passing (`proxy/cmsg.rs`) | ✅ | |
| `RequestName` local-own interception → closes emskin#60 | | ✅ |
| `ListNames` / `NameOwnerChanged` merging for policy | | ✅ |

## Architecture

```
embedded app (WeChat / Emacs pgtk / Electron / Feishu)
       │
       │ DBus (bus.sock injected via DBUS_SESSION_BUS_ADDRESS)
       ▼
┌──────────── emskin-dbus::proxy ─────────────┐
│  DbusBroker (recvmsg/sendmsg + SCM_RIGHTS)  │
│    ├─ ConnectionState (wire/sasl + frames)  │
│    ├─ fcitx::classify (InputMethod1 /       │
│    │                   InputContext1)       │
│    ├─ fcitx::build_reply (method_return)    │
│    └─ FrameBuilder::signal                  │
│        (CommitString / UpdateFormattedPreedit)│
└──────┬──────────────────────────────────────┘
       │ non-fcitx5 methods pass through, fds round-trip
       ▼
  upstream host session bus (real fcitx5 stays untouched)
```

The consumer crate (e.g. `emskin`) wires the broker's listener fd and
each accepted connection's two fds (`client`, `upstream`) into its
event loop. From this crate's perspective those fds are just data —
calloop / mio / tokio all work the same. Tests use plain
`std::os::unix::net::socketpair` and step the pumps manually.

## Invariants

- **Parser is append-only.** `ConnectionState::feed_from_client(chunk)`
  must be called with successive socket reads; internally buffers
  partial messages. The returned `FeedOutcome.outbound` is the *exact*
  byte sequence to write to the other side — intercept sites filter
  it, not mutate it in place.
- **Encoder is little-endian only.** The parser still accepts
  big-endian input for messages the broker forwards verbatim; anything
  the broker synthesizes itself is LE because every modern Linux DBus
  client is LE and there's no value in the extra path.
- **Signals need a unique-name sender.** `fcitx::build_reply` does not
  set sender — the broker owns that (the caller tracks the real
  fcitx5 unique name via GetNameOwner-reply parsing +
  NameOwnerChanged refresh) and stamps it on the signal frame before
  encoding.
- **IC paths are opaque, not state.** `InputContextAllocator::allocate`
  hands out `(path, uuid)` for the `CreateInputContext` reply and
  forgets immediately — no per-IC state lives in the broker. emskin's
  IME state lives in `winit` + `ImeBridge`, driven by the FcitxEvent
  stream from `dbus_broker::emit_fcitx_event`. Ids are per-connection
  and monotonic so client-side stale references can't collide.
- **Serials are non-zero.** `SerialCounter::bump` skips zero on wrap;
  `next_serial == 0` violates the DBus spec and lockstep clients
  reject the frame.
- **Preedit format flags** (per fcitx5's `FcitxTextFormatFlag`,
  `fcitx-utils/textformatflags.h`): `Underline = 1 << 3`,
  `HighLight = 1 << 4`. `UpdateFormattedPreedit` chunks MUST include
  `Underline` or GTK fcitx-gtk renders the preedit as plain inline
  text (no visual distinction from committed content). The active
  segment (from winit's `(begin, end)` cursor range) gets
  `Underline | HighLight` for the inverted-color "currently composing"
  rendering — see `proxy::signals::build_preedit_chunks`.
- **`BareSignature`, not `Value::Signature`, encodes the SIGNATURE
  header.** zvariant 5 wraps multi-element signatures in `()` (it
  models them as an implicit struct); GDBus / fcitx5 reject signal
  bodies whose declared SIGNATURE includes those parens — IM signals
  silently drop. Regression test:
  `wire::frame::tests::signature_field_does_not_wrap_in_parens`.
- **`SCM_RIGHTS` rides one packet at a time.** The proxy's IO uses
  `recvmsg(MSG_CMSG_CLOEXEC)` / `sendmsg`; outbound queues are
  `VecDeque<OutPacket>` where one packet = one DBus message
  (post-SASL) and its declared `unix_fds` ride alongside that
  packet's first byte. On partial write the fds are gone — they were
  delivered with the first byte — so retry sends the remaining bytes
  with no ancillary. Pre-SASL bytes go through as one fd-less packet.

## Non-goals

- No high-level `Proxy` / `ObjectServer` API. This is raw-byte
  primitives for a broker, not a DBus service library.
- No activation fork-exec logic — all activation stays on the host bus.
- No policy / sandbox filtering. xdg-dbus-proxy's security model is
  out of scope; we use the same DBus-parsing techniques but the
  "what's allowed" question is fully answered by "emskin only
  intercepts fcitx5 interfaces, forwards everything else verbatim".
