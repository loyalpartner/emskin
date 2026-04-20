//! `emez` — a minimal smithay-based Wayland compositor used as a private
//! host for emskin's E2E test suite.
//!
//! No rendering, no window management, no input injection. It just
//! advertises the Wayland globals emskin (and any other wayland clients)
//! need to make a round-trip: `wl_compositor`, `wl_shm`, `wl_seat`,
//! `wl_output`, `xdg_shell`, `wl_data_device_manager`,
//! `zwp_primary_selection_device_manager_v1`, and — critically —
//! `zwlr_data_control_v1` + `ext_data_control_v1`, which are missing
//! from weston and force emskin onto its X11 clipboard fallback.

use clap::Parser;
use smithay::reexports::{
    calloop::{EventLoop, LoopSignal},
    wayland_server::Display,
};

mod handlers;
mod state;
mod xwayland;

use state::Emez;

#[derive(Parser)]
#[command(
    name = "emez",
    about = "Minimal smithay-based wayland compositor for emskin E2E tests"
)]
struct Cli {
    /// Pin the Wayland socket name (default: auto-chosen wayland-N).
    #[arg(long)]
    socket: Option<String>,

    /// Redirect tracing to a file instead of stderr.
    #[arg(long)]
    log_file: Option<std::path::PathBuf>,

    /// Spawn an embedded XWayland so outside X clients can participate
    /// in tests against this host. Default: off.
    #[arg(long, default_value_t = false)]
    xwayland: bool,

    /// Pin the XWayland DISPLAY number. Without this, smithay scans for
    /// a free number which races under parallel emez spawns. The test
    /// harness uses this to pre-allocate a unique number per test.
    #[arg(long)]
    xwayland_display: Option<u32>,

    /// When XWayland reports Ready, write `:<display>` to this path. The
    /// test harness polls for the file's existence as a readiness
    /// barrier (XWayland takes 100-300ms to finish its X11 handshake).
    #[arg(long)]
    xwayland_ready_file: Option<std::path::PathBuf>,

    /// Hide the data-control globals (`zwlr_data_control_v1` +
    /// `ext_data_control_v1`) from all clients. Simulates KDE/GNOME,
    /// where no data-control extension is advertised. Used by E2E tests
    /// that exercise emskin's `wl_data_device` clipboard fallback path.
    #[arg(long, default_value_t = false)]
    no_data_control: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_logging(cli.log_file.as_deref());

    let mut event_loop: EventLoop<'static, Emez> = EventLoop::try_new()?;
    let display: Display<Emez> = Display::new()?;
    let mut state = Emez::new(
        &mut event_loop,
        display,
        cli.socket.as_deref(),
        cli.no_data_control,
    )?;

    tracing::info!(
        "emez ready on WAYLAND_DISPLAY={}",
        state.socket_name.to_string_lossy()
    );

    if cli.xwayland {
        state.start_xwayland(cli.xwayland_display, cli.xwayland_ready_file.clone())?;
        tracing::info!(
            "emez XWayland requested (display={:?}, ready_file={:?})",
            cli.xwayland_display,
            cli.xwayland_ready_file
        );
    }

    install_signal_handler(state.loop_signal.clone())?;

    event_loop.run(None, &mut state, |_| {})?;

    // Belt-and-braces X socket cleanup. Dropping `state` + `event_loop`
    // drives smithay's `X11Lock::Drop` which removes these files, but
    // signal-driven shutdowns have been observed to exit with 128+SIG
    // before the full Rust Drop chain settles. This is harmless when
    // the normal path already removed them (the ENOENT branch is
    // expected); anything else is a genuine cleanup failure worth
    // surfacing so the next test doesn't inherit the residue silently.
    if let Some(n) = state.xdisplay {
        for path in [format!("/tmp/.X11-unix/X{n}"), format!("/tmp/.X{n}-lock")] {
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(path, %e, "emez: failed to clean up X file on exit");
                }
            }
        }
    }
    drop(state);
    Ok(())
}

/// Spawn a background thread that listens for SIGTERM/SIGINT and
/// signals the event loop to exit. signal-hook uses a self-pipe
/// internally, so the thread is a plain blocking reader — no
/// async-signal-safe gymnastics in the signal handler.
fn install_signal_handler(loop_signal: LoopSignal) -> Result<(), Box<dyn std::error::Error>> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT])?;
    std::thread::Builder::new()
        .name("emez-signal-handler".into())
        .spawn(move || {
            if let Some(sig) = signals.forever().next() {
                tracing::info!("emez received signal {sig}, stopping event loop");
                // calloop 0.14: stop() sets a flag; wakeup() pokes the
                // poller so the in-flight epoll_wait returns immediately.
                // Without wakeup(), the flag isn't observed until the
                // next natural event (might never come in a headless
                // host), so emez would hang until SIGKILL.
                loop_signal.stop();
                loop_signal.wakeup();
            }
        })?;
    Ok(())
}

fn init_logging(log_file: Option<&std::path::Path>) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().ok();
    match log_file {
        Some(path) => {
            let file = match std::fs::File::create(path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("emez: failed to open --log-file {}: {e}", path.display());
                    return;
                }
            };
            let builder = tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file));
            match env_filter {
                Some(f) => builder.with_env_filter(f).init(),
                None => builder.init(),
            }
        }
        None => {
            let builder = tracing_subscriber::fmt();
            match env_filter {
                Some(f) => builder.with_env_filter(f).init(),
                None => builder.init(),
            }
        }
    }
}
