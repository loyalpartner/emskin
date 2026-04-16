//! ext-workspace-v1 protocol implementation.
//!
//! Exposes emskin's workspace state to Wayland clients via the standard
//! ext-workspace-v1 protocol. Enables external bars and embedded apps to
//! discover and switch workspaces.
//!
//! Design: diff-based refresh (inspired by EWM). `refresh()` runs every
//! event loop tick, diffs real state vs. last snapshot, sends only changes.

use std::collections::{HashMap, HashSet};

use smithay::{
    output::Output,
    reexports::wayland_server::{
        backend::ClientId, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    },
};

use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::EmskinState;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Snapshot of one workspace for diff-based refresh.
#[derive(Clone, Debug)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub name: String,
    pub active: bool,
}

/// Actions queued by clients, processed on `commit`.
#[derive(Debug)]
pub enum WorkspaceAction {
    Activate(u64),
    Deactivate(u64),
    Remove(u64),
    CreateWorkspace(String),
}

// ---------------------------------------------------------------------------
// Per-client state
// ---------------------------------------------------------------------------

struct ManagerInstance {
    manager: ExtWorkspaceManagerV1,
    group: Option<ExtWorkspaceGroupHandleV1>,
    /// workspace_id → protocol handle
    workspaces: HashMap<u64, ExtWorkspaceHandleV1>,
    /// Last snapshot sent — for diff.
    last_snapshot: Vec<WorkspaceInfo>,
    /// Pending actions from this client (processed on commit).
    actions: Vec<WorkspaceAction>,
    stopped: bool,
}

// ---------------------------------------------------------------------------
// Global protocol state
// ---------------------------------------------------------------------------

pub struct WorkspaceProtocolState {
    instances: Vec<ManagerInstance>,
}

impl WorkspaceProtocolState {
    pub fn new(dh: &DisplayHandle) -> Self {
        dh.create_global::<EmskinState, ExtWorkspaceManagerV1, ()>(1, ());
        tracing::info!("ext-workspace-v1 global registered");
        Self {
            instances: Vec::new(),
        }
    }

    /// Diff-based refresh: compare real workspace state against each client's
    /// last snapshot, send only changed events, then `done`.
    pub fn refresh(&mut self, dh: &DisplayHandle, workspaces: &[WorkspaceInfo], output: &Output) {
        for inst in &mut self.instances {
            if inst.stopped || !inst.manager.is_alive() {
                continue;
            }
            let changed = Self::refresh_instance(inst, dh, workspaces, output);
            if changed {
                inst.manager.done();
                inst.last_snapshot = workspaces.to_vec();
            }
        }
    }

    /// Drain all pending actions from all clients.
    pub fn take_pending_actions(&mut self) -> Vec<WorkspaceAction> {
        let mut actions = Vec::new();
        for inst in &mut self.instances {
            actions.append(&mut inst.actions);
        }
        actions
    }

    /// Clean up dead instances.
    pub fn cleanup_dead(&mut self) {
        self.instances.retain(|inst| inst.manager.is_alive());
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn refresh_instance(
        inst: &mut ManagerInstance,
        dh: &DisplayHandle,
        workspaces: &[WorkspaceInfo],
        _output: &Output,
    ) -> bool {
        let mut changed = false;
        let Some(client) = inst.manager.client() else {
            return false;
        };

        // Ensure the workspace group exists.
        if inst.group.is_none() {
            let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, EmskinState>(
                dh,
                inst.manager.version(),
                (),
            ) else {
                return false;
            };
            inst.manager.workspace_group(&group);
            group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::CreateWorkspace);
            inst.group = Some(group);
            changed = true;
        }

        let group = inst.group.as_ref().unwrap();

        // Build set of current workspace ids.
        let current_ids: HashSet<u64> = workspaces.iter().map(|w| w.id).collect();

        // Remove workspaces that no longer exist.
        let dead_ids: Vec<u64> = inst
            .workspaces
            .keys()
            .filter(|id| !current_ids.contains(id))
            .copied()
            .collect();
        for id in dead_ids {
            if let Some(handle) = inst.workspaces.remove(&id) {
                group.workspace_leave(&handle);
                handle.removed();
                changed = true;
            }
        }

        // Create or update workspaces.
        for ws in workspaces {
            if !inst.workspaces.contains_key(&ws.id) {
                // New workspace — create handle.
                let Ok(handle) = client.create_resource::<ExtWorkspaceHandleV1, _, EmskinState>(
                    dh,
                    inst.manager.version(),
                    (),
                ) else {
                    continue;
                };
                inst.manager.workspace(&handle);
                handle.id(format!("emskin-ws-{}", ws.id));
                handle.name(ws.name.clone());
                let coords: Vec<u8> = (ws.id as u32).to_le_bytes().to_vec();
                handle.coordinates(coords);
                handle.capabilities(
                    ext_workspace_handle_v1::WorkspaceCapabilities::Activate
                        | ext_workspace_handle_v1::WorkspaceCapabilities::Deactivate
                        | ext_workspace_handle_v1::WorkspaceCapabilities::Remove,
                );
                handle.state(if ws.active {
                    ext_workspace_handle_v1::State::Active
                } else {
                    ext_workspace_handle_v1::State::empty()
                });
                group.workspace_enter(&handle);
                inst.workspaces.insert(ws.id, handle);
                changed = true;
                continue;
            }

            // Existing workspace — check for changes.
            let handle = inst.workspaces.get(&ws.id).unwrap();
            let old = inst.last_snapshot.iter().find(|s| s.id == ws.id);

            if old.map(|o| &o.name) != Some(&ws.name) {
                handle.name(ws.name.clone());
                changed = true;
            }

            let old_active = old.map(|o| o.active).unwrap_or(false);
            if old_active != ws.active {
                handle.state(if ws.active {
                    ext_workspace_handle_v1::State::Active
                } else {
                    ext_workspace_handle_v1::State::empty()
                });
                changed = true;
            }
        }

        changed
    }
}

// ---------------------------------------------------------------------------
// GlobalDispatch: client binds ext_workspace_manager_v1
// ---------------------------------------------------------------------------

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for EmskinState {
    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        state.workspace_protocol.instances.push(ManagerInstance {
            manager,
            group: None,
            workspaces: HashMap::new(),
            last_snapshot: Vec::new(),
            actions: Vec::new(),
            stopped: false,
        });
        tracing::debug!("ext-workspace-v1: client bound");
    }
}

// ---------------------------------------------------------------------------
// Dispatch: ext_workspace_manager_v1 (commit, stop)
// ---------------------------------------------------------------------------

impl Dispatch<ExtWorkspaceManagerV1, ()> for EmskinState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(inst) = state
            .workspace_protocol
            .instances
            .iter_mut()
            .find(|i| Resource::id(&i.manager) == Resource::id(resource))
        else {
            return;
        };

        match request {
            ext_workspace_manager_v1::Request::Commit => {
                tracing::debug!("ext-workspace-v1: commit");
            }
            ext_workspace_manager_v1::Request::Stop => {
                inst.stopped = true;
                resource.finished();
                tracing::debug!("ext-workspace-v1: stop");
            }
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: ClientId,
        resource: &ExtWorkspaceManagerV1,
        _data: &(),
    ) {
        state
            .workspace_protocol
            .instances
            .retain(|i| i.manager.id() != resource.id());
    }
}

// ---------------------------------------------------------------------------
// Dispatch: ext_workspace_group_handle_v1 (create_workspace, destroy)
// ---------------------------------------------------------------------------

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for EmskinState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { workspace } => {
                tracing::info!("ext-workspace-v1: create_workspace({workspace})");
                if let Some(inst) = state.workspace_protocol.instances.first_mut() {
                    inst.actions
                        .push(WorkspaceAction::CreateWorkspace(workspace));
                }
            }
            ext_workspace_group_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch: ext_workspace_handle_v1 (activate, deactivate, remove, assign, destroy)
// ---------------------------------------------------------------------------

impl Dispatch<ExtWorkspaceHandleV1, ()> for EmskinState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Find workspace_id for this handle by comparing ObjectId.
        let resource_oid = Resource::id(resource);
        let ws_id = state.workspace_protocol.instances.iter().find_map(|inst| {
            inst.workspaces
                .iter()
                .find(|(_, h)| Resource::id(*h) == resource_oid)
                .map(|(&id, _)| id)
        });
        let Some(ws_id) = ws_id else { return };

        let inst = state.workspace_protocol.instances.iter_mut().find(|i| {
            i.workspaces
                .values()
                .any(|h| Resource::id(h) == resource_oid)
        });

        match request {
            ext_workspace_handle_v1::Request::Activate => {
                tracing::info!("ext-workspace-v1: activate {ws_id}");
                if let Some(inst) = inst {
                    inst.actions.push(WorkspaceAction::Activate(ws_id));
                }
            }
            ext_workspace_handle_v1::Request::Deactivate => {
                tracing::info!("ext-workspace-v1: deactivate {ws_id}");
                if let Some(inst) = inst {
                    inst.actions.push(WorkspaceAction::Deactivate(ws_id));
                }
            }
            ext_workspace_handle_v1::Request::Remove => {
                tracing::info!("ext-workspace-v1: remove {ws_id}");
                if let Some(inst) = inst {
                    inst.actions.push(WorkspaceAction::Remove(ws_id));
                }
            }
            ext_workspace_handle_v1::Request::Assign { .. } => {}
            ext_workspace_handle_v1::Request::Destroy => {}
            _ => {}
        }
    }
}
