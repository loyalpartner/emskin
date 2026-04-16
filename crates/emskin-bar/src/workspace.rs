//! `ext-workspace-v1` client dispatch.
//!
//! The protocol is event-driven and batched: the manager sends a series of
//! `workspace_group` / `workspace` announcements plus per-handle `id` /
//! `name` / `state` events, then a single `done` that marks the batch
//! complete. We accumulate into `BarState::pending_workspaces` during the
//! batch, swap it into `workspaces` on `done`, and re-evaluate visibility.
//!
//! Workspace IDs on the wire are strings like `"emskin-ws-3"` — we peel the
//! prefix off to recover the numeric id the rest of the codebase uses.

use wayland_client::{event_created_child, Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::{
    self, ExtWorkspaceGroupHandleV1,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::{
    self, ExtWorkspaceHandleV1, State as WsState,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::{
    self, ExtWorkspaceManagerV1,
};

// Opcode constants from ext-workspace-v1.xml. wayland-protocols doesn't
// export `EVT_*` constants for staging protocols, so we declare them here.
/// Opcode for `ext_workspace_manager_v1.workspace_group` (creates group handle).
const MANAGER_EVT_WORKSPACE_GROUP: u16 = 0;
/// Opcode for `ext_workspace_manager_v1.workspace` (creates workspace handle).
const MANAGER_EVT_WORKSPACE: u16 = 1;

use crate::state::BarState;

/// One workspace tile's state. `hit_rect` is filled in by `render.rs` on the
/// most recent paint so pointer click handling can reuse it.
pub struct WorkspaceEntry {
    pub handle: ExtWorkspaceHandleV1,
    pub id: u64,
    pub name: String,
    pub active: bool,
    /// x, y, w, h in buffer pixels (filled by the renderer).
    pub hit_rect: (i32, i32, i32, i32),
}

impl WorkspaceEntry {
    fn new(handle: ExtWorkspaceHandleV1) -> Self {
        Self {
            handle,
            id: 0,
            name: String::new(),
            active: false,
            hit_rect: (0, 0, 0, 0),
        }
    }
}

/// Parse the wire id string (`"emskin-ws-N"` today) back into the numeric
/// id. Unknown formats yield 0 — they still show up in the bar, they just
/// can't be deduped by id.
fn parse_id(wire: &str) -> u64 {
    wire.rsplit('-')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

// =========================================================================
// Manager — top-level events
// =========================================================================

impl Dispatch<ExtWorkspaceManagerV1, ()> for BarState {
    // `workspace_group` and `workspace` events carry a new_id — wayland-client
    // requires us to declare the UserData for the newly-created child proxy
    // or it panics at dispatch time.
    event_created_child!(BarState, ExtWorkspaceManagerV1, [
        MANAGER_EVT_WORKSPACE_GROUP => (ExtWorkspaceGroupHandleV1, ()),
        MANAGER_EVT_WORKSPACE => (ExtWorkspaceHandleV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_workspace_manager_v1::Event;
        match event {
            Event::WorkspaceGroup { .. } => {
                // A group exists; we don't care which output it's on — the
                // bar assumes a single output universe. The dispatch for
                // ExtWorkspaceGroupHandleV1 is implemented below so events
                // on the group handle arrive normally.
            }
            Event::Workspace { workspace } => {
                // New workspaces land in `pending` and graduate to the
                // committed list on the next `done`. Between batches the
                // committed list receives id/name/state updates directly.
                state
                    .pending_workspaces
                    .push(WorkspaceEntry::new(workspace));
            }
            Event::Done => {
                state.workspaces.append(&mut state.pending_workspaces);
                state.workspaces.sort_by_key(|w| w.id);
                tracing::debug!(
                    "done: {} workspaces, active={:?}",
                    state.workspaces.len(),
                    state
                        .workspaces
                        .iter()
                        .filter(|w| w.active)
                        .map(|w| w.id)
                        .collect::<Vec<_>>(),
                );
                state.update_visibility(qh);
            }
            Event::Finished => {
                // Compositor signalled it's done talking to us — behave as if
                // the bar should retire.
                state.workspaces.clear();
                state.update_visibility(qh);
                state.exit = true;
            }
            _ => {}
        }
    }
}

// =========================================================================
// Group — output membership + workspace membership
// =========================================================================

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for BarState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtWorkspaceGroupHandleV1,
        event: ext_workspace_group_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use ext_workspace_group_handle_v1::Event;
        // We don't currently use group-level info — workspace_enter /
        // workspace_leave would matter for a multi-output bar, but the
        // first version assumes one group.
        if let Event::Removed = event {
            tracing::debug!("workspace group removed");
        }
    }
}

// =========================================================================
// Handle — per-workspace metadata + state
// =========================================================================

impl Dispatch<ExtWorkspaceHandleV1, ()> for BarState {
    fn event(
        state: &mut Self,
        proxy: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use ext_workspace_handle_v1::Event;

        // The entry we modify may be in either the pending batch (this round's
        // updates) or the already-committed `workspaces` list (sticky
        // per-handle events between rounds).
        let entry = find_entry_mut(state, proxy);
        let Some(entry) = entry else { return };

        match event {
            Event::Id { id } => {
                entry.id = parse_id(&id);
            }
            Event::Name { name } => {
                entry.name = name;
            }
            Event::State {
                state: wayland_client::WEnum::Value(flags),
            } => {
                let new_active = flags.contains(WsState::Active);
                if new_active != entry.active {
                    tracing::debug!(
                        "workspace {} active {} → {}",
                        entry.id,
                        entry.active,
                        new_active
                    );
                }
                entry.active = new_active;
            }
            Event::Removed => {
                // Protocol: the server destroys the handle immediately after
                // sending `removed`, so the client MUST NOT issue further
                // requests (including `destroy`) on it. Just forget it.
                let handle_clone = proxy.clone();
                state
                    .pending_workspaces
                    .retain(|w| w.handle != handle_clone);
                state.workspaces.retain(|w| w.handle != handle_clone);
            }
            _ => {}
        }
    }
}

fn find_entry_mut<'a>(
    state: &'a mut BarState,
    handle: &ExtWorkspaceHandleV1,
) -> Option<&'a mut WorkspaceEntry> {
    if let Some(i) = state
        .pending_workspaces
        .iter()
        .position(|w| &w.handle == handle)
    {
        return state.pending_workspaces.get_mut(i);
    }
    if let Some(i) = state.workspaces.iter().position(|w| &w.handle == handle) {
        return state.workspaces.get_mut(i);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_id;

    #[test]
    fn parses_emskin_wire_format() {
        assert_eq!(parse_id("emskin-ws-3"), 3);
        assert_eq!(parse_id("emskin-ws-42"), 42);
    }

    #[test]
    fn falls_back_to_zero_for_unknown_formats() {
        assert_eq!(parse_id(""), 0);
        assert_eq!(parse_id("foo"), 0);
        assert_eq!(parse_id("workspace"), 0);
        // Takes the last dash-separated token, so this is intentional:
        assert_eq!(parse_id("custom-7"), 7);
    }
}
