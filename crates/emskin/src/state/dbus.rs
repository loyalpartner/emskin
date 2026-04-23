//! DBus-proxy bridge substruct.
//!
//! Tracks the spawned `emskin-dbus-proxy` child, the path of the bus socket
//! children should dial (injected into `DBUS_SESSION_BUS_ADDRESS`), and the
//! ctl-socket client used to push focus rectangles. Every field is
//! optional: if the proxy binary is missing or fails to start, the bridge
//! stays inert and the compositor keeps running — IME cursor popups in
//! embedded apps will appear at the wrong place (same as today, minus the
//! rewrite) but nothing else regresses.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use emskin_dbus::ctl::client::CtlClient;
use emskin_dbus::protocol::{EmskinToProxy, Rect};

/// Fields left Default when the bridge is inert — every caller checks
/// `listen_path.is_some()` before acting.
#[derive(Debug, Default)]
pub struct DbusBridge {
    /// Child handle for the spawned `emskin-dbus-proxy`. Reaped on shutdown.
    pub proxy_child: Option<Child>,
    /// Bus socket path embedded apps dial via `DBUS_SESSION_BUS_ADDRESS`.
    pub listen_path: Option<PathBuf>,
    /// Runtime session dir we own; cleaned up on shutdown.
    pub session_dir: Option<PathBuf>,
    /// Open ctl client — used by the focus reconciler to push rects.
    pub ctl: Option<CtlClient>,
    /// Last rect we sent, for diffing. `None` means "we last sent
    /// `FocusCleared` or haven't sent anything yet".
    last_rect: Option<Rect>,
}

impl DbusBridge {
    /// Spawn the proxy binary, connect to its ctl socket, and return a
    /// fully populated [`DbusBridge`]. Every failure path — missing env,
    /// missing binary, spawn error, ctl timeout — is logged and downgraded
    /// to a default/empty bridge rather than propagated; the compositor
    /// must keep running without IME coord rewriting in that case.
    pub fn spawn_and_connect() -> Self {
        let Ok(upstream_bus) = std::env::var("DBUS_SESSION_BUS_ADDRESS") else {
            tracing::info!("DBUS_SESSION_BUS_ADDRESS not set; skipping emskin-dbus-proxy");
            return Self::default();
        };

        let Some(binary) = locate_proxy_binary() else {
            tracing::warn!(
                "emskin-dbus-proxy not found next to emskin or on PATH; \
                 IME cursor rewriting disabled"
            );
            return Self::default();
        };

        let Some(session_dir) = create_session_dir() else {
            return Self::default();
        };
        let listen_path = session_dir.join("bus.sock");
        let ctl_path = session_dir.join("ctl.sock");

        // Detach stdio: file-less so the proxy never keeps an inherited
        // pipe open past the parent's lifetime. Tracing output instead goes
        // through `EMSKIN_DBUS_LOG_DIR` below — the proxy writes to a log
        // file there via `tracing_subscriber::with_writer`, which is a plain
        // fs file fd (doesn't block `cmd | tail`-style wrappers).
        let log_dir = emskin_log_dir();
        let mut cmd = Command::new(&binary);
        cmd.env("EMSKIN_DBUS_PROXY_LISTEN", &listen_path)
            .env("EMSKIN_DBUS_PROXY_CTL", &ctl_path)
            .env("DBUS_SESSION_BUS_ADDRESS", &upstream_bus)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = &log_dir {
            cmd.env("EMSKIN_DBUS_LOG_DIR", dir);
        }
        // Linux-only safety net: when the parent dies (SIGKILL from a test
        // harness, oom-kill, etc.) the kernel delivers SIGTERM to us so the
        // proxy self-reaps instead of being orphaned to PID 1.
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                // SAFETY: prctl is a safe syscall with the given arguments;
                // the closure is called in the child between fork and exec.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "failed to spawn emskin-dbus-proxy");
                let _ = std::fs::remove_dir_all(&session_dir);
                return Self::default();
            }
        };

        let mut ctl = match CtlClient::connect(&ctl_path, Duration::from_secs(3)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "ctl connect to emskin-dbus-proxy failed");
                return Self {
                    proxy_child: Some(child),
                    listen_path: None,
                    session_dir: Some(session_dir),
                    ..Self::default()
                };
            }
        };

        if let Err(e) = ctl.wait_ready() {
            tracing::warn!(error = %e, "emskin-dbus-proxy never sent Ready");
            return Self {
                proxy_child: Some(child),
                listen_path: None,
                session_dir: Some(session_dir),
                ..Self::default()
            };
        }

        tracing::info!(
            ?listen_path,
            ?ctl_path,
            ?log_dir,
            upstream = %upstream_bus,
            "emskin-dbus-proxy spawned; bus injected into children"
        );

        Self {
            proxy_child: Some(child),
            listen_path: Some(listen_path),
            session_dir: Some(session_dir),
            ctl: Some(ctl),
            last_rect: None,
        }
    }

    /// Inject `DBUS_SESSION_BUS_ADDRESS=unix:path=<proxy listen path>` into
    /// `cmd` if the proxy is live. No-op if the bridge is inert — the
    /// child then inherits the parent's real upstream `DBUS_SESSION_BUS_ADDRESS`.
    pub fn inject_env(&self, cmd: &mut Command) {
        if let Some(path) = &self.listen_path {
            cmd.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", path.display()),
            );
        }
    }

    /// Push a focus rect, but only if it changed since the last call.
    /// Rects use emskin-local (space) coordinates; the proxy adds
    /// `(rect[0], rect[1])` to each `SetCursorRect` / `SetCursorLocation`.
    pub fn push_rect(&mut self, rect: Rect) {
        if self.last_rect == Some(rect) {
            return;
        }
        let Some(ctl) = self.ctl.as_mut() else { return };
        if let Err(e) = ctl.send(&EmskinToProxy::FocusChanged { ctx: 0, rect }) {
            tracing::warn!(error = %e, "ctl send FocusChanged failed");
            // On write failure we deliberately *don't* drop `ctl` — the
            // next focus change retries. Proxy crashes surface via the
            // reaped child in the tick loop.
            return;
        }
        self.last_rect = Some(rect);
    }

    /// Push `FocusCleared`, only if we had previously sent a rect.
    pub fn push_cleared(&mut self) {
        if self.last_rect.is_none() {
            return;
        }
        let Some(ctl) = self.ctl.as_mut() else { return };
        if let Err(e) = ctl.send(&EmskinToProxy::FocusCleared) {
            tracing::warn!(error = %e, "ctl send FocusCleared failed");
            return;
        }
        self.last_rect = None;
    }

    /// Reap the proxy child and remove the session dir.
    pub fn shutdown(&mut self) {
        if let Some(mut ctl) = self.ctl.take() {
            let _ = ctl.send(&EmskinToProxy::Shutdown);
        }
        if let Some(mut child) = self.proxy_child.take() {
            // Small grace for the ctl Shutdown to take effect, then kill.
            let _ = child.kill();
            let _ = child.wait();
        }
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
        tracing::warn!(error = %e, ?dir, "failed to create emskin-dbus session dir");
        return None;
    }
    Some(dir)
}

/// Log dir shared with the rest of emskin — `$XDG_RUNTIME_DIR/emskin-<pid>/logs`.
/// Matches the convention already used by `extract_embedded` for elisp/demo.
/// Returns `None` if the dir cannot be created; the caller should then skip
/// the `EMSKIN_DBUS_LOG_DIR` env var so the proxy logs to its stderr (which
/// is `Stdio::null`).
fn emskin_log_dir() -> Option<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let dir = runtime_dir
        .join(format!("emskin-{}", std::process::id()))
        .join("logs");
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(error = %e, ?dir, "failed to create emskin log dir");
            None
        }
    }
}

/// Locate `emskin-dbus-proxy`: sibling of the current binary first (matches
/// the AUR layout and the dev target dir), then `PATH` fallback.
fn locate_proxy_binary() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("emskin-dbus-proxy");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path_env = std::env::var_os("PATH")?;
    for p in std::env::split_paths(&path_env) {
        let candidate = p.join("emskin-dbus-proxy");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Subpath helper used by unit tests + the focus reconciler.
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
        // Converting to Debug is the simplest way to inspect env additions
        // without pulling in a richer dependency.
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
    fn push_rect_without_ctl_is_noop() {
        let mut bridge = DbusBridge::default();
        bridge.push_rect([10, 20, 0, 0]);
        assert_eq!(bridge.last_rect, None);
    }

    #[test]
    fn format_bus_address_wraps_unix_path() {
        assert_eq!(
            format_bus_address(Path::new("/tmp/x.sock")),
            "unix:path=/tmp/x.sock"
        );
    }
}
