//! DBus broker bridge — owns the in-process `DbusBroker` and injects
//! the right `DBUS_SESSION_BUS_ADDRESS` into child processes.
//!
//! Every field is optional: if the upstream session bus isn't available
//! or the broker fails to bind its listen socket, the bridge stays inert
//! and the compositor keeps running — embedded IME popups fall back to
//! hitting the host session bus directly (same as pre-PR behavior), just
//! without the fcitx5 frontend interception. No regression.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::RegistrationToken;

use emskin_dbus::{parse_unix_bus_address, ConnId, DbusBroker};

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
    /// Private `dbus-daemon` child when `--dbus-isolated` is in effect.
    /// `None` for the default mode (broker forwards to the host session
    /// bus). Owned here so [`Self::shutdown`] can SIGTERM it before the
    /// session dir is removed.
    pub isolated_daemon: Option<Child>,
}

impl std::fmt::Debug for DbusBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbusBridge")
            .field("broker", &self.broker.is_some())
            .field("listen_path", &self.listen_path)
            .field("session_dir", &self.session_dir)
            .field("isolated_daemon", &self.isolated_daemon.is_some())
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
            isolated_daemon: None,
        }
    }

    /// `--dbus-isolated`: spawn a private `dbus-daemon` per emskin
    /// instance and route the broker's upstream to that daemon instead
    /// of the host session bus. Embedded apps see an isolated session
    /// where `.service` activations spawn under emskin's environment
    /// (so portal-style fork-and-exec keeps windows inside emskin) and
    /// `org.gtk.Application.<id>` lives in a private namespace (so a
    /// host instance of the same app no longer absorbs activation).
    ///
    /// Failure paths (no `dbus-daemon` binary, config write fails,
    /// daemon exits early, broker bind fails) all downgrade to an
    /// inert bridge — embedded children fall back to the parent's
    /// upstream `DBUS_SESSION_BUS_ADDRESS` and the compositor keeps
    /// running.
    pub fn init_isolated() -> Self {
        let Some(session_dir) = create_session_dir() else {
            return Self::default();
        };

        let daemon_socket = session_dir.join("upstream-bus.sock");
        let services_dir = session_dir.join("services");
        let config_path = session_dir.join("session.conf");
        if let Err(e) =
            write_minimal_session_config(&config_path, &services_dir, &daemon_socket)
        {
            tracing::warn!(error = %e, "failed to write session.conf; bridge inert");
            let _ = std::fs::remove_dir_all(&session_dir);
            return Self::default();
        }

        let mut daemon = match spawn_isolated_daemon(&config_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "failed to spawn dbus-daemon; bridge inert");
                let _ = std::fs::remove_dir_all(&session_dir);
                return Self::default();
            }
        };

        if let Err(e) = wait_for_daemon_socket(&daemon_socket, &mut daemon) {
            tracing::warn!(error = %e, "dbus-daemon never came up; bridge inert");
            let _ = daemon.kill();
            let _ = daemon.wait();
            let _ = std::fs::remove_dir_all(&session_dir);
            return Self::default();
        }

        let broker = match DbusBroker::bind(&session_dir, daemon_socket.clone()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "broker bind under isolated daemon failed");
                let _ = daemon.kill();
                let _ = daemon.wait();
                let _ = std::fs::remove_dir_all(&session_dir);
                return Self::default();
            }
        };

        let listen_path = broker.listen_path().to_path_buf();
        tracing::info!(
            ?listen_path,
            ?daemon_socket,
            ?session_dir,
            daemon_pid = daemon.id(),
            "dbus broker bound with isolated dbus-daemon upstream"
        );

        Self {
            broker: Some(broker),
            listen_path: Some(listen_path),
            session_dir: Some(session_dir),
            connection_tokens: HashMap::new(),
            isolated_daemon: Some(daemon),
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
    /// Tears down in order: broker (so children's connections EOF
    /// first), then the isolated daemon (no live clients depending on
    /// it), then the session dir.
    pub fn shutdown(&mut self) {
        self.broker = None;
        self.listen_path = None;
        if let Some(mut daemon) = self.isolated_daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        if let Some(dir) = self.session_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Spawn `dbus-daemon` with our minimal session.conf. We deliberately
/// do **not** use `--session` because that pulls in the system-wide
/// session.conf, which references `<standard_session_servicedirs/>`
/// and exposes every host `.service` file — including ones with
/// `SystemdService=` directives that ask the host's systemd-user to
/// activate, which can't deliver into our isolated bus and produce
/// 25s method-call timeouts. Our config has only an empty servicedir
/// we control; uninhabited names fail fast with `NameHasNoOwner`.
///
/// Stays in foreground (`--nofork`) so it's a managed child of
/// emskin; `PR_SET_PDEATHSIG(SIGTERM)` in `pre_exec` ensures the
/// daemon dies if emskin is killed without running its shutdown path.
///
/// stdout/stderr go to `/dev/null`; readiness is detected by polling
/// for the socket file rather than parsing daemon output (avoids the
/// pipe-blocking trap where dbus-daemon's later log lines would stall
/// on a full pipe).
fn spawn_isolated_daemon(config_path: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new("dbus-daemon");
    cmd.arg("--nofork")
        .arg(format!("--config-file={}", config_path.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: pre_exec runs in the child between fork and exec. We
    // only call `prctl`, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Block until `socket` exists or the daemon exits. dbus-daemon
/// creates its listen socket synchronously before going into its
/// service loop, so a short poll-and-stat is sufficient and avoids
/// shimming an inotify watch.
fn wait_for_daemon_socket(socket: &Path, daemon: &mut Child) -> std::io::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(3);
    const POLL_INTERVAL: Duration = Duration::from_millis(25);
    let start = Instant::now();
    loop {
        if socket.exists() {
            return Ok(());
        }
        if let Some(status) = daemon.try_wait()? {
            return Err(std::io::Error::other(format!(
                "dbus-daemon exited before binding socket: {status}"
            )));
        }
        if start.elapsed() > TIMEOUT {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "dbus-daemon listen socket did not appear within 3s",
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}


/// Write a minimal session.conf for the isolated daemon. Critical
/// design point: we **do not** include `<standard_session_servicedirs/>`
/// (which would expose every host `.service` file, including ones with
/// `SystemdService=` entries that ask host-systemd to activate — the
/// activation lands on the host bus, never our isolated bus, and the
/// caller waits 25s for the default DBus reply timeout). Our private
/// servicedir starts empty; uninhabited names fail fast with
/// `NameHasNoOwner` and apps fall back gracefully.
///
/// Policy is intentionally permissive (`allow own="*"`, `allow
/// send_destination="*"`) — this is a single-user isolated bus, not a
/// security boundary. The boundary is process containment, not DBus
/// policy.
fn write_minimal_session_config(
    path: &Path,
    services_dir: &Path,
    socket: &Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(services_dir)?;
    let xml = format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={socket}</listen>
  <auth>EXTERNAL</auth>
  <servicedir>{services}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
  <limit name="reply_timeout">300000</limit>
</busconfig>
"#,
        socket = socket.display(),
        services = services_dir.display(),
    );
    std::fs::write(path, xml)
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
