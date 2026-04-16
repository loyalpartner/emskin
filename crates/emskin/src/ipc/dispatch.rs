use crate::ipc::IncomingMessage;
use crate::EmskinState;

pub fn handle_ipc_message(state: &mut EmskinState, msg: IncomingMessage) {
    match msg {
        IncomingMessage::SetGeometry { window_id, rect } => {
            ipc_set_geometry(state, window_id, rect);
        }
        IncomingMessage::Close { window_id } => {
            ipc_close(state, window_id);
        }
        IncomingMessage::SetVisibility { window_id, visible } => {
            ipc_set_visibility(state, window_id, visible);
        }
        IncomingMessage::PrefixDone => {
            ipc_prefix_done(state);
        }
        IncomingMessage::AddMirror {
            window_id,
            view_id,
            rect,
        } => {
            ipc_add_mirror(state, window_id, view_id, rect);
        }
        IncomingMessage::UpdateMirrorGeometry {
            window_id,
            view_id,
            rect,
        } => {
            ipc_update_mirror_geometry(state, window_id, view_id, rect);
        }
        IncomingMessage::RemoveMirror { window_id, view_id } => {
            ipc_remove_mirror(state, window_id, view_id);
        }
        IncomingMessage::PromoteMirror { window_id, view_id } => {
            ipc_promote_mirror(state, window_id, view_id);
        }
        IncomingMessage::SetFocus { window_id } => {
            ipc_set_focus(state, window_id);
        }
        IncomingMessage::SetMeasure { enabled } => {
            tracing::debug!("IPC set_measure enabled={enabled}");
            state.measure.borrow_mut().set_enabled(enabled);
        }
        IncomingMessage::SetCursorTrail { enabled } => {
            tracing::debug!("IPC set_cursor_trail enabled={enabled}");
            state.cursor_trail.borrow_mut().set_enabled(enabled);
        }
        IncomingMessage::SetSkeleton { enabled, rects } => {
            tracing::debug!("IPC set_skeleton enabled={enabled} rects={}", rects.len());
            let mut sk = state.skeleton.borrow_mut();
            sk.set_enabled(enabled);
            if enabled {
                sk.set_rects(rects);
            } else {
                sk.clear();
            }
        }
        IncomingMessage::SwitchWorkspace { workspace_id } => {
            tracing::debug!("IPC switch_workspace {workspace_id}");
            // switch_workspace sends WorkspaceSwitched IPC internally
            // (before keyboard.set_focus to avoid race conditions).
            state.switch_workspace(workspace_id);
        }
    }
}

fn ipc_set_geometry(state: &mut EmskinState, window_id: u64, rect: crate::ipc::IpcRect) {
    let crate::ipc::IpcRect { x, y, w, h } = rect;
    tracing::debug!("IPC set_geometry window={window_id} ({x},{y},{w},{h})");
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC set_geometry: invalid size ({w}x{h}), ignoring");
        return;
    }
    // IPC coordinates are relative to Emacs surface. Translate by Emacs
    // frame's current origin (non-zero when a layer surface reserves space
    // at the top via `exclusive_zone`).
    let origin_y = state.emacs_geometry().map_or(0, |g| g.loc.y);
    let ay = y + origin_y;
    let new_geo = smithay::utils::Rectangle::new((x, ay).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    app.visible = true;

    state.migrate_app_to_active(window_id);

    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };

    if let Some(toplevel) = app.window.toplevel() {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
        toplevel.with_pending_state(|s| {
            s.size = Some((w, h).into());
            s.states.set(xdg_toplevel::State::TiledLeft);
            s.states.set(xdg_toplevel::State::TiledRight);
            s.states.set(xdg_toplevel::State::TiledTop);
            s.states.set(xdg_toplevel::State::TiledBottom);
        });
        toplevel.send_pending_configure();

        if app.geometry.is_none() {
            app.geometry = Some(new_geo);
            let window = app.window.clone();
            state.space.map_element(window, new_geo.loc, false);
            tracing::info!(
                "app {window_id} mapped immediately at ({},{}) ws={}",
                new_geo.loc.x,
                new_geo.loc.y,
                state.active_workspace_id
            );
        } else {
            app.pending_geometry = Some(new_geo);
            app.pending_since = Some(std::time::Instant::now());
            tracing::debug!(
                "app {window_id} pending geometry ({},{}) ws={}",
                new_geo.loc.x,
                new_geo.loc.y,
                state.active_workspace_id
            );
        }
    } else if let Some(x11) = app.window.x11_surface() {
        if let Err(e) = x11.configure(new_geo) {
            tracing::warn!("X11 configure failed for window_id={window_id}: {e}");
        }
        app.geometry = Some(new_geo);
        let window = app.window.clone();
        state.space.map_element(window, new_geo.loc, false);
    }
}

fn ipc_close(state: &mut EmskinState, window_id: u64) {
    tracing::debug!("IPC close window={window_id}");
    if let Some(app) = state.apps.get_mut(window_id) {
        if let Some(toplevel) = app.window.toplevel() {
            toplevel.send_close();
        } else if let Some(x11) = app.window.x11_surface() {
            if let Err(e) = x11.close() {
                tracing::warn!("X11 close failed for window_id={window_id}: {e}");
            }
        }
    }
}

fn ipc_set_visibility(state: &mut EmskinState, window_id: u64, visible: bool) {
    tracing::debug!("IPC set_visibility window={window_id} visible={visible}");
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    app.visible = visible;
    let win = app.window.clone();
    let geo = app.geometry;
    if !visible {
        // Unmap from whichever space it's in.
        let ws_id = app.workspace_id;
        if let Some(space) = state.space_for_workspace_mut(ws_id) {
            space.unmap_elem(&win);
        }
    } else if let Some(geo) = geo {
        state.migrate_app_to_active(window_id);
        // Write back geometry (migrate resets it to None).
        if let Some(app) = state.apps.get_mut(window_id) {
            app.geometry = Some(geo);
        }
        state.space.map_element(win, geo.loc, false);
    }
}

fn ipc_prefix_done(state: &mut EmskinState) {
    let Some(saved) = state.focus.prefix_saved_focus.take() else {
        return;
    };
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    tracing::debug!("IPC prefix_done: restoring focus");
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    keyboard.set_focus(state, saved, serial);
}

fn ipc_add_mirror(
    state: &mut EmskinState,
    window_id: u64,
    view_id: u64,
    rect: crate::ipc::IpcRect,
) {
    let crate::ipc::IpcRect { x, y, w, h } = rect;
    // Compositor auto-binds to active workspace.
    let ws_id = state.active_workspace_id;
    tracing::debug!(
        "IPC add_mirror window={window_id} view={view_id} ({x},{y},{w},{h}) ws={ws_id}"
    );
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC add_mirror: invalid size ({w}x{h}), ignoring");
        return;
    }
    let origin_y = state.emacs_geometry().map_or(0, |g| g.loc.y);
    let geo = smithay::utils::Rectangle::new((x, y + origin_y).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        tracing::warn!("add_mirror: unknown window_id={window_id}");
        return;
    };
    app.mirrors.insert(
        view_id,
        crate::apps::MirrorView {
            geometry: geo,
            workspace_id: ws_id,
        },
    );
}

fn ipc_update_mirror_geometry(
    state: &mut EmskinState,
    window_id: u64,
    view_id: u64,
    rect: crate::ipc::IpcRect,
) {
    let crate::ipc::IpcRect { x, y, w, h } = rect;
    tracing::debug!(
        "IPC update_mirror_geometry window={window_id} view={view_id} ({x},{y},{w},{h})"
    );
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC update_mirror_geometry: invalid size ({w}x{h}), ignoring");
        return;
    }
    let origin_y = state.emacs_geometry().map_or(0, |g| g.loc.y);
    let geo = smithay::utils::Rectangle::new((x, y + origin_y).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    if let Some(mirror) = app.mirrors.get_mut(&view_id) {
        mirror.geometry = geo;
    }
}

fn ipc_remove_mirror(state: &mut EmskinState, window_id: u64, view_id: u64) {
    tracing::debug!("IPC remove_mirror window={window_id} view={view_id}");
    if let Some(app) = state.apps.get_mut(window_id) {
        app.mirrors.remove(&view_id);
    }
}

fn ipc_set_focus(state: &mut EmskinState, window_id: Option<u64>) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let target = match window_id {
        Some(id) => state
            .apps
            .get(id)
            .and_then(|app| app.wl_surface())
            .or_else(|| state.emacs_surface.clone()),
        None => state.emacs_surface.clone(),
    };
    tracing::debug!("IPC set_focus window_id={window_id:?}");
    state.focus.prefix_saved_focus = None;
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    keyboard.set_focus(state, target, serial);
}

fn ipc_promote_mirror(state: &mut EmskinState, window_id: u64, view_id: u64) {
    tracing::debug!("IPC promote_mirror window={window_id} view={view_id}");
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    // The promoted mirror becomes the source — its geometry becomes
    // the app's source geometry. If the mirror is in a different workspace,
    // migrate the app to that workspace.
    if let Some(mirror) = app.mirrors.remove(&view_id) {
        let old_ws = app.workspace_id;
        let new_ws = mirror.workspace_id;
        app.geometry = Some(mirror.geometry);
        let window = app.window.clone();

        if old_ws != new_ws {
            // Cross-workspace migration: unmap from old, map in new.
            app.workspace_id = new_ws;
            if let Some(space) = state.space_for_workspace_mut(old_ws) {
                space.unmap_elem(&window);
            }
            // Re-borrow for the target workspace.
            let app_geo = state.apps.get(window_id).and_then(|a| a.geometry);
            if let Some(geo) = app_geo {
                if let Some(space) = state.space_for_workspace_mut(new_ws) {
                    space.map_element(window, geo.loc, false);
                }
            }
        } else {
            state.space.map_element(window, mirror.geometry.loc, false);
        }
    }
}
