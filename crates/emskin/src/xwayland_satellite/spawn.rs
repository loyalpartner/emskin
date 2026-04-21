//! xwayland-satellite binary probing + `Command` construction.
//!
//! Ported from niri `src/utils/xwayland/satellite.rs` (GPL-3.0-or-later) —
//! see upstream `test_ondemand` / `spawn_and_wait` for the originals.

use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::sockets::X11Sockets;

#[derive(Debug, Clone)]
pub struct SpawnConfig {
    pub binary: PathBuf,
    /// Forwarded as `WAYLAND_DISPLAY`.
    pub wayland_socket: PathBuf,
    /// Forwarded as `XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: PathBuf,
}

/// Probe whether `binary` supports on-demand listenfd activation by
/// running `binary :0 --test-listenfd-support`. Returns `true` only on a
/// clean zero-exit; any spawn / wait / non-zero-exit yields `false`.
///
/// Ports niri `test_ondemand` (niri satellite.rs:79).
pub fn test_ondemand(binary: &Path) -> bool {
    let mut cmd = Command::new(binary);
    cmd.args([":0", "--test-listenfd-support"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE");

    let Ok(mut child) = cmd.spawn() else {
        return false;
    };
    matches!(child.wait(), Ok(status) if status.success())
}

/// Construct the `Command` that execs the satellite with the pre-bound
/// listenfds handed over via `-listenfd` argv + pre_exec CLOEXEC-clearing.
///
/// The returned `Command` is NOT spawned. Callers are responsible for
/// keeping `sockets` alive until `spawn()` completes — the raw fds are
/// referenced by argv and the pre_exec hook.
///
/// Ports niri `spawn_and_wait` argv/env/pre_exec setup (niri
/// satellite.rs:213-297).
pub fn build_spawn_command(config: &SpawnConfig, sockets: &X11Sockets) -> Command {
    build_spawn_command_raw(
        config,
        &sockets.display_name,
        sockets.unix_fd.as_raw_fd(),
        sockets.abstract_fd.as_ref().map(|fd| fd.as_raw_fd()),
    )
}

/// Lower-level variant that takes raw fds. Used by the spawner thread
/// (which holds cloned `OwnedFd`s independently of the parent's
/// `X11Sockets`, because that struct's RAII guards would unlink on drop
/// across thread boundaries).
pub fn build_spawn_command_raw(
    config: &SpawnConfig,
    display_name: &str,
    unix_fd_raw: RawFd,
    abstract_fd_raw: Option<RawFd>,
) -> Command {
    let mut cmd = Command::new(&config.binary);

    // First positional arg is always `:N`. After that come the `-listenfd`
    // pairs for the unix socket and (on Linux) the abstract socket.
    cmd.arg(display_name);
    cmd.arg("-listenfd").arg(unix_fd_raw.to_string());
    if let Some(r) = abstract_fd_raw {
        cmd.arg("-listenfd").arg(r.to_string());
    }

    cmd.env("WAYLAND_DISPLAY", &config.wayland_socket)
        .env("XDG_RUNTIME_DIR", &config.xdg_runtime_dir)
        // Strip anything that could confuse the satellite or its children.
        .env_remove("DISPLAY")
        .env_remove("RUST_BACKTRACE")
        .env_remove("RUST_LIB_BACKTRACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // pre_exec runs in the child after fork/before exec. Clear CLOEXEC on
    // the listenfd(s) so they survive into the satellite.
    unsafe {
        cmd.pre_exec(move || {
            // SAFETY: caller guarantees the raw fds stay open until after
            // Command::spawn() returns (parent keeps the OwnedFd). The fork
            // duplicates the fd table, so between fork and exec the fd is
            // open in the child as well.
            let unix_fd = BorrowedFd::borrow_raw(unix_fd_raw);
            clear_cloexec(unix_fd)?;
            if let Some(r) = abstract_fd_raw {
                let ab_fd = BorrowedFd::borrow_raw(r);
                clear_cloexec(ab_fd)?;
            }
            Ok(())
        });
    }

    cmd
}

fn clear_cloexec(fd: BorrowedFd<'_>) -> std::io::Result<()> {
    // Read current flags, mask out FD_CLOEXEC, write back. We avoid pulling
    // in `nix` / extra deps — raw libc is fine and matches the rest of
    // emskin (see main.rs's direct libc usage).
    let raw = fd.as_raw_fd();
    // SAFETY: fcntl on an open fd is safe; raw fd is provided by caller.
    let cur = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if cur < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let new = cur & !libc::FD_CLOEXEC;
    // SAFETY: same fd; new flag bits valid.
    let rc = unsafe { libc::fcntl(raw, libc::F_SETFD, new) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
