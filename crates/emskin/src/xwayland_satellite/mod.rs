//! Host-managed xwayland-satellite integration (niri-pattern).
//!
//! emskin pre-binds the X11 display sockets (`/tmp/.X11-unix/X<N>` + the
//! Linux abstract socket) itself, then lazily spawns `xwayland-satellite`
//! only when an X11 client connects. This matches niri's approach and
//! replaces the former in-tree smithay `X11Wm` path.
//!
//! # Derived from
//!
//! The socket-setup + spawn helpers are ported from niri
//! (`src/utils/xwayland/`, GPL-3.0-or-later) — license-compatible with
//! emskin. See `niri` upstream for history. Per-function attribution in the
//! respective submodules.
//!
//! # Scope of this module
//!
//! - [`sockets`]: X11 lock file + unix/abstract socket allocation, RAII
//!   cleanup.
//! - [`spawn`]: binary probing (`--test-listenfd-support`) and `Command`
//!   construction that hands sockets to the child via `-listenfd`.
//!
//! Calloop event-loop integration (arm/disarm/re-arm on child exit) lives
//! in a follow-up module — this crate-level `mod.rs` only re-exports the
//! pure pieces.

pub mod sockets;
pub mod spawn;
pub mod watch;

pub use sockets::{setup_connection, X11Sockets};
pub use spawn::{build_spawn_command, build_spawn_command_raw, test_ondemand, SpawnConfig};
pub use watch::{HasXwls, ToMain, XwlsIntegration};
