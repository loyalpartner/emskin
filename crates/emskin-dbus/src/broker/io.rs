//! Unix-socket I/O driver: accept loop, per-connection threads, and the
//! byte pumps that wire [`ConnectionState`] + [`cursor`] to live sockets.
//!
//! Phase 1 model (simplest that closes emskin#55):
//!
//! - One Unix listener per proxy instance (the "bus socket" that embedded
//!   clients dial via `DBUS_SESSION_BUS_ADDRESS=unix:path=…`).
//! - Each accepted connection gets two threads: `client → bus` runs the
//!   parsed-message pipeline; `bus → client` is raw pass-through.
//! - A single process-wide [`SharedOffset`] carries the focused emskin
//!   surface's host-screen origin. Every connection reads this same offset
//!   when rewriting — we don't resolve per-client `ctx` yet.
//!
//! Later phases will:
//!
//! - Resolve `SO_PEERCRED` → pid → ctx so the offset becomes per-client.
//! - Add `SCM_RIGHTS` / unix-fd passing for DBus methods that transfer fds
//!   (portals, notifications). Messages carrying `unix_fds > 0` currently
//!   forward without the fds and those calls will fail — flagged in
//!   emskin#55 as an accepted Phase 1 limitation.

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::broker::apply_cursor_rewrites;
use crate::broker::state::ConnectionState;

const READ_BUF: usize = 8 * 1024;

/// Shared focus-origin offset, updated from the ctl-socket and read by every
/// connection thread. `None` means "no focused surface — pass cursor
/// coordinates through unchanged". `Some((x, y))` means "every recognized
/// cursor-method body gets `(x, y)` added to its leading `(cx, cy)`".
#[derive(Clone, Default, Debug)]
pub struct SharedOffset(Arc<Mutex<Option<(i32, i32)>>>);

impl SharedOffset {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, value: Option<(i32, i32)>) {
        if let Ok(mut g) = self.0.lock() {
            *g = value;
        }
    }

    pub fn get(&self) -> Option<(i32, i32)> {
        self.0.lock().ok().and_then(|g| *g)
    }
}

/// Accept clients on `listen_path`, dial `bus_path` for each, and spawn a
/// broker thread per pair.
///
/// The listener is bound by [`BrokerServer::bind`] and run synchronously by
/// [`BrokerServer::run`]. Callers own the process's main thread while
/// `run` is executing — typical usage is in `main()` after spawning other
/// long-lived threads (e.g. the ctl-socket listener).
pub struct BrokerServer {
    listener: UnixListener,
    bus_path: PathBuf,
    offset: SharedOffset,
}

impl BrokerServer {
    pub fn bind(
        listen_path: impl AsRef<Path>,
        bus_path: impl Into<PathBuf>,
        offset: SharedOffset,
    ) -> std::io::Result<Self> {
        let listener = UnixListener::bind(listen_path.as_ref())?;
        Ok(Self {
            listener,
            bus_path: bus_path.into(),
            offset,
        })
    }

    /// Local path the listener is bound to — useful in tests and for
    /// injecting into `DBUS_SESSION_BUS_ADDRESS`.
    pub fn listen_path(&self) -> std::io::Result<PathBuf> {
        Ok(self
            .listener
            .local_addr()?
            .as_pathname()
            .ok_or_else(|| std::io::Error::other("listener has no path"))?
            .to_path_buf())
    }

    pub fn run(&self) -> std::io::Result<()> {
        for conn in self.listener.incoming() {
            let client = match conn {
                Ok(s) => s,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let bus = match UnixStream::connect(&self.bus_path) {
                Ok(s) => s,
                Err(e) => {
                    // Can't reach the host bus — drop the client and keep
                    // accepting; the user can investigate without tearing
                    // down the proxy.
                    tracing::warn!(error = %e, bus_path = ?self.bus_path, "upstream dial failed");
                    continue;
                }
            };
            let offset = self.offset.clone();
            thread::Builder::new()
                .name("emskin-dbus-conn".into())
                .spawn(move || {
                    if let Err(e) = run_pair(client, bus, offset) {
                        tracing::warn!(error = %e, "broker pair terminated with error");
                    }
                })?;
        }
        Ok(())
    }
}

/// Drive one client↔bus connection pair to completion. Returns when either
/// side closes or a fatal error is encountered. Safe to call from any
/// thread — the caller typically spawns this.
pub fn run_pair(client: UnixStream, bus: UnixStream, offset: SharedOffset) -> std::io::Result<()> {
    // Separate handles for shutdown signaling — when one half exits we
    // shut both sides of both sockets so the partner thread unblocks from
    // its read and exits too.
    let client_shut = client.try_clone()?;
    let bus_shut = bus.try_clone()?;

    let c2b = {
        let client_r = client.try_clone()?;
        let bus_w = bus.try_clone()?;
        let offset = offset.clone();
        thread::Builder::new()
            .name("emskin-dbus-c2b".into())
            .spawn(move || drive_client_to_bus(client_r, bus_w, offset))?
    };

    let b2c = thread::Builder::new()
        .name("emskin-dbus-b2c".into())
        .spawn(move || drive_bus_to_client(bus, client))?;

    // Wait for c2b first; then tear down sockets to unblock b2c.
    let c2b_res = c2b.join();
    let _ = client_shut.shutdown(Shutdown::Both);
    let _ = bus_shut.shutdown(Shutdown::Both);
    let b2c_res = b2c.join();

    flatten_thread_result(c2b_res)?;
    flatten_thread_result(b2c_res)?;
    Ok(())
}

fn flatten_thread_result(res: thread::Result<std::io::Result<()>>) -> std::io::Result<()> {
    match res {
        Ok(inner) => inner,
        Err(_) => Err(std::io::Error::other("broker thread panicked")),
    }
}

fn drive_client_to_bus(
    mut client: UnixStream,
    mut bus: UnixStream,
    offset: SharedOffset,
) -> std::io::Result<()> {
    let mut state = ConnectionState::new();
    let mut buf = [0u8; READ_BUF];
    loop {
        let n = match client.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };

        let mut out = state
            .client_feed(&buf[..n])
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;

        for msg in &out.messages {
            tracing::info!(
                member = msg.header.member.as_deref().unwrap_or(""),
                interface = msg.header.interface.as_deref().unwrap_or(""),
                signature = msg.header.signature.as_deref().unwrap_or(""),
                body_len = msg.header.body_len,
                "client → bus message"
            );
        }

        if let Some(delta) = offset.get() {
            apply_cursor_rewrites(&mut out, delta);
        }

        bus.write_all(&out.forward)?;
    }
}

fn drive_bus_to_client(mut bus: UnixStream, mut client: UnixStream) -> std::io::Result<()> {
    let mut buf = [0u8; READ_BUF];
    loop {
        let n = match bus.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        client.write_all(&buf[..n])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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

    #[test]
    fn shared_offset_round_trips() {
        let off = SharedOffset::new();
        assert_eq!(off.get(), None);
        off.set(Some((10, 20)));
        assert_eq!(off.get(), Some((10, 20)));
        off.set(None);
        assert_eq!(off.get(), None);
    }

    /// End-to-end: a socketpair stands in for the embedded client, a second
    /// socketpair stands in for the upstream bus. The broker rewrites
    /// SetCursorRect body bytes in flight.
    #[test]
    fn run_pair_rewrites_set_cursor_rect_live() {
        // Simulated upstream bus: we use a socketpair and pretend one end
        // is dbus-daemon.
        let (bus_proxy_side, mut bus_upstream_side) = UnixStream::pair().unwrap();
        // Simulated embedded client.
        let (mut client_app_side, client_proxy_side) = UnixStream::pair().unwrap();

        let offset = SharedOffset::new();
        offset.set(Some((50, 60)));

        let offset_clone = offset.clone();
        let broker = thread::spawn(move || {
            run_pair(client_proxy_side, bus_proxy_side, offset_clone).unwrap();
        });

        // Collector thread for the upstream side.
        let collector = thread::spawn(move || {
            let mut collected = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match bus_upstream_side.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => collected.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            collected
        });

        // Write handshake + SetCursorRect.
        client_app_side
            .write_all(b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n")
            .unwrap();
        let call = build_set_cursor_rect(7, (100, 200, 10, 20));
        client_app_side.write_all(&call).unwrap();
        // Small grace period so the broker flushes before we EOF.
        thread::sleep(Duration::from_millis(50));
        drop(client_app_side);

        broker.join().unwrap();
        let bus_bytes = collector.join().unwrap();

        let hpre = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        assert!(bus_bytes.starts_with(hpre));
        let msg_bytes = &bus_bytes[hpre.len()..];

        let hdr = crate::dbus::message::parse_header(msg_bytes).unwrap();
        assert_eq!(hdr.member.as_deref(), Some("SetCursorRect"));
        let body_start = msg_bytes.len() - hdr.body_len as usize;
        let body = &msg_bytes[body_start..];
        assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), 150);
        assert_eq!(i32::from_le_bytes(body[4..8].try_into().unwrap()), 260);
        // w and h unchanged.
        assert_eq!(i32::from_le_bytes(body[8..12].try_into().unwrap()), 10);
        assert_eq!(i32::from_le_bytes(body[12..16].try_into().unwrap()), 20);
    }

    /// When the offset is `None`, the broker should pass bytes through
    /// verbatim.
    #[test]
    fn run_pair_with_none_offset_is_pass_through() {
        let (bus_proxy_side, mut bus_upstream_side) = UnixStream::pair().unwrap();
        let (mut client_app_side, client_proxy_side) = UnixStream::pair().unwrap();

        let offset = SharedOffset::new(); // no offset
        let broker = thread::spawn(move || {
            run_pair(client_proxy_side, bus_proxy_side, offset).unwrap();
        });
        let collector = thread::spawn(move || {
            let mut collected = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = bus_upstream_side.read(&mut buf) {
                if n == 0 {
                    break;
                }
                collected.extend_from_slice(&buf[..n]);
            }
            collected
        });

        let handshake = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n".to_vec();
        client_app_side.write_all(&handshake).unwrap();
        let call = build_set_cursor_rect(11, (42, 84, 5, 5));
        client_app_side.write_all(&call).unwrap();
        thread::sleep(Duration::from_millis(50));
        drop(client_app_side);

        broker.join().unwrap();
        let bus_bytes = collector.join().unwrap();
        let mut expected = handshake;
        expected.extend_from_slice(&call);
        assert_eq!(bus_bytes, expected);
    }

    /// When the client sends bytes in the reverse direction (bus → client),
    /// they should pass through verbatim without parsing.
    #[test]
    fn bus_to_client_is_verbatim() {
        let (bus_proxy_side, mut bus_upstream_side) = UnixStream::pair().unwrap();
        let (mut client_app_side, client_proxy_side) = UnixStream::pair().unwrap();
        let offset = SharedOffset::new();

        let broker = thread::spawn(move || {
            run_pair(client_proxy_side, bus_proxy_side, offset).unwrap();
        });

        // Pretend the bus sends some auth replies to the client.
        let bus_blob = b"OK 0123456789abcdef0123456789abcdef\r\n";
        bus_upstream_side.write_all(bus_blob).unwrap();

        // Read from client side.
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        let n = client_app_side.read(&mut buf).unwrap();
        received.extend_from_slice(&buf[..n]);
        assert_eq!(received, bus_blob);

        drop(bus_upstream_side);
        drop(client_app_side);
        broker.join().unwrap();
    }
}
