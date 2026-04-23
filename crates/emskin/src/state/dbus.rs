//! DBus broker bridge — owns the in-process `DbusBroker` and injects
//! the right `DBUS_SESSION_BUS_ADDRESS` into child processes.
//!
//! Every field is optional: if the upstream session bus isn't available
//! or the broker fails to bind its listen socket, the bridge stays inert
//! and the compositor keeps running — embedded IME popups fall back to
//! hitting the host session bus directly (same as pre-PR behavior), just
//! without the fcitx5 frontend interception. No regression.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use smithay::reexports::calloop::RegistrationToken;

use crate::dbus_broker::{parse_unix_bus_address, ConnId, DbusBroker};

/// The bridge is "live" iff `broker.is_some()`. Fields left `None` when
/// inert; every caller checks before acting.
#[derive(Default)]
pub struct DbusBridge {
    /// In-process broker listening on `session_dir/bus.sock`. When
    /// present, embedded children's `DBUS_SESSION_BUS_ADDRESS` is
    /// rewritten to point here.
    pub broker: Option<DbusBroker>,
    /// Bus socket path embedded apps dial via `DBUS_SESSION_BUS_ADDRESS`.
    /// Kept as a separate field (duplicates `broker.listen_path()`) so
    /// [`Self::inject_env`] can work even if we later want to keep the
    /// bridge partially live.
    pub listen_path: Option<PathBuf>,
    /// Runtime session dir we own; cleaned up on shutdown.
    pub session_dir: Option<PathBuf>,
    /// Calloop `RegistrationToken`s for every active connection's
    /// (client → upstream, upstream → client) source pair. Owned here
    /// rather than on [`DbusBroker`] so that the broker stays
    /// calloop-agnostic (lets its unit tests run without an event
    /// loop).
    pub connection_tokens: HashMap<ConnId, (RegistrationToken, RegistrationToken)>,
}

impl std::fmt::Debug for DbusBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbusBridge")
            .field("broker", &self.broker.is_some())
            .field("listen_path", &self.listen_path)
            .field("session_dir", &self.session_dir)
            .finish()
    }
}

impl DbusBridge {
    /// Bind the in-process broker. Every failure path — missing env,
    /// unparseable bus address, failed socket bind — is logged and
    /// downgraded to a default/empty bridge.
    pub fn init() -> Self {
        let Ok(upstream_addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") else {
            tracing::info!("DBUS_SESSION_BUS_ADDRESS not set; dbus bridge inert");
            return Self::default();
        };
        let upstream_path = match parse_unix_bus_address(&upstream_addr) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    addr = %upstream_addr,
                    "unsupported DBUS_SESSION_BUS_ADDRESS; dbus bridge inert"
                );
                return Self::default();
            }
        };

        let Some(session_dir) = create_session_dir() else {
            return Self::default();
        };

        let broker = match DbusBroker::bind(&session_dir, upstream_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "dbus broker bind failed; bridge inert");
                let _ = std::fs::remove_dir_all(&session_dir);
                return Self::default();
            }
        };

        let listen_path = broker.listen_path().to_path_buf();
        tracing::info!(
            ?listen_path,
            ?session_dir,
            "dbus broker bound; bus injected into children"
        );

        Self {
            broker: Some(broker),
            listen_path: Some(listen_path),
            session_dir: Some(session_dir),
            connection_tokens: HashMap::new(),
        }
    }

    /// Inject `DBUS_SESSION_BUS_ADDRESS=unix:path=<listen_path>` into
    /// `cmd` if the broker is live. No-op if the bridge is inert — the
    /// child then inherits the parent's real upstream `DBUS_SESSION_BUS_ADDRESS`.
    pub fn inject_env(&self, cmd: &mut Command) {
        if let Some(path) = &self.listen_path {
            cmd.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", path.display()),
            );
        }
    }

    /// Drop the broker (closes all sockets) and remove the session dir.
    pub fn shutdown(&mut self) {
        // Dropping the broker closes its listen socket + every active
        // connection's client+upstream pair, so embedded apps will see
        // their DBus fds go EOF on next read.
        self.broker = None;
        self.listen_path = None;
        if let Some(dir) = self.session_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn create_session_dir() -> Option<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = runtime_dir.join(format!("emskin-dbus-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, ?dir, "failed to create dbus session dir");
        return None;
    }
    Some(dir)
}

/// Subpath helper exposed for unit tests.
#[allow(dead_code)]
pub fn format_bus_address(listen_path: &Path) -> String {
    format!("unix:path={}", listen_path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_env_is_noop_when_inert() {
        let bridge = DbusBridge::default();
        let mut cmd = Command::new("true");
        bridge.inject_env(&mut cmd);
        let dbg = format!("{cmd:?}");
        assert!(
            !dbg.contains("DBUS_SESSION_BUS_ADDRESS"),
            "inert bridge must not set env; got: {dbg}"
        );
    }

    #[test]
    fn inject_env_sets_unix_path_when_live() {
        let bridge = DbusBridge {
            listen_path: Some(PathBuf::from("/run/user/1000/emskin-dbus-42/bus.sock")),
            ..DbusBridge::default()
        };
        let mut cmd = Command::new("true");
        bridge.inject_env(&mut cmd);
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("unix:path=/run/user/1000/emskin-dbus-42/bus.sock"));
    }

    #[test]
    fn format_bus_address_wraps_unix_path() {
        assert_eq!(
            format_bus_address(Path::new("/tmp/x.sock")),
            "unix:path=/tmp/x.sock"
        );
    }
}
