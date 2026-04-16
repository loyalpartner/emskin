//! emskin-bar — external workspace bar for emskin.
//!
//! Pure Wayland client built on smithay-client-toolkit. Connects to whatever
//! compositor `WAYLAND_DISPLAY` points at (emskin spawns us with its own
//! socket inherited), subscribes to `ext-workspace-v1`, and shows a
//! top-anchored layer-shell strip whenever ≥ 2 workspaces exist.
//!
//! The bar never touches emskin's private JSON-over-UnixSocket IPC — the
//! protocol boundary is the contract, so this binary can also be replaced by
//! any third-party bar that speaks the same standard Wayland protocols.

mod render;
mod state;
mod workspace;

use wayland_client::globals::registry_queue_init;
use wayland_client::Connection;

use crate::state::BarState;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("EMSKIN_BAR_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .ok();

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let mut state = BarState::new(&globals, &qh)?;

    tracing::info!("emskin-bar started");
    while !state.exit_requested() {
        event_queue.blocking_dispatch(&mut state)?;
    }
    tracing::info!("emskin-bar exiting");
    Ok(())
}
