//! In-process DBus broker — calloop-driven replacement for the
//! `emskin-dbus-proxy` subprocess.
//!
//! # Responsibilities
//!
//! - Bind a Unix socket inside `$XDG_RUNTIME_DIR/emskin-dbus-<pid>/bus.sock`
//!   that embedded apps dial via `DBUS_SESSION_BUS_ADDRESS`.
//! - For each accepted client, dial the real upstream session bus.
//! - Drive both halves of the pair via non-blocking reads + write buffers.
//! - On the `client → bus` direction, apply the cursor-coord rewrite from
//!   [`emskin_dbus::fcitx`] classifier + reply synthesizer.
//!
//! # What this is **not** (yet)
//!
//! This module is wired up in a follow-up commit. Right now it only
//! provides the plumbing — the existing `DbusBridge::spawn_and_connect`
//! subprocess path stays the source of truth until the switch-over.
//!
//! # Design choices
//!
//! - The broker struct owns fds and protocol state; the calloop glue lives
//!   in `main.rs` (`register_dbus_sources` in commit 3) so the broker has
//!   zero calloop dep. This keeps it unit-testable with plain
//!   `socketpair()`.
//! - `offset` is a plain `Option<(i32, i32)>` — no `Arc<Mutex>`, because
//!   every callback runs on the event loop thread.
//! - Writes use a `VecDeque<u8>` back-pressure buffer per direction,
//!   mirroring [`crate::ipc::IpcServer`]'s pattern. If the peer isn't
//!   readable, bytes sit in the buffer until it is.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use emskin_dbus::fcitx::{self, INPUT_CONTEXT_INTERFACE};
use emskin_dbus::{
    ConnectionState, Fcitx5MethodCall, Frame, FrameBuilder, InputContextAllocator, MessageKind,
    SerialCounter,
};

mod signals;
use signals::{build_preedit_chunks, HIGHLIGHT, UNDERLINE};

/// Sender name we stamp on synthesized signals. GDBus (and most other
/// DBus libraries) filter incoming signals against the `sender=`
/// clause of AddMatch rules — WeChat / GTK IM module's match rules
/// typically look like `sender='org.fcitx.Fcitx5'`, so emitting with
/// an empty sender causes the client library to drop the signal on
/// the floor. Using the well-known name (not a `:1.N` unique name)
/// matches what clients configure.
const SIGNAL_SENDER: &str = "org.fcitx.Fcitx5";

/// Newtype for per-connection id. Generated sequentially by the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId(u64);

impl ConnId {
    /// Construct a `ConnId` for tests (ImeBridge unit tests need to
    /// build owner identities without a live broker).
    #[cfg(test)]
    pub(crate) fn new_for_test(n: u64) -> Self {
        Self(n)
    }
}

/// Returned by [`DbusBroker::accept_one`]. Caller (calloop glue in
/// `main.rs`) uses the fds to register the client + upstream sockets as
/// separate Generic sources. `id` identifies the pair for subsequent pump
/// / flush calls.
pub struct ConnAccepted {
    pub id: ConnId,
    pub client_fd: RawFd,
    pub upstream_fd: RawFd,
}

/// Per-tick outcome from a pump call. Callers use this to decide whether
/// to drop the connection (on `PeerClosed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpOutcome {
    /// Read more bytes, connection still live.
    Active,
    /// EOF on the side we just read from — the pair is dead, caller
    /// should remove both calloop sources and drop the connection.
    PeerClosed,
}

/// Side-channel events emitted by the broker when it observes
/// fcitx5 state changes on one of its intercepted connections.
/// Drained by `emskin`'s tick loop via
/// [`DbusBroker::drain_events`].
///
/// These are *not* DBus messages — they're a typed view onto the
/// state changes the broker saw so emskin can drive winit IME
/// without re-parsing DBus bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FcitxEvent {
    /// Client's IC called `FocusIn` (`focused=true`) or `FocusOut`
    /// (`focused=false`). The broker no longer carries a stored
    /// cursor rect across focus events — `ImeBridge` falls back to
    /// `(origin, 1×1)` until the next `CursorRect` arrives, which
    /// real GTK / Qt clients always send right after `FocusIn`.
    FocusChanged {
        conn: ConnId,
        ic_path: String,
        focused: bool,
    },
    /// Client's IC reported a new cursor rectangle (in its own
    /// surface-local coords). `[x, y, w, h]`.
    CursorRect {
        conn: ConnId,
        ic_path: String,
        rect: [i32; 4],
    },
    /// Client destroyed an IC. Emskin should tear down any winit IME
    /// state tied to it.
    IcDestroyed { conn: ConnId, ic_path: String },
}

struct Connection {
    client: UnixStream,
    upstream: UnixStream,
    state: ConnectionState,
    /// Bytes waiting to be written to `client` (came from upstream
    /// or synthesized by us intercepting fcitx5 methods).
    client_out: VecDeque<u8>,
    /// Bytes waiting to be written to `upstream` (came from client,
    /// minus any fcitx5 method_calls we intercepted).
    upstream_out: VecDeque<u8>,
    /// Allocator for synthetic IC paths handed back in
    /// `CreateInputContext` replies. Holds no per-IC state — see
    /// `emskin_dbus::fcitx::ic` for the rationale.
    ic_allocator: InputContextAllocator,
    /// Monotonic outgoing-serial counter for broker-synthesized
    /// method_returns / signals on this connection.
    serial_counter: SerialCounter,
    /// Best-guess unique name for fcitx5 as the client knows it.
    /// Populated via two independent paths so whichever observes it
    /// first wins:
    ///   1. `GetNameOwner` replies from upstream (`upstream_buf` +
    ///      `pending_name_lookups` below) — the authoritative source.
    ///   2. The `destination` field of intercepted fcitx5 method_calls
    ///      — a fallback for clients that skip GetNameOwner or whose
    ///      reply we missed due to timing.
    ///
    /// Used as `sender` on broker-synthesized signals (`CommitString`,
    /// `UpdateFormattedPreedit`). Per xdg-dbus-proxy (flatpak-proxy.c:
    /// 2708), client libraries filter signals against the unique name
    /// their match rule's `sender=` clause resolves to, so getting
    /// this right is what makes commits actually reach the client.
    fcitx_server_name: Option<String>,
    /// Buffer for incremental parsing of the bus → client byte stream.
    /// Fed only after `state.is_authenticated()` is true — before that the
    /// upstream is still in SASL mode (`OK <guid>\r\n`, `DATA`, …) and
    /// those bytes shouldn't be parsed as DBus messages. Those bytes
    /// are still forwarded to the client; we just don't inspect them.
    upstream_buf: Vec<u8>,
    /// Outstanding `GetNameOwner` requests the client has sent. Keyed
    /// by the method_call's serial, value is the well-known name the
    /// client was asking about. When the matching reply comes back
    /// from upstream (reply_serial == this key), we parse its string
    /// body to learn the unique name owner — and, if the looked-up
    /// name is a fcitx5 well-known, cache it in `fcitx_server_name`.
    pending_name_lookups: HashMap<u32, String>,
}

/// The in-process broker. Holds the listener, the upstream bus path
/// for per-connection dials, and all active connection state.
pub struct DbusBroker {
    listen_path: PathBuf,
    listener: UnixListener,
    upstream_path: PathBuf,
    connections: HashMap<ConnId, Connection>,
    next_id: u64,
    /// Queued fcitx5-observation events (FocusChanged, CursorRect,
    /// IcDestroyed). Drained by emskin each tick.
    events: Vec<FcitxEvent>,
}

impl DbusBroker {
    /// Bind `session_dir/bus.sock` as the listener. `upstream` is the
    /// path of the real session bus — either parsed from
    /// `DBUS_SESSION_BUS_ADDRESS=unix:path=…` or passed in directly in
    /// tests.
    pub fn bind(session_dir: &Path, upstream: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(session_dir)?;
        let listen_path = session_dir.join("bus.sock");
        // Reuse of a stale socket (from a crashed prior emskin) is safe
        // because we own the session dir; unlink first then bind.
        let _ = std::fs::remove_file(&listen_path);
        let listener = UnixListener::bind(&listen_path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listen_path,
            listener,
            upstream_path: upstream,
            connections: HashMap::new(),
            next_id: 1,
            events: Vec::new(),
        })
    }

    pub fn listen_path(&self) -> &Path {
        &self.listen_path
    }

    pub fn listener_fd(&self) -> RawFd {
        self.listener.as_raw_fd()
    }

    /// Accept one pending connection, dial upstream, register state.
    /// Returns `Ok(None)` when the listener has no pending connection
    /// (WouldBlock) — the calloop source is level-triggered so we'll be
    /// called again on the next ready event.
    ///
    /// On upstream dial failure we drop the accepted client; the embedded
    /// app will see its first `write()` fail. Alternative would be to
    /// keep a half-open connection, but DBus clients don't have a story
    /// for "half-dialed bus" so fail-fast is kinder.
    pub fn accept_one(&mut self) -> io::Result<Option<ConnAccepted>> {
        let client = match self.listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        };
        let upstream = match UnixStream::connect(&self.upstream_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    upstream = ?self.upstream_path,
                    "dbus broker: upstream dial failed; dropping client"
                );
                return Ok(None);
            }
        };
        client.set_nonblocking(true)?;
        upstream.set_nonblocking(true)?;

        let id = ConnId(self.next_id);
        self.next_id += 1;
        let client_fd = client.as_raw_fd();
        let upstream_fd = upstream.as_raw_fd();

        self.connections.insert(
            id,
            Connection {
                client,
                upstream,
                state: ConnectionState::new(),
                client_out: VecDeque::new(),
                upstream_out: VecDeque::new(),
                ic_allocator: InputContextAllocator::new(),
                serial_counter: SerialCounter::new(),
                fcitx_server_name: None,
                upstream_buf: Vec::new(),
                pending_name_lookups: HashMap::new(),
            },
        );

        tracing::debug!(?id, "dbus broker: connection accepted");
        Ok(Some(ConnAccepted {
            id,
            client_fd,
            upstream_fd,
        }))
    }

    /// Client → upstream pump. Reads all readable bytes from the client,
    /// feeds them through the DBus state machine, and for each
    /// observed message decides:
    ///
    /// - **Intercept** (fcitx5 method_calls) — build a synthetic
    ///   `method_return` via [`fcitx::build_reply`], enqueue to
    ///   `client_out`, emit a typed [`FcitxEvent`] for emskin to
    ///   consume, and **don't** forward the bytes to upstream.
    /// - **Track** (`GetNameOwner` for fcitx5 well-knowns) — record
    ///   the request serial so the matching upstream reply can be
    ///   matched and the fcitx5 unique name extracted for signal
    ///   emission. Still forwarded.
    /// - **Forward verbatim** — every other message.
    pub fn pump_client_to_upstream(&mut self, id: ConnId) -> io::Result<PumpOutcome> {
        // Split-borrow so we can touch `self.events` while `conn` is
        // live.
        let Self {
            connections,
            events,
            ..
        } = self;
        let Some(conn) = connections.get_mut(&id) else {
            return Ok(PumpOutcome::PeerClosed);
        };

        let mut buf = [0u8; 8 * 1024];
        let n = match conn.client.read(&mut buf) {
            Ok(0) => return Ok(PumpOutcome::PeerClosed),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(PumpOutcome::Active),
            Err(e) if e.kind() == ErrorKind::Interrupted => return Ok(PumpOutcome::Active),
            Err(e) => return Err(e),
        };

        let out = conn
            .state
            .feed_from_client(&buf[..n])
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;

        // Fast path: no messages (handshake bytes only). Forward
        // verbatim — nothing to inspect or intercept.
        if out.frame_ranges.is_empty() {
            conn.upstream_out.extend(out.outbound);
            Self::try_flush(&mut conn.upstream, &mut conn.upstream_out)?;
            return Ok(PumpOutcome::Active);
        }

        // Slow path: walk every message, decide its disposition. Any
        // bytes before the first parsed message (auth tail, if the
        // BEGIN landed in this same chunk) are prepended verbatim.
        let mut forwarded = Vec::with_capacity(out.outbound.len());
        forwarded.extend_from_slice(&out.outbound[..out.frame_ranges[0].start]);

        for msg in &out.frame_ranges {
            let msg_bytes = &out.outbound[msg.start..msg.end];
            let frame = match Frame::parse(msg_bytes) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(?id, error = %e, "failed to parse client → bus frame; forwarding verbatim");
                    forwarded.extend_from_slice(msg_bytes);
                    continue;
                }
            };
            tracing::trace!(
                member = frame.headers.member.as_deref().unwrap_or(""),
                interface = frame.headers.interface.as_deref().unwrap_or(""),
                signature = frame.headers.signature.as_deref().unwrap_or(""),
                destination = frame.headers.destination.as_deref().unwrap_or(""),
                body_len = frame.body.len(),
                "client → bus message"
            );

            // Track outgoing `org.freedesktop.DBus.GetNameOwner(s)`
            // requests that ask about a fcitx5 well-known name. When
            // the matching reply comes back from upstream we'll learn
            // the unique owner and cache it as our signal sender.
            if is_get_name_owner_method(&frame) {
                if let Some(name) = frame.decode_body::<String>() {
                    if fcitx::is_fcitx_well_known(&name) {
                        tracing::debug!(
                            ?id,
                            serial = frame.serial,
                            name,
                            "tracking GetNameOwner for fcitx5 name"
                        );
                        conn.pending_name_lookups.insert(frame.serial, name);
                    }
                }
            }

            if let Some(fm) = fcitx::classify(&frame) {
                // Capture the destination the client used as a
                // fallback signal-sender source — the upstream
                // GetNameOwner parse in `pump_upstream_to_client` is
                // authoritative, but if for some reason that didn't
                // land (e.g. the client skipped GetNameOwner and just
                // used the well-known), the destination field of an
                // intercepted call still tells us the unique name.
                if let Some(dest) = frame.headers.destination.as_deref() {
                    if dest.starts_with(':') && conn.fcitx_server_name.as_deref() != Some(dest) {
                        tracing::debug!(
                            ?id,
                            dest,
                            "captured fcitx5 unique name from method_call destination"
                        );
                        conn.fcitx_server_name = Some(dest.to_string());
                    }
                }
                let reply = fcitx::build_reply(
                    &frame,
                    &fm,
                    &mut conn.ic_allocator,
                    &mut conn.serial_counter,
                );
                conn.client_out.extend(reply);
                Self::emit_fcitx_event(events, id, &fm);
                tracing::debug!(
                    ?id,
                    member = frame.headers.member.as_deref().unwrap_or(""),
                    "intercepted fcitx5 method_call; reply queued"
                );
                continue;
            }

            // Not fcitx5 — forward verbatim.
            forwarded.extend_from_slice(msg_bytes);
        }

        conn.upstream_out.extend(forwarded);
        Self::try_flush(&mut conn.upstream, &mut conn.upstream_out)?;
        // Also flush client_out now — our intercepted replies shouldn't
        // wait for the peer's next wakeup to reach the client.
        Self::try_flush(&mut conn.client, &mut conn.client_out)?;
        Ok(PumpOutcome::Active)
    }

    /// Map a classified fcitx5 method_call to a [`FcitxEvent`] and
    /// push onto the broker's event queue. Most methods emit no
    /// event; only focus + cursor + destroy are interesting.
    fn emit_fcitx_event(events: &mut Vec<FcitxEvent>, conn: ConnId, method: &Fcitx5MethodCall) {
        match method {
            Fcitx5MethodCall::FocusIn {
                input_context_path: ic_path,
            } => {
                events.push(FcitxEvent::FocusChanged {
                    conn,
                    ic_path: ic_path.clone(),
                    focused: true,
                });
            }
            Fcitx5MethodCall::FocusOut {
                input_context_path: ic_path,
            } => {
                events.push(FcitxEvent::FocusChanged {
                    conn,
                    ic_path: ic_path.clone(),
                    focused: false,
                });
            }
            Fcitx5MethodCall::SetCursorRect {
                input_context_path: ic_path,
                x,
                y,
                w,
                h,
            } => events.push(FcitxEvent::CursorRect {
                conn,
                ic_path: ic_path.clone(),
                rect: [*x, *y, *w, *h],
            }),
            Fcitx5MethodCall::SetCursorRectV2 {
                input_context_path: ic_path,
                x,
                y,
                w,
                h,
                scale,
            } => {
                // V2 reports device pixels; `scale` is device-per-logical.
                // winit's `set_ime_cursor_area` takes `LogicalPosition`, so
                // we convert here. `scale <= 0` is a malformed body — fall
                // back to 1.0 so we at least pass a sane value through.
                let s = if *scale > 0.0 { *scale } else { 1.0 };
                let to_logical = |v: i32| -> i32 { (v as f64 / s).round() as i32 };
                events.push(FcitxEvent::CursorRect {
                    conn,
                    ic_path: ic_path.clone(),
                    rect: [
                        to_logical(*x),
                        to_logical(*y),
                        to_logical(*w),
                        to_logical(*h),
                    ],
                });
            }
            Fcitx5MethodCall::SetCursorLocation {
                input_context_path: ic_path,
                x,
                y,
            } => events.push(FcitxEvent::CursorRect {
                conn,
                ic_path: ic_path.clone(),
                rect: [*x, *y, 0, 0],
            }),
            Fcitx5MethodCall::DestroyIC {
                input_context_path: ic_path,
            } => events.push(FcitxEvent::IcDestroyed {
                conn,
                ic_path: ic_path.clone(),
            }),
            // CreateInputContext / Reset / SetCapability / ProcessKeyEvent
            // / SetSurroundingText[Position] don't change state we need
            // emskin to react to.
            _ => {}
        }
    }

    /// Drain every queued fcitx5 event. Called by emskin's tick loop;
    /// empties the internal queue.
    pub fn drain_events(&mut self) -> Vec<FcitxEvent> {
        std::mem::take(&mut self.events)
    }

    /// Send an `org.fcitx.Fcitx.InputContext1.CommitString(s)` signal to
    /// the given connection's client, targeted at `ic_path`. Used by
    /// emskin's winit IME handler to relay `Ime::Commit` text back to
    /// the DBus client that owns the active IC.
    pub fn emit_commit_string(
        &mut self,
        conn: ConnId,
        ic_path: &str,
        text: &str,
    ) -> io::Result<()> {
        let Some(c) = self.connections.get_mut(&conn) else {
            return Ok(());
        };
        let serial = c.serial_counter.bump();
        let sender = c.fcitx_server_name.as_deref().unwrap_or(SIGNAL_SENDER);
        let frame = FrameBuilder::signal(ic_path, INPUT_CONTEXT_INTERFACE, "CommitString")
            .serial(serial)
            .sender(sender)
            .body(&text.to_string())
            .build();
        tracing::trace!(?conn, ic_path, text, sender, "emit CommitString signal");
        c.client_out.extend(frame.encode());
        Self::try_flush(&mut c.client, &mut c.client_out)
    }

    /// Send an `org.fcitx.Fcitx.InputContext1.UpdateFormattedPreedit(a(si)i)`
    /// signal — relays `Ime::Preedit` back to the DBus client so it can
    /// render inline preedit.
    ///
    /// `cursor` is the `(begin, end)` byte range that winit reports for
    /// the active preedit segment; when `begin != end` we split the
    /// text into three chunks so the active segment carries the
    /// `HighLight` flag — that's what GTK fcitx-gtk uses to render the
    /// inverted-color "currently composing" segment, matching what
    /// pgtk Emacs gets natively via text_input_v3. `None` → single
    /// chunk, plain underline; cursor offset encoded as `-1`.
    pub fn emit_preedit(
        &mut self,
        conn: ConnId,
        ic_path: &str,
        text: &str,
        cursor: Option<(i32, i32)>,
    ) -> io::Result<()> {
        let Some(c) = self.connections.get_mut(&conn) else {
            return Ok(());
        };
        let serial = c.serial_counter.bump();
        let sender = c.fcitx_server_name.as_deref().unwrap_or(SIGNAL_SENDER);
        let chunks = build_preedit_chunks(text, cursor, UNDERLINE, HIGHLIGHT);
        // Cursor field in the wire body is a single offset into the
        // concatenated preedit text. We use `end` so the caret sits at
        // the right edge of the highlighted segment, which matches
        // fcitx5's own convention.
        let cursor_offset = cursor.map(|(_, e)| e).unwrap_or(-1);
        let frame =
            FrameBuilder::signal(ic_path, INPUT_CONTEXT_INTERFACE, "UpdateFormattedPreedit")
                .serial(serial)
                .sender(sender)
                .body_args()
                .arg(&chunks)
                .arg(&cursor_offset)
                .finish()
                .build();
        tracing::trace!(
            ?conn,
            ic_path,
            text,
            sender,
            chunks_n = chunks.len(),
            cursor_offset,
            "emit UpdateFormattedPreedit signal"
        );
        c.client_out.extend(frame.encode());
        Self::try_flush(&mut c.client, &mut c.client_out)
    }

    /// Upstream → client pump. Forwards bytes verbatim, and (once the
    /// client is past SASL) ALSO parses them as DBus messages so we
    /// can observe replies to our outstanding GetNameOwner requests
    /// and learn the fcitx5 unique-name mapping.
    ///
    /// Forwarding is independent of parsing — even if the parser
    /// errors on a malformed upstream reply, the bytes still reach
    /// the client. We just stop observing.
    pub fn pump_upstream_to_client(&mut self, id: ConnId) -> io::Result<PumpOutcome> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(PumpOutcome::PeerClosed);
        };
        let mut buf = [0u8; 8 * 1024];
        let n = match conn.upstream.read(&mut buf) {
            Ok(0) => return Ok(PumpOutcome::PeerClosed),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(PumpOutcome::Active),
            Err(e) if e.kind() == ErrorKind::Interrupted => return Ok(PumpOutcome::Active),
            Err(e) => return Err(e),
        };
        // Forward verbatim first — preserve existing behavior.
        conn.client_out.extend(&buf[..n]);
        Self::try_flush(&mut conn.client, &mut conn.client_out)?;

        // Observe post-SASL bytes. `state.is_authenticated()` only flips once
        // the client's `BEGIN\r\n` has crossed the wire, which happens
        // AFTER upstream has finished its `OK <guid>\r\n` handshake —
        // so once authed, every byte arriving from upstream is a DBus
        // v1 message frame.
        if !conn.state.is_authenticated() {
            return Ok(PumpOutcome::Active);
        }
        conn.upstream_buf.extend_from_slice(&buf[..n]);
        loop {
            let total = match Frame::bytes_needed(&conn.upstream_buf) {
                Ok(None) => break,
                Ok(Some(n)) => n,
                Err(e) => {
                    tracing::warn!(?id, error = %e, "upstream parser: giving up on this conn");
                    conn.upstream_buf.clear();
                    break;
                }
            };
            if conn.upstream_buf.len() < total {
                break;
            }
            let bytes = conn.upstream_buf.drain(..total).collect::<Vec<u8>>();
            let frame = match Frame::parse(&bytes) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(?id, error = %e, "upstream parser: bad frame");
                    continue;
                }
            };
            match frame.kind {
                // Reply to an outgoing GetNameOwner we tracked: parse
                // the single-string body to learn the unique-name
                // owner. Authoritative source — overwrites any earlier
                // guess from the destination-capture path.
                MessageKind::MethodReturn => {
                    let Some(reply_serial) = frame.headers.reply_serial else {
                        continue;
                    };
                    let Some(looked_up) = conn.pending_name_lookups.remove(&reply_serial) else {
                        continue;
                    };
                    let Some(owner) = frame.decode_body::<String>() else {
                        tracing::warn!(
                            ?id,
                            reply_serial,
                            looked_up,
                            "GetNameOwner reply body not a string; skipping"
                        );
                        continue;
                    };
                    tracing::trace!(
                        ?id,
                        name = looked_up,
                        owner,
                        "resolved fcitx5 unique owner via GetNameOwner reply"
                    );
                    conn.fcitx_server_name = Some(owner);
                }
                // `org.freedesktop.DBus.NameOwnerChanged(sss)` signal
                // — fired by the daemon when a well-known name's
                // unique owner changes (e.g. real fcitx5 restarted).
                // We refresh / invalidate our cache so signals emitted
                // after the change carry the correct sender.
                MessageKind::Signal if is_name_owner_changed_signal(&frame) => {
                    let Some((name, _old, new)) = frame.decode_body::<(String, String, String)>()
                    else {
                        continue;
                    };
                    if !fcitx::is_fcitx_well_known(&name) {
                        continue;
                    }
                    if new.is_empty() {
                        tracing::trace!(
                            ?id,
                            name,
                            "fcitx5 owner went away (NameOwnerChanged, new_owner empty)"
                        );
                        conn.fcitx_server_name = None;
                    } else {
                        tracing::trace!(
                            ?id,
                            name,
                            new_owner = new,
                            "fcitx5 owner changed (NameOwnerChanged)"
                        );
                        conn.fcitx_server_name = Some(new);
                    }
                }
                _ => {}
            }
        }
        Ok(PumpOutcome::Active)
    }

    /// Retry draining the upstream_out buffer after a prior WouldBlock.
    /// Wired to a WRITE-interest calloop source by the glue layer.
    pub fn flush_upstream_out(&mut self, id: ConnId) -> io::Result<bool> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(false);
        };
        Self::try_flush(&mut conn.upstream, &mut conn.upstream_out)?;
        Ok(!conn.upstream_out.is_empty())
    }

    /// Symmetric partner to [`Self::flush_upstream_out`] for the other
    /// direction.
    pub fn flush_client_out(&mut self, id: ConnId) -> io::Result<bool> {
        let Some(conn) = self.connections.get_mut(&id) else {
            return Ok(false);
        };
        Self::try_flush(&mut conn.client, &mut conn.client_out)?;
        Ok(!conn.client_out.is_empty())
    }

    /// Drop connection state. Caller is responsible for removing the two
    /// calloop sources first — this only frees the fds and the parser.
    pub fn remove_connection(&mut self, id: ConnId) {
        if self.connections.remove(&id).is_some() {
            tracing::debug!(?id, "dbus broker: connection removed");
        }
    }

    /// Write as many bytes from `buf` to `stream` as the kernel will
    /// take without blocking. Leftover stays in `buf`. Matches the
    /// pattern in [`crate::ipc::connection::IpcConn::try_flush`].
    fn try_flush(stream: &mut UnixStream, buf: &mut VecDeque<u8>) -> io::Result<()> {
        while !buf.is_empty() {
            let (front, back) = buf.as_slices();
            let slice = if !front.is_empty() { front } else { back };
            match stream.write(slice) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    buf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

impl Drop for DbusBroker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.listen_path);
    }
}

/// Recognize `org.freedesktop.DBus.GetNameOwner(s)` method_calls so
/// the broker can track the request and match the eventual reply's
/// unique-name body back to the looked-up well-known.
fn is_get_name_owner_method(frame: &Frame<'_>) -> bool {
    frame.headers.interface.as_deref() == Some("org.freedesktop.DBus")
        && frame.headers.member.as_deref() == Some("GetNameOwner")
        && frame.headers.signature.as_deref() == Some("s")
}

/// Recognize `org.freedesktop.DBus.NameOwnerChanged(sss)` signals
/// from the daemon so the broker can refresh the cached fcitx5
/// unique name after a service restart.
fn is_name_owner_changed_signal(frame: &Frame<'_>) -> bool {
    frame.headers.interface.as_deref() == Some("org.freedesktop.DBus")
        && frame.headers.member.as_deref() == Some("NameOwnerChanged")
        && frame.headers.signature.as_deref() == Some("sss")
}

/// Parse `unix:path=/run/user/1000/bus[,guid=…]` into the filesystem
/// path. Mirrors the parser in the old `emskin-dbus-proxy` binary but
/// lives alongside the broker now.
pub fn parse_unix_bus_address(addr: &str) -> io::Result<PathBuf> {
    const PREFIX: &str = "unix:path=";
    let stripped = addr.strip_prefix(PREFIX).ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported bus scheme: {addr}"),
        )
    })?;
    let path = stripped.split(',').next().unwrap_or(stripped);
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn parses_plain_unix_path_form() {
        let p = parse_unix_bus_address("unix:path=/run/user/1000/bus").unwrap();
        assert_eq!(p, PathBuf::from("/run/user/1000/bus"));
    }

    #[test]
    fn parses_unix_path_with_guid_suffix() {
        let p = parse_unix_bus_address("unix:path=/run/user/1000/bus,guid=deadbeef").unwrap();
        assert_eq!(p, PathBuf::from("/run/user/1000/bus"));
    }

    #[test]
    fn rejects_tcp_scheme() {
        assert!(parse_unix_bus_address("tcp:host=localhost,port=1234").is_err());
    }

    /// Helper: accept a client pair against a fake upstream listener.
    /// Returns (broker, client-side stream, upstream-side stream,
    /// conn id). Caller writes to `client`, reads from `upstream`.
    fn setup_pair(
        session: &Path,
        upstream_path: PathBuf,
        upstream_listener: &UnixListener,
    ) -> (DbusBroker, UnixStream, UnixStream, ConnId) {
        let mut broker = DbusBroker::bind(session, upstream_path).unwrap();
        let client = UnixStream::connect(broker.listen_path()).unwrap();
        client.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(20));
        let accepted = broker.accept_one().unwrap().expect("accept ready");
        let (upstream_peer, _) = upstream_listener.accept().unwrap();
        upstream_peer.set_nonblocking(true).unwrap();
        (broker, client, upstream_peer, accepted.id)
    }

    /// Drain all pending reads from a non-blocking stream until it
    /// WouldBlock. Retries a few times to let the broker pump.
    fn drain(stream: &mut UnixStream) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        for _ in 0..5 {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5))
                }
                Err(_) => break,
            }
        }
        got
    }

    /// Intercepted fcitx5 methods don't reach upstream; instead the
    /// broker synthesizes a method_return and writes it back to the
    /// client. Verifies the SetCursorRect path is now Intercept.
    #[test]
    fn set_cursor_rect_is_intercepted_not_forwarded() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, mut upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let call = build_set_cursor_rect(7, (100, 200, 10, 20));
        let mut payload = Vec::from(&handshake[..]);
        payload.extend_from_slice(&call);
        client.write_all(&payload).unwrap();

        for _ in 0..5 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        // Upstream should only see the handshake — the SetCursorRect
        // was intercepted.
        let upstream_got = drain(&mut upstream_peer);
        assert_eq!(
            upstream_got, handshake,
            "upstream should see only the handshake; SetCursorRect was intercepted"
        );

        // Client should receive our synthesized method_return.
        let client_got = drain(&mut client);
        assert!(!client_got.is_empty(), "client should have a reply");
        let reply = emskin_dbus::wire::frame::Frame::parse(&client_got).unwrap();
        assert_eq!(reply.headers.reply_serial, Some(7));
        assert_eq!(reply.body.len(), 0); // empty body

        // And a CursorRect event should be on the queue.
        let events = broker.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            FcitxEvent::CursorRect {
                conn: id,
                ic_path: "/a".into(),
                rect: [100, 200, 10, 20],
            }
        );

        broker.remove_connection(id);
    }

    /// A non-fcitx5 method_call (e.g. `Hello` to the DBus daemon)
    /// must still flow through to upstream unchanged. Regression guard
    /// against an over-eager interceptor.
    #[test]
    fn non_fcitx_method_passes_through() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, mut upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let hello = build_hello(99);
        let mut payload = Vec::from(&handshake[..]);
        payload.extend_from_slice(&hello);
        client.write_all(&payload).unwrap();

        for _ in 0..5 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        let upstream_got = drain(&mut upstream_peer);
        assert!(upstream_got.starts_with(handshake));
        let msg_bytes = &upstream_got[handshake.len()..];
        assert_eq!(msg_bytes, hello.as_slice(), "Hello should pass through");
        // Client shouldn't see a reply from us; the upstream bus is
        // responsible for answering Hello.
        let client_got = drain(&mut client);
        assert!(client_got.is_empty(), "broker should not reply to Hello");
        assert!(broker.drain_events().is_empty());

        broker.remove_connection(id);
    }

    /// GetNameOwner round-trip: client asks about
    /// `org.fcitx.Fcitx5`, the broker forwards to upstream, upstream
    /// replies with the real unique name, and the broker caches it on
    /// the connection's `fcitx_server_name`. Covers the main fix for
    /// "signals dropped because sender is well-known not unique".
    #[test]
    fn get_name_owner_reply_caches_fcitx_unique_name() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, mut upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        // Step 1: client sends handshake + GetNameOwner("org.fcitx.Fcitx5")
        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let req = build_get_name_owner(77, "org.fcitx.Fcitx5");
        let mut payload = Vec::from(&handshake[..]);
        payload.extend_from_slice(&req);
        client.write_all(&payload).unwrap();

        for _ in 0..5 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        // Request should have been forwarded to upstream — we don't
        // intercept `org.freedesktop.DBus` methods, only track them.
        let upstream_got = drain(&mut upstream_peer);
        assert!(upstream_got.starts_with(handshake));
        assert!(
            upstream_got.len() > handshake.len(),
            "GetNameOwner should reach upstream"
        );

        // Step 2: upstream replies with method_return carrying the
        // unique name `:1.42`.
        let reply = build_name_owner_reply(77, ":1.42");
        upstream_peer.write_all(&reply).unwrap();

        for _ in 0..5 {
            broker.pump_upstream_to_client(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        // Reply gets forwarded to client (the broker doesn't swallow
        // it — the client still needs to process it for its own
        // bookkeeping).
        let client_got = drain(&mut client);
        assert!(!client_got.is_empty(), "reply should reach client");

        // And the broker should now have cached the unique name for
        // signal emission.
        let c = broker.connections.get(&id).expect("conn alive");
        assert_eq!(c.fcitx_server_name.as_deref(), Some(":1.42"));
        // The pending lookup should have been consumed.
        assert!(c.pending_name_lookups.is_empty());

        broker.remove_connection(id);
    }

    /// NameOwnerChanged refresh: if the broker has cached
    /// `:1.42` as the fcitx5 owner and then the daemon broadcasts
    /// `NameOwnerChanged("org.fcitx.Fcitx5", ":1.42", ":1.73")`
    /// (real fcitx5 restarted), the cache must update to `:1.73`.
    /// Otherwise every subsequent signal we emit has the stale
    /// sender and the client drops it again.
    #[test]
    fn name_owner_changed_signal_refreshes_cached_fcitx_name() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, mut upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        // Handshake first so is_authenticated() flips.
        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        client.write_all(handshake).unwrap();
        for _ in 0..3 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        // Seed the cache by hand — same as if we'd seen the initial
        // GetNameOwner reply.
        broker.connections.get_mut(&id).unwrap().fcitx_server_name = Some(":1.42".into());

        // Daemon broadcasts NameOwnerChanged after fcitx5 restart.
        let sig = build_name_owner_changed("org.fcitx.Fcitx5", ":1.42", ":1.73");
        upstream_peer.write_all(&sig).unwrap();
        for _ in 0..3 {
            broker.pump_upstream_to_client(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        let c = broker.connections.get(&id).unwrap();
        assert_eq!(c.fcitx_server_name.as_deref(), Some(":1.73"));

        broker.remove_connection(id);
    }

    /// NameOwnerChanged with an empty `new_owner` means the service
    /// disappeared — cache should be cleared so we stop using a
    /// dangling sender name.
    #[test]
    fn name_owner_changed_to_empty_clears_cache() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, mut upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        client.write_all(handshake).unwrap();
        for _ in 0..3 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        broker.connections.get_mut(&id).unwrap().fcitx_server_name = Some(":1.42".into());

        let sig = build_name_owner_changed("org.fcitx.Fcitx5", ":1.42", "");
        upstream_peer.write_all(&sig).unwrap();
        for _ in 0..3 {
            broker.pump_upstream_to_client(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        let c = broker.connections.get(&id).unwrap();
        assert_eq!(c.fcitx_server_name, None);

        broker.remove_connection(id);
    }

    /// Non-fcitx5 GetNameOwner lookups (e.g. asking about
    /// `org.freedesktop.Notifications`) should NOT be tracked — the
    /// broker only cares about fcitx5 names.
    #[test]
    fn get_name_owner_for_unrelated_name_is_not_tracked() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, _upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let req = build_get_name_owner(88, "org.freedesktop.Notifications");
        let mut payload = Vec::from(&handshake[..]);
        payload.extend_from_slice(&req);
        client.write_all(&payload).unwrap();

        for _ in 0..5 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        let c = broker.connections.get(&id).expect("conn alive");
        assert!(c.pending_name_lookups.is_empty());

        broker.remove_connection(id);
    }

    /// CreateInputContext: the broker should allocate an IC path,
    /// send back `(o, ay)` in the method_return, and NOT forward to
    /// upstream (real fcitx5 never learns about this client).
    #[test]
    fn create_input_context_is_intercepted_with_oay_reply() {
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        let upstream_path = dir.path().join("upstream.sock");
        let upstream_listener = UnixListener::bind(&upstream_path).unwrap();
        upstream_listener.set_nonblocking(true).unwrap();
        let (mut broker, mut client, mut upstream_peer, id) =
            setup_pair(&session, upstream_path, &upstream_listener);

        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let call = build_create_input_context(42);
        let mut payload = Vec::from(&handshake[..]);
        payload.extend_from_slice(&call);
        client.write_all(&payload).unwrap();

        for _ in 0..5 {
            broker.pump_client_to_upstream(id).unwrap();
            thread::sleep(Duration::from_millis(5));
        }

        // Upstream: handshake only.
        let upstream_got = drain(&mut upstream_peer);
        assert_eq!(upstream_got, handshake);

        // Client: method_return with `oay` signature (two top-level
        // args — object path + byte array — not a struct).
        let client_got = drain(&mut client);
        let reply = emskin_dbus::wire::frame::Frame::parse(&client_got).unwrap();
        assert_eq!(reply.headers.reply_serial, Some(42));
        assert_eq!(reply.headers.signature.as_deref(), Some("oay"));

        broker.remove_connection(id);
    }

    // ------- DBus message builders (copied from emskin-dbus io.rs tests) -------

    fn pad_to(out: &mut Vec<u8>, bound: usize) {
        while !out.len().is_multiple_of(bound) {
            out.push(0);
        }
    }

    fn push_string_field(out: &mut Vec<u8>, code: u8, sig: &str, value: &str) {
        pad_to(out, 8);
        out.push(code);
        out.push(sig.len() as u8);
        out.extend_from_slice(sig.as_bytes());
        out.push(0);
        pad_to(out, 4);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
        out.push(0);
    }

    fn push_signature_field(out: &mut Vec<u8>, code: u8, sig: &str) {
        pad_to(out, 8);
        out.push(code);
        out.push(1);
        out.push(b'g');
        out.push(0);
        out.push(sig.len() as u8);
        out.extend_from_slice(sig.as_bytes());
        out.push(0);
    }

    /// A `GetNameOwner(s)` method_call asking the DBus daemon who
    /// owns `name`.
    fn build_get_name_owner(serial: u32, name: &str) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/org/freedesktop/DBus");
        push_string_field(&mut fields, 2, "s", "org.freedesktop.DBus");
        push_string_field(&mut fields, 3, "s", "GetNameOwner");
        push_string_field(&mut fields, 6, "s", "org.freedesktop.DBus");
        push_signature_field(&mut fields, 8, "s");

        // Body: a single `s` arg.
        let mut body = Vec::new();
        body.extend_from_slice(&(name.len() as u32).to_le_bytes());
        body.extend_from_slice(name.as_bytes());
        body.push(0);

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(&body);
        msg
    }

    /// A method_return from the DBus daemon that answers a
    /// `GetNameOwner` request with a unique-name string.
    fn build_name_owner_reply(reply_serial: u32, unique_name: &str) -> Vec<u8> {
        let mut fields = Vec::new();
        // REPLY_SERIAL field: code 5, variant sig "u", u32 value.
        pad_to(&mut fields, 8);
        fields.push(5);
        fields.push(1);
        fields.push(b'u');
        fields.push(0);
        pad_to(&mut fields, 4);
        fields.extend_from_slice(&reply_serial.to_le_bytes());
        push_string_field(&mut fields, 7, "s", "org.freedesktop.DBus"); // SENDER
        push_signature_field(&mut fields, 8, "s"); // body sig

        let mut body = Vec::new();
        body.extend_from_slice(&(unique_name.len() as u32).to_le_bytes());
        body.extend_from_slice(unique_name.as_bytes());
        body.push(0);

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 2, 0, 1]); // type=2 (method_return)
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        // Our own serial for this reply — any non-zero value works.
        msg.extend_from_slice(&9999u32.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(&body);
        msg
    }

    /// A DBus daemon signal `NameOwnerChanged(s, s, s)` announcing an
    /// ownership change for `name` from `old_owner` to `new_owner`.
    fn build_name_owner_changed(name: &str, old_owner: &str, new_owner: &str) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/org/freedesktop/DBus");
        push_string_field(&mut fields, 2, "s", "org.freedesktop.DBus");
        push_string_field(&mut fields, 3, "s", "NameOwnerChanged");
        push_string_field(&mut fields, 7, "s", "org.freedesktop.DBus"); // SENDER
        push_signature_field(&mut fields, 8, "sss");

        let mut body = Vec::new();
        for s in [name, old_owner, new_owner] {
            while !body.len().is_multiple_of(4) {
                body.push(0);
            }
            body.extend_from_slice(&(s.len() as u32).to_le_bytes());
            body.extend_from_slice(s.as_bytes());
            body.push(0);
        }

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 4, 0, 1]); // type=4 signal
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&1234u32.to_le_bytes()); // serial
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(&body);
        msg
    }

    /// A plain DBus `Hello` method_call (goes to the DBus daemon, not
    /// fcitx5 — so it should pass through the broker unchanged).
    fn build_hello(serial: u32) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/org/freedesktop/DBus");
        push_string_field(&mut fields, 2, "s", "org.freedesktop.DBus");
        push_string_field(&mut fields, 3, "s", "Hello");
        push_string_field(&mut fields, 6, "s", "org.freedesktop.DBus");

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&0u32.to_le_bytes()); // body_len
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg
    }

    /// A `CreateInputContext` with an empty `a(ss)` body.
    fn build_create_input_context(serial: u32) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/org/freedesktop/portal/inputmethod");
        push_string_field(&mut fields, 2, "s", "org.fcitx.Fcitx.InputMethod1");
        push_string_field(&mut fields, 3, "s", "CreateInputContext");
        push_string_field(&mut fields, 6, "s", "org.fcitx.Fcitx5");
        push_signature_field(&mut fields, 8, "a(ss)");

        // Body: empty `a(ss)` array. DBus §4.1 requires padding to the
        // first element's alignment even when the array is empty — for
        // `(ss)` that's 8-byte alignment, so an empty body is `len=0` +
        // 4 zero pad bytes. GDBus / fcitx5 always emit this; only ad-hoc
        // hand-rolled bodies omit it (zvariant rejects those).
        let mut body = Vec::new();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[0u8; 4]);

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(&body);
        msg
    }

    fn build_set_cursor_rect(serial: u32, coords: (i32, i32, i32, i32)) -> Vec<u8> {
        let mut fields = Vec::new();
        push_string_field(&mut fields, 1, "o", "/a");
        push_string_field(&mut fields, 2, "s", "org.fcitx.Fcitx.InputContext1");
        push_string_field(&mut fields, 3, "s", "SetCursorRect");
        push_string_field(&mut fields, 6, "s", "org.fcitx.Fcitx5");
        push_signature_field(&mut fields, 8, "iiii");

        let mut body = Vec::new();
        body.extend_from_slice(&coords.0.to_le_bytes());
        body.extend_from_slice(&coords.1.to_le_bytes());
        body.extend_from_slice(&coords.2.to_le_bytes());
        body.extend_from_slice(&coords.3.to_le_bytes());

        let mut msg = Vec::new();
        msg.extend_from_slice(&[b'l', 1, 0, 1]);
        msg.extend_from_slice(&(body.len() as u32).to_le_bytes());
        msg.extend_from_slice(&serial.to_le_bytes());
        msg.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        msg.extend_from_slice(&fields);
        pad_to(&mut msg, 8);
        msg.extend_from_slice(&body);
        msg
    }
}
