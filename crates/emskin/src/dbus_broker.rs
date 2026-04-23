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
//!   [`emskin_dbus::broker::apply_cursor_rewrites`] using [`Self::offset`].
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

use emskin_dbus::broker::{apply_cursor_rewrites, state::ConnectionState};
use emskin_dbus::dbus::encode::{body_preedit, body_string, Signal};
use emskin_dbus::fcitx::{self, reply::next_nonzero, FcitxMethod, IcRegistry, INPUT_CONTEXT_IFACE};

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
    /// (`focused=false`). `rect` is the IC's last-known cursor
    /// rectangle (client-local) — present on `FocusIn` if the client
    /// set one before, otherwise `None`.
    FocusChanged {
        conn: ConnId,
        ic_path: String,
        focused: bool,
        rect: Option<[i32; 4]>,
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
    /// Fcitx5 input contexts this client has registered with us.
    ic_registry: IcRegistry,
    /// Monotonic outgoing-serial counter for broker-synthesized
    /// method_returns / signals on this connection. Starts at 1 (DBus
    /// requires non-zero serials).
    serial_counter: u32,
    /// Best-guess unique name for fcitx5 as the client knows it,
    /// captured from the `destination` field of intercepted fcitx5
    /// method_calls. DBus clients normally call `GetNameOwner` once
    /// to resolve `org.fcitx.Fcitx5 → :1.N` and then use the unique
    /// name as destination for efficiency, so by the time we see
    /// method_calls like `InputContext1.FocusIn`, this field is
    /// populated. Used as the `sender` of broker-synthesized signals
    /// (`CommitString`, `UpdateFormattedPreedit`) — GDBus and friends
    /// filter received signals against the unique name their match
    /// rule's `sender=` clause resolves to, so getting this right is
    /// what makes commits actually reach the client.
    fcitx_server_name: Option<String>,
}

/// The in-process broker. Holds the listener, the upstream bus path for
/// per-connection dials, the shared focus-origin offset, and all active
/// connection state.
pub struct DbusBroker {
    listen_path: PathBuf,
    listener: UnixListener,
    upstream_path: PathBuf,
    offset: Option<(i32, i32)>,
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
            offset: None,
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

    /// Current cursor-rewrite offset. `None` = pass-through.
    pub fn offset(&self) -> Option<(i32, i32)> {
        self.offset
    }

    /// Update the cursor-rewrite offset. Called from the tick's focus
    /// reconciler. `None` disables rewrite.
    pub fn set_offset(&mut self, off: Option<(i32, i32)>) {
        self.offset = off;
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
                ic_registry: IcRegistry::new(),
                serial_counter: 0,
                fcitx_server_name: None,
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
    /// observed message decides between three dispositions:
    ///
    /// 1. **Intercept** (fcitx5 method_calls) — build a synthetic
    ///    `method_return` via `fcitx::build_reply`, enqueue to
    ///    `client_out`, emit a typed [`FcitxEvent`] for emskin to
    ///    consume, and **don't** forward the bytes to upstream.
    /// 2. **Rewrite-and-forward** — for non-intercepted messages with
    ///    a cursor-rewrite offset active, apply the offset in place.
    ///    (Mostly legacy / defensive — once interception is on every
    ///    SetCursorRect is Intercept'd, but we keep the codepath for
    ///    a clean fallback if the classifier ever returns `None` for
    ///    a message that still needs the old offset treatment.)
    /// 3. **Forward verbatim** — every other message.
    pub fn pump_client_to_upstream(&mut self, id: ConnId) -> io::Result<PumpOutcome> {
        // Split-borrow so we can touch `self.events` while `conn` is
        // live. `offset` is `Option<(i32,i32)>` which is Copy so no
        // borrow-gymnastics needed.
        let Self {
            connections,
            events,
            offset,
            ..
        } = self;
        let Some(conn) = connections.get_mut(&id) else {
            return Ok(PumpOutcome::PeerClosed);
        };
        let offset = *offset;

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
            .client_feed(&buf[..n])
            .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;

        // Fast path: no messages (handshake bytes only). Forward
        // verbatim — nothing to inspect or intercept.
        if out.messages.is_empty() {
            conn.upstream_out.extend(out.forward);
            Self::try_flush(&mut conn.upstream, &mut conn.upstream_out)?;
            return Ok(PumpOutcome::Active);
        }

        // Slow path: walk every message, decide its disposition.
        let mut forwarded = Vec::with_capacity(out.forward.len());
        // Copy the pre-message prefix (auth bytes if the BEGIN landed
        // in this same chunk) verbatim.
        forwarded.extend_from_slice(&out.forward[..out.messages[0].offset]);

        // We may still need cursor rewrite for the non-intercepted
        // tail — track which messages survive so `apply_cursor_rewrites`
        // can walk them. Using a scratch `Output` keeps the existing
        // function signature.
        let mut kept = emskin_dbus::broker::state::Output::default();

        for msg in &out.messages {
            tracing::info!(
                member = msg.header.member.as_deref().unwrap_or(""),
                interface = msg.header.interface.as_deref().unwrap_or(""),
                signature = msg.header.signature.as_deref().unwrap_or(""),
                destination = msg.header.destination.as_deref().unwrap_or(""),
                body_len = msg.header.body_len,
                "client → bus message"
            );

            let msg_bytes = &out.forward[msg.offset..msg.offset + msg.length];
            let body_start_in_msg = msg.length - msg.header.body_len as usize;
            let body = &msg_bytes[body_start_in_msg..];

            if let Some(fm) = fcitx::classify(&msg.header, body) {
                // Capture the destination the client used. After the
                // client has resolved `org.fcitx.Fcitx5` via
                // GetNameOwner it'll typically send subsequent calls
                // to the resolved unique name (`:N.M` format); that's
                // what we want as the sender of our signals.
                if let Some(dest) = msg.header.destination.as_deref() {
                    if dest.starts_with(':') && conn.fcitx_server_name.as_deref() != Some(dest) {
                        tracing::debug!(
                            ?id,
                            dest,
                            "captured fcitx5 unique name for signal sender"
                        );
                        conn.fcitx_server_name = Some(dest.to_string());
                    }
                }
                // Intercept.
                let reply = fcitx::build_reply(
                    &msg.header,
                    &fm,
                    &mut conn.ic_registry,
                    &mut conn.serial_counter,
                );
                conn.client_out.extend(reply);
                Self::emit_fcitx_event(events, id, &fm, &conn.ic_registry);
                tracing::debug!(
                    ?id,
                    member = msg.header.member.as_deref().unwrap_or(""),
                    "intercepted fcitx5 method_call; reply queued"
                );
                continue;
            }

            // Not fcitx5 — keep for upstream.
            let offset_in_kept = forwarded.len();
            forwarded.extend_from_slice(msg_bytes);
            kept.messages
                .push(emskin_dbus::broker::state::ObservedMessage {
                    header: msg.header.clone(),
                    offset: offset_in_kept,
                    length: msg.length,
                });
        }

        // Legacy cursor rewrite. Once M3 lands with winit IME driving,
        // this branch should be dead (every SetCursorRect is Intercept).
        if let Some(delta) = offset {
            kept.forward = forwarded;
            apply_cursor_rewrites(&mut kept, delta);
            forwarded = kept.forward;
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
    fn emit_fcitx_event(
        events: &mut Vec<FcitxEvent>,
        conn: ConnId,
        method: &FcitxMethod,
        registry: &IcRegistry,
    ) {
        match method {
            FcitxMethod::FocusIn { ic_path } => {
                let rect = registry.get(ic_path).and_then(|s| s.cursor_rect);
                events.push(FcitxEvent::FocusChanged {
                    conn,
                    ic_path: ic_path.clone(),
                    focused: true,
                    rect,
                });
            }
            FcitxMethod::FocusOut { ic_path } => {
                events.push(FcitxEvent::FocusChanged {
                    conn,
                    ic_path: ic_path.clone(),
                    focused: false,
                    rect: None,
                });
            }
            FcitxMethod::SetCursorRect {
                ic_path, x, y, w, h,
            }
            | FcitxMethod::SetCursorRectV2 {
                ic_path, x, y, w, h, ..
            } => events.push(FcitxEvent::CursorRect {
                conn,
                ic_path: ic_path.clone(),
                rect: [*x, *y, *w, *h],
            }),
            FcitxMethod::SetCursorLocation { ic_path, x, y } => {
                events.push(FcitxEvent::CursorRect {
                    conn,
                    ic_path: ic_path.clone(),
                    rect: [*x, *y, 0, 0],
                })
            }
            FcitxMethod::DestroyIC { ic_path } => events.push(FcitxEvent::IcDestroyed {
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
        let serial = next_nonzero(&mut c.serial_counter);
        let sender = c.fcitx_server_name.as_deref().unwrap_or(SIGNAL_SENDER);
        let bytes = Signal {
            our_serial: serial,
            path: ic_path,
            interface: INPUT_CONTEXT_IFACE,
            member: "CommitString",
            destination: None,
            sender: Some(sender),
            body: body_string(text),
        }
        .encode();
        tracing::info!(?conn, ic_path, text, sender, "emit CommitString signal");
        c.client_out.extend(bytes);
        Self::try_flush(&mut c.client, &mut c.client_out)
    }

    /// Send an `org.fcitx.Fcitx.InputContext1.UpdateFormattedPreedit(a(si)i)`
    /// signal — relays `Ime::Preedit` back to the DBus client so it can
    /// render inline preedit. A `None` `cursor` omits the cursor
    /// position (encoded as `-1`).
    pub fn emit_preedit(
        &mut self,
        conn: ConnId,
        ic_path: &str,
        text: &str,
        cursor: Option<i32>,
    ) -> io::Result<()> {
        let Some(c) = self.connections.get_mut(&conn) else {
            return Ok(());
        };
        let serial = next_nonzero(&mut c.serial_counter);
        let sender = c.fcitx_server_name.as_deref().unwrap_or(SIGNAL_SENDER);
        let bytes = Signal {
            our_serial: serial,
            path: ic_path,
            interface: INPUT_CONTEXT_IFACE,
            member: "UpdateFormattedPreedit",
            destination: None,
            sender: Some(sender),
            body: body_preedit(text, cursor.unwrap_or(-1)),
        }
        .encode();
        tracing::info!(?conn, ic_path, text, sender, "emit UpdateFormattedPreedit signal");
        c.client_out.extend(bytes);
        Self::try_flush(&mut c.client, &mut c.client_out)
    }

    /// Upstream → client pump. Raw pass-through (phase 1 doesn't inspect
    /// bus → client traffic). Same buffering story as the other pump.
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
        conn.client_out.extend(&buf[..n]);
        Self::try_flush(&mut conn.client, &mut conn.client_out)?;
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

    #[test]
    fn set_offset_round_trip() {
        // Tiny: just exercise the Option<(i32, i32)> plumbing.
        let dir = tempdir().unwrap();
        let session = dir.path().join("s");
        // Fake upstream: a listener we never dial to.
        let upstream_path = dir.path().join("upstream.sock");
        let _u = UnixListener::bind(&upstream_path).unwrap();
        let mut b = DbusBroker::bind(&session, upstream_path).unwrap();
        assert_eq!(b.offset(), None);
        b.set_offset(Some((10, 20)));
        assert_eq!(b.offset(), Some((10, 20)));
        b.set_offset(None);
        assert_eq!(b.offset(), None);
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
        let reply_hdr = emskin_dbus::dbus::message::parse_header(&client_got).unwrap();
        assert_eq!(reply_hdr.reply_serial, Some(7));
        assert_eq!(reply_hdr.body_len, 0); // empty body

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
        let hdr = emskin_dbus::dbus::message::parse_header(&client_got).unwrap();
        assert_eq!(hdr.reply_serial, Some(42));
        assert_eq!(hdr.signature.as_deref(), Some("oay"));

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

        // Body: u32 array length = 0.
        let body = 0u32.to_le_bytes();

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
