//! Event loop tick — the per-frame idle callback for the compositor.

use smithay::reexports::wayland_server::Resource;
use smithay::utils::IsAlive;
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

    // --- Embedded toplevels: classify dialog vs app, then route ---
    // Same one-tick defer rationale (parent + min/max are cleared up only
    // after the client's initial dispatch burst completes).
    process_pending_app_toplevels(state);

    // --- Workspace: process ext-workspace-v1 client actions ---
    process_workspace_actions(state);

    // --- Workspace: detect dead Emacs frames ---
    detect_dead_workspaces(state);

    // --- Workspace: refresh ext-workspace-v1 protocol + bar ---
    refresh_workspace_state(state);

    // --- Clean up destroyed embedded app windows ---
    cleanup_dead_apps(state);
    // --- Clean up destroyed floating dialogs ---
    cleanup_dead_dialogs(state);

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

/// Drain `pending_app_toplevels`, classify each as either a floating
/// dialog (login boxes, file pickers, …) or an embedded app, and route
/// it accordingly. Same one-tick-defer rationale as
/// `process_pending_toplevels`: by now the client's
/// `set_parent` / `set_min_size` / `set_max_size` from the same dispatch
/// burst have been processed, so `wants_floating` returns a stable
/// answer.
fn process_pending_app_toplevels(state: &mut EmskinState) {
    let pending = std::mem::take(&mut state.workspace.pending_app_toplevels);
    if pending.is_empty() {
        return;
    }
    state.needs_redraw = true;
    for (surface, window) in pending {
        if !window.alive() {
            continue;
        }
        let floating = crate::handlers::xdg_shell::wants_floating(state, &surface);
        // Log the full set of inputs the heuristic actually sees, so a
        // misclassified login window (wechat / feishu / file pickers)
        // is debuggable from the journal without rebuilding emskin.
        let log = inspect_toplevel_for_log(&surface);
        tracing::info!(
            target: "emskin::dialog",
            floating,
            has_parent = surface.parent().is_some(),
            title = ?log.title,
            app_id = ?log.app_id,
            min_w = log.min.w,
            min_h = log.min.h,
            max_w = log.max.w,
            max_h = log.max.h,
            "embedded toplevel classification",
        );
        if floating {
            promote_floating_dialog(state, surface, window);
        } else {
            register_embedded_app(state, surface, window);
        }
    }
}

/// Inputs to `wants_floating`, captured for diagnostic logging.
struct ToplevelLogInputs {
    title: Option<String>,
    app_id: Option<String>,
    min: smithay::utils::Size<i32, smithay::utils::Logical>,
    max: smithay::utils::Size<i32, smithay::utils::Logical>,
}

/// Read the inputs that `wants_floating` consults, for diagnostic
/// logging. Pure side-effect-free read of the toplevel's cached state.
fn inspect_toplevel_for_log(
    surface: &smithay::wayland::shell::xdg::ToplevelSurface,
) -> ToplevelLogInputs {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::shell::xdg::{SurfaceCachedState, XdgToplevelSurfaceData};

    let (title, app_id) = with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .map(|d| (d.title.clone(), d.app_id.clone()))
            .unwrap_or((None, None))
    });
    let (min, max) = with_states(surface.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let current = cached.current();
        (current.min_size, current.max_size)
    });
    ToplevelLogInputs {
        title,
        app_id,
        min,
        max,
    }
}

/// Marker stored in a floating dialog's `Window::user_data`. The
/// compositor commit handler looks for this tag and re-centers the
/// window on every commit until the client's natural size stops
/// changing — the tag itself stays for the dialog's lifetime so a
/// configure-driven resize (e.g. content swap inside a wechat login →
/// captcha → 2FA flow) still pulls the window back to center.
#[derive(Clone, Copy, Default)]
pub struct FloatingDialogTag;

/// Marker on `Window::user_data` indicating an embedded toplevel that
/// hasn't been classified yet (still in `pending_app_toplevels`).
/// `handle_surface_commit` checks for this tag and **skips** the
/// initial xdg_toplevel configure — otherwise the configure goes out
/// with whatever pending size we left in `new_toplevel` (now `(0, 0)`,
/// which xwayland-satellite faithfully forwards to the X client; some
/// X clients react to seeing a (0, 0)-derived natural-size frame
/// followed by a Tiled configure by terminating, e.g. Feishu).
/// Removed in `process_pending_app_toplevels` once we know whether to
/// route to dialog (configure 0, 0) or app (configure 1, 1).
#[derive(Clone, Copy, Default)]
pub struct PendingClassificationTag;

/// Map a toplevel as a centered, AppManager-less dialog. The client's
/// natural size drives geometry — we configure 0×0 so the toplevel
/// commits its preferred size. Re-centering happens on every commit
/// in `handlers::compositor::commit` via the `FloatingDialogTag`
/// marker, because at this point the buffer hasn't been attached yet
/// and `window.geometry().size` is still (0, 0) / (1, 1).
fn promote_floating_dialog(
    state: &mut EmskinState,
    surface: smithay::wayland::shell::xdg::ToplevelSurface,
    window: smithay::desktop::Window,
) {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

    let title = with_states(surface.wl_surface(), |s| {
        s.data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .and_then(|d| d.title.clone())
    });
    tracing::info!("embedded toplevel routed to floating dialog: title={title:?}");

    // Tag the window so compositor::commit re-centers it on every
    // buffer commit (idempotent — `map_element` just updates location).
    window
        .user_data()
        .insert_if_missing(FloatingDialogTag::default);

    // Let the client pick its own size (don't pin to 1×1 anymore).
    // Set `Activated` so the client renders an active titlebar / border —
    // keyboard.set_focus alone only delivers wl_keyboard.enter; the
    // visual "focused" state needs the protocol bit too. (xdg_toplevel
    // protocol §4.2: "Activated: client should draw its active state".)
    surface.with_pending_state(|s| {
        s.size = Some((0, 0).into());
        s.states
            .set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated);
    });
    surface.send_pending_configure();

    // Grant keyboard focus unless a prefix chord is in flight.
    let prefix_active = state.focus.is_active(crate::state::FocusOverride::Prefix);
    tracing::info!(prefix_active, "floating dialog: granting keyboard focus");
    if !prefix_active {
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = state.seat.get_keyboard() {
            keyboard.set_focus(state, Some(window.into()), serial);
        }
    }
}

/// Center a window inside the active output. Mirrors
/// sway/tree/container.c:1219 `container_floating_move_to_center`:
///   new_lx = output.x + (output.w - win.w) / 2
///   new_ly = output.y + (output.h - win.h) / 2
pub(crate) fn re_center_dialog(state: &mut EmskinState, window: &smithay::desktop::Window) {
    let Some(output) = state.workspace.active_space.outputs().next().cloned() else {
        return;
    };
    let Some(output_geo) = state.workspace.active_space.output_geometry(&output) else {
        return;
    };
    let win_size = window.geometry().size;
    let new_x = output_geo.loc.x + (output_geo.size.w - win_size.w) / 2;
    let new_y = output_geo.loc.y + (output_geo.size.h - win_size.h) / 2;
    state
        .workspace
        .active_space
        .map_element(window.clone(), (new_x, new_y), false);
}

/// Register an embedded toplevel with `AppManager`, send `WindowCreated`
/// IPC, and apply the auto-focus policy. This is the original
/// synchronous path from `xdg_shell::new_toplevel`, now run from the
/// drain pass.
fn register_embedded_app(
    state: &mut EmskinState,
    surface: smithay::wayland::shell::xdg::ToplevelSurface,
    window: smithay::desktop::Window,
) {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;

    let window_id = state.apps.alloc_id();
    let title = with_states(surface.wl_surface(), |s| {
        s.data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .and_then(|d| d.title.clone())
    })
    .unwrap_or_default();

    tracing::info!("embedded app toplevel connected: window_id={window_id} title={title:?}");

    // Send the initial configure with `(1, 1)` so the client can ack
    // and proceed; the real target size arrives later via
    // `ipc_set_geometry` once elisp processes `WindowCreated`. Without
    // this, satellite has no configure to forward to its X client and
    // the app stays stuck in pre-map state.
    surface.with_pending_state(|s| {
        s.size = Some((1, 1).into());
    });
    surface.send_pending_configure();

    state.apps.insert(crate::apps::AppWindow {
        window_id,
        window: window.clone(),
        workspace_id: state.workspace.active_id,
        geometry: None,
        pending_geometry: None,
        pending_since: None,
        visible: false,
        mirrors: std::collections::HashMap::new(),
    });

    state
        .ipc
        .send(OutgoingMessage::WindowCreated { window_id, title });

    state.auto_focus_new_window(window, window_id);
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

/// Reap dead floating-dialog Windows. Dialogs aren't tracked in
/// `AppManager`, so `cleanup_dead_apps` doesn't see them — we walk
/// every Space and unmap any element whose underlying surface has
/// died. Runs *after* `cleanup_dead_apps` so any AppWindow's space
/// element has already been removed by that pass; everything left
/// dead in a Space is therefore a dialog (Emacs' Window stays alive
/// for the compositor's lifetime).
fn cleanup_dead_dialogs(state: &mut EmskinState) {
    let mut had_dead = false;

    let dead: Vec<smithay::desktop::Window> = state
        .workspace
        .active_space
        .elements()
        .filter(|w| !w.alive())
        .cloned()
        .collect();
    for window in &dead {
        state.workspace.active_space.unmap_elem(window);
    }
    had_dead |= !dead.is_empty();

    for ws in state.workspace.inactive.values_mut() {
        let dead: Vec<smithay::desktop::Window> = ws
            .space
            .elements()
            .filter(|w| !w.alive())
            .cloned()
            .collect();
        for window in &dead {
            ws.space.unmap_elem(window);
        }
        had_dead |= !dead.is_empty();
    }

    if !had_dead {
        return;
    }
    state.needs_redraw = true;

    // Focus may still point at the now-dead dialog Window; fall back to
    // Emacs in that case (mirrors the same fallback in cleanup_dead_apps).
    if let Some(keyboard) = state.seat.get_keyboard() {
        let needs_fallback = match keyboard.current_focus() {
            None => true,
            Some(crate::state::KeyboardFocusTarget::Window(ref w)) => !w.alive(),
            _ => false,
        };
        if needs_fallback {
            let target = state.emacs_focus_target();
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(state, target, serial);
            tracing::debug!("focus returned to Emacs after dialog destroy");
        }
    }
}
