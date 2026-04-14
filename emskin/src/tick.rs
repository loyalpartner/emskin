//! Event loop tick — the per-frame idle callback for the compositor.

use smithay::reexports::wayland_server::Resource;

use crate::ipc::OutgoingMessage;
use crate::state::{self, EmskinState, Workspace};

/// Called once per event loop iteration. Handles workspace lifecycle,
/// IPC dispatch, clipboard events, and pending geometry timeouts.
pub fn event_loop_tick(state: &mut EmskinState) {
    // --- Check if Emacs child process has exited ---
    if let Some(ref mut child) = state.emacs_child {
        if let Ok(Some(status)) = child.try_wait() {
            tracing::info!("Emacs exited with {status}, stopping compositor");
            state.loop_signal.stop();
        }
    }

    // --- Workspace: process deferred Emacs toplevels ---
    // After dispatch_clients, set_parent has been processed for same-batch
    // toplevels, so surface.parent() is now accurate.
    process_pending_toplevels(state);

    // --- Workspace: process ext-workspace-v1 client actions ---
    process_workspace_actions(state);

    // --- Workspace: detect dead Emacs frames ---
    detect_dead_workspaces(state);

    // --- Workspace: refresh ext-workspace-v1 protocol + bar ---
    refresh_workspace_state(state);

    // --- Clean up destroyed embedded app windows ---
    cleanup_dead_apps(state);

    // --- Dispatch incoming IPC messages from Emacs ---
    if let Some(msgs) = state.ipc.recv_all() {
        for msg in msgs {
            crate::ipc::dispatch::handle_ipc_message(state, msg);
        }
        state.needs_redraw = true;
    }

    // --- Process clipboard events from host compositor ---
    let clipboard_events = state
        .selection
        .clipboard
        .as_mut()
        .map(|c| c.take_events())
        .unwrap_or_default();
    let has_clipboard_events = !clipboard_events.is_empty();
    for event in clipboard_events {
        crate::clipboard_dispatch::handle_clipboard_event(state, event);
    }
    // Flush immediately so Wayland clients see selection changes / send
    // requests without waiting for the next render frame.
    if has_clipboard_events {
        let _ = state.display_handle.flush_clients();
        state.needs_redraw = true;
    }

    // --- Force-commit pending geometries that have timed out (100ms) ---
    let timed_out = state
        .apps
        .collect_timed_out(std::time::Duration::from_millis(100));
    if !timed_out.is_empty() {
        state.needs_redraw = true;
    }
    for (window_id, window, geo) in timed_out {
        let ws_id = state
            .apps
            .get(window_id)
            .map(|a| a.workspace_id)
            .unwrap_or(state.active_workspace_id);
        if let Some(space) = state.space_for_workspace_mut(ws_id) {
            space.map_element(window, geo.loc, false);
        }
        tracing::debug!(
            "embedded app window_id={window_id} geometry force-committed (timeout)"
        );
    }
}

fn process_pending_toplevels(state: &mut EmskinState) {
    let pending = std::mem::take(&mut state.pending_emacs_toplevels);
    if pending.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for (surface, window) in pending {
        if surface.parent().is_some() {
            // Child frame (posframe, etc.) — leave in current space, GTK manages.
            tracing::info!(
                "Emacs child frame confirmed (has parent), workspace {}",
                state.active_workspace_id
            );
        } else {
            // Real new Emacs frame — create workspace.
            state.space.unmap_elem(&window);
            let ws_id = state.alloc_workspace_id();
            tracing::info!("new Emacs frame → workspace {ws_id}");

            // Create workspace first (before computing geometry, because
            // workspace_count() affects bar_height which affects emacs_geometry).
            let emacs_wl = surface.wl_surface().clone();
            let mut new_space = smithay::desktop::Space::default();
            if let Some(output) = state.space.outputs().next().cloned() {
                new_space.map_output(&output, (0, 0));
            }

            state.inactive_workspaces.insert(
                ws_id,
                Workspace {
                    space: new_space,
                    emacs_surface: Some(emacs_wl),
                    emacs_x11_window: None,
                    name: String::new(),
                },
            );

            // Now workspace_count() > 1 → bar appears → emacs_geometry
            // accounts for bar height. Configure the new frame.
            if let Some(geo) = state.emacs_geometry() {
                surface.with_pending_state(|s| {
                    s.size = Some(geo.size);
                    s.states.set(
                        smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen,
                    );
                });
                surface.send_pending_configure();

                // Map window at bar offset in the new workspace's space.
                if let Some(ws) = state.inactive_workspaces.get_mut(&ws_id) {
                    ws.space.map_element(window, geo.loc, false);
                }
            }

            // Resize existing Emacs frames for bar (1→2 workspace transition).
            resize_all_emacs_for_bar(state);

            state
                .ipc
                .send(OutgoingMessage::WorkspaceCreated { workspace_id: ws_id });

            // Switch immediately.
            state.switch_workspace(ws_id);
            state
                .ipc
                .send(OutgoingMessage::WorkspaceSwitched { workspace_id: ws_id });
        }
    }
}

fn process_workspace_actions(state: &mut EmskinState) {
    let actions = state.workspace_protocol.take_pending_actions();
    if actions.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for action in actions {
        use crate::protocols::workspace::WorkspaceAction;
        match action {
            WorkspaceAction::Activate(id) => {
                if state.switch_workspace(id) {
                    state
                        .ipc
                        .send(OutgoingMessage::WorkspaceSwitched { workspace_id: id });
                }
            }
            WorkspaceAction::Remove(id) => {
                if id != state.active_workspace_id {
                    state.destroy_workspace(id);
                    state
                        .ipc
                        .send(OutgoingMessage::WorkspaceDestroyed { workspace_id: id });
                }
            }
            _ => {} // Deactivate / CreateWorkspace: future extension
        }
    }
}

fn detect_dead_workspaces(state: &mut EmskinState) {
    // Detect dead Emacs frames in inactive workspaces.
    let dead_ws: Vec<u64> = state
        .inactive_workspaces
        .iter()
        .filter(|(_, ws)| ws.emacs_surface.as_ref().is_none_or(|s| !s.is_alive()))
        .map(|(id, _)| *id)
        .collect();
    let had_dead = !dead_ws.is_empty();
    for ws_id in dead_ws {
        state.destroy_workspace(ws_id);
        state
            .ipc
            .send(OutgoingMessage::WorkspaceDestroyed { workspace_id: ws_id });
        tracing::info!("workspace {ws_id} destroyed (Emacs frame died)");
    }
    if had_dead {
        // Bar might have disappeared (2→1 workspace) — resize Emacs back to fullscreen.
        resize_all_emacs_for_bar(state);
        state.needs_redraw = true;
    }

    // Detect active Emacs frame death.
    if state
        .emacs_surface
        .as_ref()
        .is_some_and(|s| !s.is_alive())
        && state.initial_size_settled
    {
        if let Some(&fallback_id) = state.inactive_workspaces.keys().next() {
            tracing::info!("active Emacs died, switching to workspace {fallback_id}");
            state.switch_workspace(fallback_id);
            state.ipc.send(OutgoingMessage::WorkspaceSwitched {
                workspace_id: fallback_id,
            });
            state.needs_redraw = true;
        } else {
            tracing::info!("last Emacs frame died, stopping");
            state.loop_signal.stop();
        }
    }
}

fn refresh_workspace_state(state: &mut EmskinState) {
    let ws_ids = state.all_workspace_ids();

    // Build (id, &name) pairs — borrow from state, no cloning.
    let ws_named: Vec<(u64, &str)> = ws_ids
        .iter()
        .map(|&id| {
            let name: &str = if id == state.active_workspace_id {
                &state.active_workspace_name
            } else {
                state
                    .inactive_workspaces
                    .get(&id)
                    .map(|ws| ws.name.as_str())
                    .unwrap_or("")
            };
            (id, name)
        })
        .collect();

    let ws_infos: Vec<crate::protocols::workspace::WorkspaceInfo> = ws_named
        .iter()
        .map(|&(id, name)| {
            let display_name = if name.is_empty() {
                format!("Workspace {id}")
            } else {
                name.to_string()
            };
            crate::protocols::workspace::WorkspaceInfo {
                id,
                name: display_name,
                active: id == state.active_workspace_id,
            }
        })
        .collect();
    if let Some(output) = state.space.outputs().next().cloned() {
        let dh = state.display_handle.clone();
        state.workspace_protocol.refresh(&dh, &ws_infos, &output);
    }
    state.workspace_protocol.cleanup_dead();

    if state.bar_enabled {
        state
            .workspace_bar
            .update(&ws_named, state.active_workspace_id);
    }
}

fn cleanup_dead_apps(state: &mut EmskinState) {
    let dead = state.apps.drain_dead();
    if dead.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for app in &dead {
        if let Some(space) = state.space_for_workspace_mut(app.workspace_id) {
            space.unmap_elem(&app.window);
        }
        state.ipc.send(OutgoingMessage::WindowDestroyed {
            window_id: app.window_id,
        });
        tracing::info!("embedded app window_id={} destroyed", app.window_id);
    }
    // Fall back to Emacs when focus is lost.
    if let Some(keyboard) = state.seat.get_keyboard() {
        if keyboard.current_focus().is_none() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(state, state.emacs_surface.clone(), serial);
            tracing::debug!("focus returned to Emacs after window destroy");
        }
    }
}

/// Resize and reposition all Emacs frames to account for bar height changes.
/// Called when workspace count transitions (1→2 or 2→1).
pub fn resize_all_emacs_for_bar(state: &mut EmskinState) {
    let Some(geo) = state.emacs_geometry() else {
        return;
    };
    tracing::info!(
        "bar transition: emacs geometry = ({},{}) {}x{}",
        geo.loc.x,
        geo.loc.y,
        geo.size.w,
        geo.size.h,
    );

    state::resize_emacs_in_space(
        &mut state.space,
        &state.emacs_surface.clone(),
        &state.emacs_x11_window.clone(),
        geo,
    );
    for ws in state.inactive_workspaces.values_mut() {
        state::resize_emacs_in_space(&mut ws.space, &ws.emacs_surface, &ws.emacs_x11_window, geo);
    }

    state.ipc.send(OutgoingMessage::SurfaceSize {
        width: geo.size.w,
        height: geo.size.h,
    });
}
