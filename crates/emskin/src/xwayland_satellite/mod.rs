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

/// Which backend provides XWayland services.
///
/// `Smithay` uses the in-tree smithay `X11Wm` path (the original
/// implementation). `Satellite` uses the niri-style on-demand
/// xwayland-satellite supervisor from this module.
///
/// The default is `Smithay` while the satellite backend is under
/// evaluation (issue #50 suggestion 2 & 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum XwaylandBackend {
    #[default]
    Smithay,
    Satellite,
}

impl std::fmt::Display for XwaylandBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Smithay => f.write_str("smithay"),
            Self::Satellite => f.write_str("satellite"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum as _;

    #[test]
    fn default_backend_is_smithay() {
        assert_eq!(XwaylandBackend::default(), XwaylandBackend::Smithay);
    }

    #[test]
    fn value_enum_parses_lowercase_names() {
        assert_eq!(
            XwaylandBackend::from_str("smithay", true).unwrap(),
            XwaylandBackend::Smithay
        );
        assert_eq!(
            XwaylandBackend::from_str("satellite", true).unwrap(),
            XwaylandBackend::Satellite
        );
    }

    #[test]
    fn value_enum_rejects_unknown_names() {
        assert!(XwaylandBackend::from_str("xorg", true).is_err());
        assert!(XwaylandBackend::from_str("", true).is_err());
    }

    #[test]
    fn display_round_trips_value_enum() {
        for b in [XwaylandBackend::Smithay, XwaylandBackend::Satellite] {
            let s = b.to_string();
            assert_eq!(XwaylandBackend::from_str(&s, true).unwrap(), b);
        }
    }
}
