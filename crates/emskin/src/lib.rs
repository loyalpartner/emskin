pub mod capture;
pub mod clipboard_bridge;
pub mod handlers;
pub mod input;
pub mod ipc;
pub mod mirror_render;
pub mod protocols;
pub mod recording;
pub mod state;
pub mod tick;
pub mod winit;
pub mod xwayland_satellite;

// Re-export state sub-modules at crate root so existing
// `crate::apps::*`, `crate::focus::*`, `crate::ime::*`,
// `crate::workspace::*` paths keep resolving.
pub use state::{apps, cursor, effects, emacs, focus, ime, workspace, xwayland};
pub use state::{EmskinState, KeyboardFocusTarget};
