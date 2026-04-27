//! Event loop tick — the per-frame idle callback for the compositor.

use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

use crate::ipc::OutgoingMessage;
use crate::state::EmskinState;
use crate::workspace::Workspace;

/// Called once per event loop iteration. Handles workspace lifecycle,
/// IPC dispatch, clipboard events, and pending geometry timeouts.
pub fn event_loop_tick(state: &mut EmskinState) {
    // --- Check if Emacs child process has exited ---
    if let Some(child) = state.emacs.child_mut() {
        if let Ok(Some(status)) = child.try_wait() {
            tracing::info!("Emacs exited with {status}, stopping compositor");
            state.loop_signal.stop();
        }
    }

    // --- Reap the bar child if it exited unexpectedly ---
    // The bar is non-critical; a crash shouldn't take down the compositor,
    // but leaving it as a zombie would be visible as <defunct> in ps.
    if let Some(ref mut child) = state.bar_child {
        if let Ok(Some(status)) = child.try_wait() {
            tracing::warn!("emskin-bar exited with {status}");
            state.bar_child = None;
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

    state.ipc.flush();

    // --- Process clipboard events from host compositor ---
    // `OwnedFd` backends (data-control, X11) are driven by their calloop
    // fd source in `main::register_clipboard_source`, so we just drain
    // events here. `Piggyback` backends (wl_data_device on winit's
    // shared connection) have no owned fd — this tick is the only point
    // at which we can collect buffered events from libwayland's per-queue
    // ring before draining them.
    let clipboard_events = state
        .selection
        .clipboard
        .as_mut()
        .map(|c| {
            if matches!(c.driver(), emskin_clipboard::Driver::Piggyback) {
                c.dispatch();
            }
            c.take_events()
        })
        .unwrap_or_default();
    let has_clipboard_events = !clipboard_events.is_empty();
    for event in clipboard_events {
        crate::clipboard_bridge::handle_clipboard_event(state, event);
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
            .unwrap_or(state.workspace.active_id);
        if let Some(space) = state.workspace.space_for_mut(ws_id) {
            space.map_element(window, geo.loc, false);
        }
        tracing::debug!("embedded app window_id={window_id} geometry force-committed (timeout)");
    }

    // Drain broker-observed fcitx5 events and drive winit IME in
    // emskin-winit-local coords.
    drain_fcitx_events(state);

    // Poll text_input_v3 cursor freshness — picks up the focused
    // client's `set_cursor_rectangle` request the moment it lands,
    // without waiting for a host IME event. Fixes "popup at fallback
    // (surface origin) until first preedit" for tip-only clients
    // (Alacritty etc.) whose cursor_rectangle arrives async after
    // focus.
    state.ime.poll_tip_freshness(&state.seat, &state.apps);
}

/// Drain broker-observed fcitx5 events and hand them to the IME
/// bridge. Each event is translated relative to the currently focused
/// embedded app's emskin-space origin so the cursor rect reaches
/// winit in emskin-winit-local coordinates.
///
/// Marks `needs_redraw` so the staged `pending_cursor_area` /
/// `pending_ime_enabled` reach winit within the next render frame
/// — without this, apply-on-redraw can lag one keystroke behind the
/// caret if the render loop is idle between input events, and the
/// candidate popup visibly drifts.
fn drain_fcitx_events(state: &mut EmskinState) {
    let Some(broker) = state.dbus.broker.as_mut() else {
        return;
    };
    let events = broker.drain_events();
    if events.is_empty() {
        return;
    }
    state.needs_redraw = true;
    let origin = focused_app_origin(state);
    for event in events {
        state
            .ime
            .on_fcitx_event(event, origin, &state.seat, &state.apps);
    }
}

/// Emskin-space origin of the app whose DBus fcitx5 IC is currently
/// active. Added to the client-reported caret rect to translate it
/// into emskin-winit-local coordinates before we hand it to winit IME.
///
/// - Emacs main surface: origin is `(0, 0)` — Emacs's wl_surface IS
///   the emskin winit window, so its surface-local caret coords are
///   already emskin-winit-local.
/// - Embedded app (xwayland-satellite or Wayland native): origin is
///   the buffer top-left inside the emskin Space. Subtracts
///   `geometry().loc` to back out any CSD shadow padding, matching
///   the convention in `Space::render_location`.
fn focused_app_origin(state: &EmskinState) -> Option<[i32; 2]> {
    let kb = state.seat.get_keyboard()?;
    let focus = kb.current_focus()?;
    let window = match focus {
        crate::state::KeyboardFocusTarget::Window(w) => w,
        _ => return None,
    };
    let surface = window.wl_surface()?;
    if state.emacs.is_main_surface(&surface) {
        return Some([0, 0]);
    }
    let loc = state.workspace.active_space.element_location(&window)?;
    let geo_offset = window.geometry().loc;
    Some([loc.x - geo_offset.x, loc.y - geo_offset.y])
}

fn process_pending_toplevels(state: &mut EmskinState) {
    let pending = std::mem::take(&mut state.workspace.pending_emacs_toplevels);
    if pending.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for (surface, window) in pending {
        if surface.parent().is_some() {
            // Child frame (posframe, etc.) — leave in current space, GTK manages.
            tracing::info!(
                "Emacs child frame confirmed (has parent), workspace {}",
                state.workspace.active_id
            );
        } else {
            // Real new Emacs frame — create workspace.
            state.workspace.active_space.unmap_elem(&window);
            let ws_id = state.workspace.alloc_id();
            tracing::info!("new Emacs frame → workspace {ws_id}");

            // Create workspace first (before computing geometry, because
            // workspace_count() affects bar_height which affects emacs_geometry).
            let emacs_wl = surface.wl_surface().clone();
            let mut new_space = smithay::desktop::Space::default();
            if let Some(output) = state.workspace.active_space.outputs().next().cloned() {
                new_space.map_output(&output, (0, 0));
            }

            state.workspace.inactive.insert(
                ws_id,
                Workspace {
                    space: new_space,
                    emacs_surface: Some(emacs_wl),
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
                // Emacs sits at the bottom of the stack so future app
                // toplevels migrate above it naturally.
                if let Some(ws) = state.workspace.inactive.get_mut(&ws_id) {
                    ws.space.map_element(window.clone(), geo.loc, false);
                    ws.space.lower_element(&window);
                }
            }

            state.ipc.send(OutgoingMessage::WorkspaceCreated {
                workspace_id: ws_id,
            });

            // Switch immediately.
            state.switch_workspace(ws_id);
        }
    }
}

fn process_workspace_actions(state: &mut EmskinState) {
    let actions = state.workspace.protocol.take_pending_actions();
    if actions.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for action in actions {
        use crate::protocols::workspace::WorkspaceAction;
        match action {
            WorkspaceAction::Activate(id) => {
                state.switch_workspace(id);
            }
            WorkspaceAction::Remove(id) => {
                if id != state.workspace.active_id {
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
        .workspace
        .inactive
        .iter()
        .filter(|(_, ws)| ws.emacs_surface.as_ref().is_none_or(|s| !s.is_alive()))
        .map(|(id, _)| *id)
        .collect();
    let had_dead = !dead_ws.is_empty();
    for ws_id in dead_ws {
        state.destroy_workspace(ws_id);
        state.ipc.send(OutgoingMessage::WorkspaceDestroyed {
            workspace_id: ws_id,
        });
        tracing::info!("workspace {ws_id} destroyed (Emacs frame died)");
    }
    if had_dead {
        state.needs_redraw = true;
    }

    // Detect active Emacs frame death.
    if state.emacs.main_died() {
        if let Some(&fallback_id) = state.workspace.inactive.keys().next() {
            tracing::info!("active Emacs died, switching to workspace {fallback_id}");
            state.switch_workspace(fallback_id);
            state.needs_redraw = true;
        } else {
            tracing::info!("last Emacs frame died, stopping");
            state.loop_signal.stop();
        }
    }
}

fn refresh_workspace_state(state: &mut EmskinState) {
    let ws_ids = state.workspace.all_ids();

    // Build (id, &name) pairs — borrow from state, no cloning.
    let ws_named: Vec<(u64, &str)> = ws_ids
        .iter()
        .map(|&id| {
            let name: &str = if id == state.workspace.active_id {
                &state.workspace.active_name
            } else {
                state
                    .workspace
                    .inactive
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
                active: id == state.workspace.active_id,
            }
        })
        .collect();
    if let Some(output) = state.workspace.active_space.outputs().next().cloned() {
        let dh = state.display_handle.clone();
        state.workspace.protocol.refresh(&dh, &ws_infos, &output);
    }
    state.workspace.protocol.cleanup_dead();

    // External workspace bar (emskin-bar) consumes ext-workspace-v1 directly;
    // compositor no longer pushes workspace list into an internal overlay.
    let _ = ws_named;
}

fn cleanup_dead_apps(state: &mut EmskinState) {
    let dead = state.apps.drain_dead();
    if dead.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for app in &dead {
        if let Some(space) = state.workspace.space_for_mut(app.workspace_id) {
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
            let target = state.emacs_focus_target();
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(state, target, serial);
            tracing::debug!("focus returned to Emacs after window destroy");
        }
    }
}
