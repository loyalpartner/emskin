//! Workspace model: each Emacs frame = one workspace. The active
//! workspace's `Space<Window>` lives inline; inactive workspaces are
//! swapped out into `inactive`.
//!
//! Cross-subsystem operations (`switch_workspace`, `destroy_workspace`,
//! `migrate_app_to_active`) stay on `EmskinState` because they touch
//! seat, IME, focus, apps, and IPC. Only pure workspace-local
//! operations live as methods here.

use std::collections::HashMap;

use smithay::{
    desktop::{Space, Window},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::shell::xdg::ToplevelSurface,
};

/// State for an inactive workspace (swapped out when another is active).
pub struct Workspace {
    pub space: Space<Window>,
    pub emacs_surface: Option<WlSurface>,
    /// Display name for the bar (extracted from Emacs frame title).
    pub name: String,
}

/// Workspace-related fields grouped together. Replaces seven loose
/// fields on `EmskinState`.
pub struct WorkspaceState {
    /// The active workspace's space (swapped in/out on switch).
    pub active_space: Space<Window>,
    /// Inactive workspaces, keyed by workspace id.
    pub inactive: HashMap<u64, Workspace>,
    /// Id of the currently active workspace.
    pub active_id: u64,
    /// Display name of the active workspace.
    pub active_name: String,
    /// Next workspace id to allocate.
    pub next_id: u64,
    /// Emacs toplevels awaiting parent() check (child frame detection).
    pub pending_emacs_toplevels: Vec<(ToplevelSurface, Window)>,
    /// ext-workspace-v1 protocol state.
    pub protocol: crate::protocols::workspace::WorkspaceProtocolState,
}

impl WorkspaceState {
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Total number of workspaces (active + inactive).
    pub fn count(&self) -> usize {
        1 + self.inactive.len()
    }

    /// Mutable reference to the `Space<Window>` for a given workspace
    /// id. Returns the active space if `ws_id` matches, otherwise looks
    /// up inactive.
    pub fn space_for_mut(&mut self, ws_id: u64) -> Option<&mut Space<Window>> {
        if ws_id == self.active_id {
            Some(&mut self.active_space)
        } else {
            self.inactive.get_mut(&ws_id).map(|ws| &mut ws.space)
        }
    }

    /// Sorted list of all workspace ids.
    pub fn all_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = std::iter::once(self.active_id)
            .chain(self.inactive.keys().copied())
            .collect();
        ids.sort_unstable();
        ids
    }
}
