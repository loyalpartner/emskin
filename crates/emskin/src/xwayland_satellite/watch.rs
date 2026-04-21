//! Event-driven xwayland-satellite supervisor.
//!
//! State machine:
//! ```text
//!            arm()                                on_socket_connect()
//! Disarmed ─────────── Watching { tokens } ──────────────────────── Running
//!    ▲                         ▲                                         │
//!    │      disarm()           │ on_rearm() ← ToMain::Rearm (via channel)│
//!    └─────────────────────────┴─────────────────────────────────────────┘
//! ```
//!
//! `arm()` inserts calloop `Generic` sources for the unix + (on Linux)
//! abstract fds of [`X11Sockets`]. When an X11 client connects, the
//! callback invokes `on_socket_connect`, which removes both sources,
//! spawns the satellite in a dedicated thread, and transitions to
//! `Running`. The thread waits for the child to exit, then sends
//! `ToMain::Rearm` through a [`calloop::channel`] — the main loop's
//! handler invokes `on_rearm()`, which drains any pending connections
//! (anti-busyloop) and re-installs the `Generic` sources.
//!
//! Ported from niri `src/utils/xwayland/satellite.rs` (GPL-3.0-or-later).

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::mpsc;
use std::thread;

use smithay::reexports::calloop::{
    channel::Sender, generic::Generic, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};

use super::sockets::{clear_out_pending_connections, X11Sockets};
use super::spawn::{build_spawn_command_raw, SpawnConfig};

/// Access trait: lets [`XwlsIntegration`] be generic over the user's
/// calloop state type while still reaching back into itself from inside
/// source callbacks.
pub trait HasXwls {
    fn xwls_mut(&mut self) -> Option<&mut XwlsIntegration>;
}

/// Message sent from the spawner thread back to the main loop.
#[derive(Debug, Clone, Copy)]
pub enum ToMain {
    /// Child exited (for any reason) — please re-arm the socket watch.
    Rearm,
}

#[derive(Debug)]
enum WatchState {
    Disarmed,
    Watching {
        unix_token: RegistrationToken,
        abstract_token: Option<RegistrationToken>,
    },
    Running {
        /// Detached at drop time; the child's lifecycle is managed by the
        /// spawner thread itself.
        #[allow(dead_code)]
        spawner: thread::JoinHandle<()>,
    },
}

pub struct XwlsIntegration {
    sockets: X11Sockets,
    spawn_cfg: SpawnConfig,
    state: WatchState,
    to_main: Sender<ToMain>,
    spawner_done: Option<mpsc::Sender<()>>,
}

impl std::fmt::Debug for XwlsIntegration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XwlsIntegration")
            .field("display_name", &self.sockets.display_name)
            .field("state", &self.state)
            .finish()
    }
}

impl XwlsIntegration {
    pub fn new(sockets: X11Sockets, spawn_cfg: SpawnConfig, to_main: Sender<ToMain>) -> Self {
        Self {
            sockets,
            spawn_cfg,
            state: WatchState::Disarmed,
            to_main,
            spawner_done: None,
        }
    }

    pub fn display_name(&self) -> &str {
        &self.sockets.display_name
    }

    pub fn is_disarmed(&self) -> bool {
        matches!(self.state, WatchState::Disarmed)
    }

    pub fn is_watching(&self) -> bool {
        matches!(self.state, WatchState::Watching { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, WatchState::Running { .. })
    }

    pub fn set_spawner_done_hook(&mut self, tx: mpsc::Sender<()>) {
        self.spawner_done = Some(tx);
    }

    pub fn arm<D: HasXwls + 'static>(
        &mut self,
        handle: &LoopHandle<'static, D>,
    ) -> std::io::Result<()> {
        if !matches!(self.state, WatchState::Disarmed) {
            return Ok(()); // idempotent
        }

        // We register clones of the owned fds so the parent-owned fds stay
        // alive independent of source removal / satellite spawn-handoff.
        let unix_clone = self.sockets.unix_fd.try_clone()?;
        let abstract_clone = self
            .sockets
            .abstract_fd
            .as_ref()
            .map(|fd| fd.try_clone())
            .transpose()?;

        let handle_for_unix = handle.clone();
        let unix_token = handle
            .insert_source(
                Generic::new(unix_clone, Interest::READ, Mode::Level),
                move |_, _, state| {
                    if let Some(x) = state.xwls_mut() {
                        if let Err(e) = x.on_socket_connect(&handle_for_unix) {
                            tracing::warn!("xwls on_socket_connect failed: {e}");
                        }
                    }
                    // Matches niri: remove self on first fire. The sibling
                    // abstract source is removed explicitly inside
                    // on_socket_connect via handle.remove(token), so both
                    // get cleaned up regardless of which fires first.
                    Ok(PostAction::Remove)
                },
            )
            .map_err(|e| std::io::Error::other(format!("calloop insert unix source: {e}")))?;

        let abstract_token = if let Some(fd) = abstract_clone {
            let handle_for_abs = handle.clone();
            let tok = handle
                .insert_source(
                    Generic::new(fd, Interest::READ, Mode::Level),
                    move |_, _, state| {
                        if let Some(x) = state.xwls_mut() {
                            if let Err(e) = x.on_socket_connect(&handle_for_abs) {
                                tracing::warn!("xwls on_socket_connect failed: {e}");
                            }
                        }
                        Ok(PostAction::Remove)
                    },
                )
                .map_err(|e| {
                    std::io::Error::other(format!("calloop insert abstract source: {e}"))
                })?;
            Some(tok)
        } else {
            None
        };

        self.state = WatchState::Watching {
            unix_token,
            abstract_token,
        };
        Ok(())
    }

    pub fn disarm<D: HasXwls + 'static>(&mut self, handle: &LoopHandle<'static, D>) {
        if let WatchState::Watching {
            unix_token,
            abstract_token,
        } = std::mem::replace(&mut self.state, WatchState::Disarmed)
        {
            handle.remove(unix_token);
            if let Some(t) = abstract_token {
                handle.remove(t);
            }
        }
    }

    pub fn on_socket_connect<D: HasXwls + 'static>(
        &mut self,
        handle: &LoopHandle<'static, D>,
    ) -> std::io::Result<()> {
        tracing::info!(
            display = %self.sockets.display_name,
            "xwayland-satellite: X client connected, spawning satellite"
        );

        // Remove both watch sources — satellite takes over the fds once
        // it's spawned. Does nothing if we're already Running (callbacks
        // could in theory race; idempotent guard here).
        match std::mem::replace(&mut self.state, WatchState::Disarmed) {
            WatchState::Watching {
                unix_token,
                abstract_token,
            } => {
                handle.remove(unix_token);
                if let Some(t) = abstract_token {
                    handle.remove(t);
                }
            }
            other @ (WatchState::Running { .. } | WatchState::Disarmed) => {
                // Not watching → nothing to do. Restore state.
                self.state = other;
                return Ok(());
            }
        }

        // Spawn the satellite in a dedicated thread. The thread owns
        // cloned fds; the parent's X11Sockets stays intact so rearming
        // later can re-register sources.
        if let Err(e) = self.spawn_satellite_thread() {
            // Spawner setup failed after sources were removed. Loud error
            // and attempt to restore the Watching state so the next X
            // client connect gets another chance. If re-arm itself fails
            // (calloop source insert failed — truly terminal), state
            // stays Disarmed and XWayland integration is effectively
            // disabled for this run.
            tracing::error!(
                display = %self.sockets.display_name,
                "xwayland-satellite: spawner setup failed: {e}; attempting re-arm"
            );
            if let Err(rearm_err) = self.arm(handle) {
                tracing::error!(
                    display = %self.sockets.display_name,
                    "xwayland-satellite: re-arm after spawner failure also failed: \
                     {rearm_err} — XWayland integration disabled until emskin restart"
                );
            }
            return Err(e);
        }
        Ok(())
    }

    /// Helper: clone fds, build SpawnConfig hands-off, and start the
    /// spawner thread. On success transitions self into `Running`.
    /// Factored out of `on_socket_connect` so failures can be handled
    /// without deeply nested `?` across the source-removal boundary.
    fn spawn_satellite_thread(&mut self) -> std::io::Result<()> {
        let unix_clone = self.sockets.unix_fd.try_clone()?;
        let abstract_clone = self
            .sockets
            .abstract_fd
            .as_ref()
            .map(|fd| fd.try_clone())
            .transpose()?;
        let display_name = self.sockets.display_name.clone();
        let spawn_cfg = self.spawn_cfg.clone();
        let to_main = self.to_main.clone();
        let done_hook = self.spawner_done.clone();

        let spawner = thread::Builder::new()
            .name("emskin-xwls-spawner".to_owned())
            .spawn(move || {
                spawn_and_wait(
                    display_name,
                    spawn_cfg,
                    unix_clone,
                    abstract_clone,
                    to_main,
                    done_hook,
                );
            })?;

        self.state = WatchState::Running { spawner };
        Ok(())
    }

    pub fn on_rearm<D: HasXwls + 'static>(
        &mut self,
        handle: &LoopHandle<'static, D>,
    ) -> std::io::Result<()> {
        // Drain any pending connections the dead satellite couldn't
        // serve; otherwise the Generic source would fire immediately on
        // re-arm and busyloop us.
        drain_pending_in_place(&mut self.sockets)?;

        // Reset state (in case we're in Running) before re-arming.
        self.state = WatchState::Disarmed;
        self.arm(handle)
    }
}

/// Per-thread spawn+wait routine. Mirrors niri `spawn_and_wait`.
fn spawn_and_wait(
    display_name: String,
    spawn_cfg: SpawnConfig,
    unix_fd: OwnedFd,
    abstract_fd: Option<OwnedFd>,
    to_main: Sender<ToMain>,
    done_hook: Option<mpsc::Sender<()>>,
) {
    let unix_raw = unix_fd.as_raw_fd();
    let abstract_raw = abstract_fd.as_ref().map(|fd| fd.as_raw_fd());

    tracing::info!(
        display = %display_name,
        binary = %spawn_cfg.binary.display(),
        "xwayland-satellite: spawner thread executing {}",
        spawn_cfg.binary.display()
    );
    let mut cmd = build_spawn_command_raw(&spawn_cfg, &display_name, unix_raw, abstract_raw);

    let wait_result = match cmd.spawn() {
        Ok(mut child) => {
            let pid = child.id();
            tracing::info!(
                display = %display_name,
                pid,
                "xwayland-satellite: child spawned"
            );
            // After spawn, the child has inherited the fds; the cloned
            // OwnedFds in this thread can be dropped.
            drop(unix_fd);
            drop(abstract_fd);
            child.wait().ok()
        }
        Err(e) => {
            tracing::warn!("xwayland-satellite spawn failed: {e}");
            None
        }
    };
    if let Some(status) = wait_result {
        tracing::info!(
            display = %display_name,
            "xwayland-satellite exited with {status}; requesting rearm"
        );
    } else {
        tracing::warn!(
            display = %display_name,
            "xwayland-satellite: spawner exiting without a child status; requesting rearm"
        );
    }

    if let Some(tx) = done_hook {
        let _ = tx.send(());
    }
    let _ = to_main.send(ToMain::Rearm);
}

/// Drain any pending connections on the sockets' fds so a stale readable
/// queue doesn't retrigger the Generic source immediately after re-arm.
fn drain_pending_in_place(sockets: &mut X11Sockets) -> std::io::Result<()> {
    let placeholder = sockets.unix_fd.try_clone()?;
    let old_unix = std::mem::replace(&mut sockets.unix_fd, placeholder);
    sockets.unix_fd = clear_out_pending_connections(old_unix);

    if let Some(ab) = sockets.abstract_fd.take() {
        sockets.abstract_fd = Some(clear_out_pending_connections(ab));
    }
    Ok(())
}
