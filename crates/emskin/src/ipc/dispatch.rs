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
        IncomingMessage::PrefixClear => {
            // IME-only cleanup, no focus restoration.
            state.ime.set_prefix_active(false);
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
            state.effects.measure.borrow_mut().set_enabled(enabled);
        }
        IncomingMessage::SetCursorTrail { enabled } => {
            tracing::debug!("IPC set_cursor_trail enabled={enabled}");
            state.effects.cursor_trail.borrow_mut().set_enabled(enabled);
        }
        IncomingMessage::SetKeyCast { enabled } => {
            tracing::debug!("IPC set_key_cast enabled={enabled}");
            state.effects.key_cast.borrow_mut().set_enabled(enabled);
        }
        IncomingMessage::SetSkeleton { enabled, rects } => {
            tracing::debug!("IPC set_skeleton enabled={enabled} rects={}", rects.len());
            let mut sk = state.effects.skeleton.borrow_mut();
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
        IncomingMessage::SetJellyCursor { enabled } => {
            tracing::debug!("IPC set_jelly_cursor enabled={enabled}");
            state.effects.jelly_cursor.borrow_mut().set_enabled(enabled);
        }
        IncomingMessage::SetCursorRect { rect, color } => {
            ipc_set_cursor_rect(state, rect, color);
        }
        IncomingMessage::TakeScreenshot { path } => {
            tracing::debug!("IPC take_screenshot path={path}");
            state
                .recorder
                .request_screenshot(std::path::PathBuf::from(path));
            // Force a render so the request fires this tick even on an
            // otherwise-idle frame.
            state.needs_redraw = true;
        }
        IncomingMessage::SetRecording { enabled, path, fps } => {
            // The overlay and Emacs-side `emskin-record` both derive
            // from the Recorder's state, not from this dispatch arm:
            //   - overlay is set in `render_frame` via
            //     `Recorder::overlay_started_at`, so if ffmpeg spawn
            //     fails the red dot never lights up
            //   - the elisp toggle resyncs via `RecordingStopped` IPC,
            //     emitted when `finalize` drains `Stopping`
            // Dispatch here only drives the state machine + forces a
            // render tick so the request is picked up this frame.
            if enabled {
                match (path, fps) {
                    (Some(path), Some(fps)) if fps > 0 => {
                        tracing::debug!("IPC set_recording start path={path} fps={fps}");
                        state
                            .recorder
                            .request_recording(std::path::PathBuf::from(path), fps);
                        state.needs_redraw = true;
                    }
                    (path, fps) => {
                        tracing::warn!(
                            "IPC set_recording enabled=true with invalid path={path:?} fps={fps:?}"
                        );
                    }
                }
            } else {
                tracing::debug!("IPC set_recording stop");
                state.recorder.stop_recording();
                state.needs_redraw = true;
            }
        }
    }
}

fn ipc_set_cursor_rect(state: &mut EmskinState, rect: crate::ipc::IpcRect, color: Option<String>) {
    let now = state.start_time.elapsed();
    let canvas_rect = state.emacs_rect_to_canvas(rect);
    let mut jc = state.effects.jelly_cursor.borrow_mut();
    if let Some(hex) = color.as_deref() {
        jc.set_color_hex(hex);
    }
    // Zero-size rect cancels animation (e.g. buffer lost focus).
    if rect.w <= 0 || rect.h <= 0 {
        jc.update(None, now);
        return;
    }
    jc.update(Some(canvas_rect), now);
}

fn ipc_set_geometry(state: &mut EmskinState, window_id: u64, rect: crate::ipc::IpcRect) {
    let crate::ipc::IpcRect { x, y, w, h } = rect;
    tracing::debug!("IPC set_geometry window={window_id} ({x},{y},{w},{h})");
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC set_geometry: invalid size ({w}x{h}), ignoring");
        return;
    }
    let new_geo = state.emacs_rect_to_canvas(rect);
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    app.visible = true;

    state.migrate_app_to_active(window_id);

    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };

    let Some(toplevel) = app.window.toplevel() else {
        return;
    };
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
        state
            .workspace
            .active_space
            .map_element(window, new_geo.loc, false);
        tracing::info!(
            "app {window_id} mapped immediately at ({},{}) ws={}",
            new_geo.loc.x,
            new_geo.loc.y,
            state.workspace.active_id
        );
    } else {
        app.pending_geometry = Some(new_geo);
        app.pending_since = Some(std::time::Instant::now());
        tracing::debug!(
            "app {window_id} pending geometry ({},{}) ws={}",
            new_geo.loc.x,
            new_geo.loc.y,
            state.workspace.active_id
        );
    }
}

fn ipc_close(state: &mut EmskinState, window_id: u64) {
    tracing::debug!("IPC close window={window_id}");
    if let Some(app) = state.apps.get_mut(window_id) {
        if let Some(toplevel) = app.window.toplevel() {
            toplevel.send_close();
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
        if let Some(space) = state.workspace.space_for_mut(ws_id) {
            space.unmap_elem(&win);
        }
    } else if let Some(geo) = geo {
        state.migrate_app_to_active(window_id);
        // Write back geometry (migrate resets it to None).
        if let Some(app) = state.apps.get_mut(window_id) {
            app.geometry = Some(geo);
        }
        state
            .workspace
            .active_space
            .map_element(win, geo.loc, false);
    }
}

fn ipc_prefix_done(state: &mut EmskinState) {
    // Always re-enable host IME at chord end, even if no prefix override
    // was active (so Emacs's prefix_done IPC remains functional after a
    // dropped intermediate IPC).
    state.ime.set_prefix_active(false);
    let Some(saved) = state.focus.exit(crate::state::FocusOverride::Prefix) else {
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
    let ws_id = state.workspace.active_id;
    tracing::debug!(
        "IPC add_mirror window={window_id} view={view_id} ({x},{y},{w},{h}) ws={ws_id}"
    );
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC add_mirror: invalid size ({w}x{h}), ignoring");
        return;
    }
    let geo = state.emacs_rect_to_canvas(rect);
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
    let geo = state.emacs_rect_to_canvas(rect);
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
            .map(|app| crate::KeyboardFocusTarget::from(app.window.clone()))
            .or_else(|| state.emacs_focus_target()),
        None => state.emacs_focus_target(),
    };
    tracing::debug!("IPC set_focus window_id={window_id:?}");
    state.focus.exit(crate::state::FocusOverride::Prefix);
    state.ime.set_prefix_active(false);
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
            if let Some(space) = state.workspace.space_for_mut(old_ws) {
                space.unmap_elem(&window);
            }
            // Re-borrow for the target workspace.
            let app_geo = state.apps.get(window_id).and_then(|a| a.geometry);
            if let Some(geo) = app_geo {
                if let Some(space) = state.workspace.space_for_mut(new_ws) {
                    space.map_element(window, geo.loc, false);
                }
            }
        } else {
            state
                .workspace
                .active_space
                .map_element(window, mirror.geometry.loc, false);
        }
    }
}
