use clap::Parser;
use include_dir::{include_dir, Dir};
use smithay::reexports::wayland_server::{Display, Resource};

use emskin::{clipboard, clipboard_x11, cursor_x11, ipc, state, EmskinState};

static ELISP_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../elisp");
static DEMO_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../demo");

/// Nested Wayland compositor for Emacs Application Framework.
#[derive(Parser, Debug)]
#[command(name = "emskin")]
struct Cli {
    /// Do not spawn a child process; wait for an external connection.
    #[arg(long)]
    no_spawn: bool,

    /// Program to launch (default: "emacs").
    #[arg(long, default_value = "emacs")]
    command: String,

    /// Arguments for --command.
    #[arg(long = "arg", num_args = 1)]
    command_args: Vec<String>,

    /// Explicit IPC socket path (default: $XDG_RUNTIME_DIR/emskin-<pid>.ipc).
    #[arg(long)]
    ipc_path: Option<std::path::PathBuf>,

    /// XKB keyboard layout (e.g. "us", "de", "cn").
    #[arg(long, default_value = "")]
    xkb_layout: String,

    /// XKB keyboard model (e.g. "pc105").
    #[arg(long, default_value = "")]
    xkb_model: String,

    /// XKB layout variant (e.g. "nodeadkeys").
    #[arg(long, default_value = "")]
    xkb_variant: String,

    /// XKB options (e.g. "ctrl:nocaps").
    #[arg(long)]
    xkb_options: Option<String>,

    /// Standalone mode: auto-load built-in elisp without user config.
    #[arg(long)]
    standalone: bool,

    /// Workspace bar mode: "builtin" (default) or "none".
    #[arg(long, default_value = "builtin")]
    bar: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    let cli = Cli::parse();

    let mut event_loop: smithay::reexports::calloop::EventLoop<'static, EmskinState> =
        smithay::reexports::calloop::EventLoop::try_new()?;

    let display: Display<EmskinState> = Display::new()?;

    let ipc_path = cli.ipc_path.clone().unwrap_or_else(default_ipc_path);
    tracing::info!("IPC socket path: {}", ipc_path.display());

    // xkbcommon treats "" as invalid (not "use default"), so when variant is
    // set but layout is empty we must supply a base layout explicitly.
    let xkb_layout = if cli.xkb_layout.is_empty() && !cli.xkb_variant.is_empty() {
        "us".to_string()
    } else {
        cli.xkb_layout.clone()
    };
    let xkb_config = smithay::input::keyboard::XkbConfig {
        layout: &xkb_layout,
        model: &cli.xkb_model,
        variant: &cli.xkb_variant,
        options: cli.xkb_options.clone(),
        ..Default::default()
    };

    let ipc = emskin::ipc::IpcServer::bind(ipc_path)?;
    let loop_handle = event_loop.handle();
    let mut state = EmskinState::new(&mut event_loop, loop_handle, display, ipc, xkb_config)?;

    // Initialize clipboard synchronization with host compositor.
    // Try Wayland data_control first; fall back to X11 selection protocol.
    state.clipboard = clipboard::ClipboardProxy::new()
        .map(clipboard::HostClipboard::Wayland)
        .or_else(|| clipboard_x11::X11ClipboardProxy::new().map(clipboard::HostClipboard::X11));
    if let Some(ref clipboard) = state.clipboard {
        register_clipboard_source(&mut event_loop, clipboard)?;
    }

    register_ipc_source(&mut event_loop, &state)?;

    // Open a Wayland/X11 window for our nested compositor
    emskin::winit::init_winit(&mut event_loop, &mut state)?;

    match cli.bar.as_str() {
        "builtin" | "none" => {}
        other => {
            eprintln!("Unknown --bar value '{other}', expected 'builtin' or 'none'");
            std::process::exit(1);
        }
    }
    state.bar_enabled = cli.bar != "none";

    if !cli.no_spawn {
        state.pending_command = Some(state::PendingCommand {
            command: cli.command.clone(),
            args: cli.command_args.clone(),
            standalone: cli.standalone,
        });
    }

    start_xwayland(event_loop.handle(), &mut state);

    event_loop.run(None, &mut state, |state| {
        if let Some(ref mut child) = state.emacs_child {
            if let Ok(Some(status)) = child.try_wait() {
                tracing::info!("Emacs exited with {status}, stopping compositor");
                state.loop_signal.stop();
            }
        }

        // --- Workspace: process deferred Emacs toplevels ---
        // After dispatch_clients, set_parent has been processed for same-batch
        // toplevels, so surface.parent() is now accurate.
        let pending = std::mem::take(&mut state.pending_emacs_toplevels);
        if !pending.is_empty() {
            state.needs_redraw = true;
        }
        for (surface, window) in pending {
            if surface.parent().is_some() {
                // Child frame (posframe, etc.) — leave in current space, GTK manages.
                tracing::info!("Emacs child frame confirmed (has parent), workspace {}",
                    state.active_workspace_id);
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
                    emskin::state::Workspace {
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
                    .send(ipc::OutgoingMessage::WorkspaceCreated { workspace_id: ws_id });

                // Switch immediately.
                state.switch_workspace(ws_id);
                state
                    .ipc
                    .send(ipc::OutgoingMessage::WorkspaceSwitched { workspace_id: ws_id });
            }
        }

        // --- Workspace: process ext-workspace-v1 client actions ---
        let actions = state.workspace_protocol.take_pending_actions();
        if !actions.is_empty() {
            state.needs_redraw = true;
        }
        for action in actions {
            use emskin::protocols::workspace::WorkspaceAction;
            match action {
                WorkspaceAction::Activate(id) => {
                    if state.switch_workspace(id) {
                        state.ipc.send(ipc::OutgoingMessage::WorkspaceSwitched {
                            workspace_id: id,
                        });
                    }
                }
                WorkspaceAction::Remove(id) => {
                    if id != state.active_workspace_id {
                        state.destroy_workspace(id);
                        state.ipc.send(ipc::OutgoingMessage::WorkspaceDestroyed {
                            workspace_id: id,
                        });
                    }
                }
                _ => {} // Deactivate / CreateWorkspace: future extension
            }
        }

        // --- Workspace: detect dead Emacs frames in inactive workspaces ---
        let dead_ws: Vec<u64> = state.inactive_workspaces.iter()
            .filter(|(_, ws)| {
                ws.emacs_surface.as_ref().is_none_or(|s| !s.is_alive())
            })
            .map(|(id, _)| *id)
            .collect();
        let had_dead = !dead_ws.is_empty();
        for ws_id in dead_ws {
            state.destroy_workspace(ws_id);
            state
                .ipc
                .send(ipc::OutgoingMessage::WorkspaceDestroyed { workspace_id: ws_id });
            tracing::info!("workspace {ws_id} destroyed (Emacs frame died)");
        }
        if had_dead {
            // Bar might have disappeared (2→1 workspace) — resize Emacs back to fullscreen.
            resize_all_emacs_for_bar(state);
            state.needs_redraw = true;
        }

        // --- Workspace: detect active Emacs frame death ---
        if state.emacs_surface.as_ref().is_some_and(|s| !s.is_alive())
            && state.initial_size_settled
        {
            if let Some(&fallback_id) = state.inactive_workspaces.keys().next() {
                tracing::info!("active Emacs died, switching to workspace {fallback_id}");
                state.switch_workspace(fallback_id);
                state.ipc.send(ipc::OutgoingMessage::WorkspaceSwitched {
                    workspace_id: fallback_id,
                });
                state.needs_redraw = true;
            } else {
                tracing::info!("last Emacs frame died, stopping");
                state.loop_signal.stop();
            }
        }

        // --- Workspace: refresh ext-workspace-v1 protocol + bar ---
        {
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

            let ws_infos: Vec<emskin::protocols::workspace::WorkspaceInfo> = ws_named
                .iter()
                .map(|&(id, name)| {
                    let display_name = if name.is_empty() {
                        format!("Workspace {id}")
                    } else {
                        name.to_string()
                    };
                    emskin::protocols::workspace::WorkspaceInfo {
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

        // Clean up embedded app windows whose Wayland surface was destroyed.
        // Route unmap to the correct workspace's space.
        let dead = state.apps.drain_dead();
        if !dead.is_empty() {
            state.needs_redraw = true;
            for app in &dead {
                if let Some(space) = state.space_for_workspace_mut(app.workspace_id) {
                    space.unmap_elem(&app.window);
                }
                state.ipc.send(ipc::OutgoingMessage::WindowDestroyed {
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

        // Dispatch incoming IPC messages from Emacs.
        if let Some(msgs) = state.ipc.recv_all() {
            for msg in msgs {
                handle_ipc_message(state, msg);
            }
            state.needs_redraw = true;
        }

        // Process clipboard events from host compositor.
        let clipboard_events = state
            .clipboard
            .as_mut()
            .map(|c| c.take_events())
            .unwrap_or_default();
        let has_clipboard_events = !clipboard_events.is_empty();
        for event in clipboard_events {
            handle_clipboard_event(state, event);
        }
        // Flush immediately so Wayland clients see selection changes / send
        // requests without waiting for the next render frame.
        if has_clipboard_events {
            let _ = state.display_handle.flush_clients();
            state.needs_redraw = true;
        }

        // Force-commit pending geometries that have timed out (100ms).
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
    })?;

    // Clean up Emacs child process
    if let Some(mut child) = state.emacs_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }

    // Clean up extracted elisp files
    if let Some(ref dir) = state.elisp_dir {
        let _ = std::fs::remove_dir_all(dir);
    }

    Ok(())
}

fn register_ipc_source(
    event_loop: &mut smithay::reexports::calloop::EventLoop<EmskinState>,
    state: &EmskinState,
) -> Result<(), Box<dyn std::error::Error>> {
    use smithay::reexports::calloop::{generic::Generic, Interest, Mode, PostAction};
    use std::os::unix::io::FromRawFd;
    let listener_fd = state.ipc.listener_fd();
    // SAFETY: We duplicate the fd so the Generic source owns its own copy.
    // The original fd remains valid inside IpcServer for the lifetime of state.
    let dup_fd = unsafe { libc::dup(listener_fd) };
    if dup_fd < 0 {
        return Err("dup(ipc listener fd) failed".into());
    }
    // SAFETY: dup_fd is a valid open fd (dup succeeded above, dup_fd >= 0).
    // Ownership transfers to File; the original listener_fd stays open in IpcServer.
    let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
    event_loop
        .handle()
        .insert_source(
            Generic::new(file, Interest::READ, Mode::Level),
            |_, _, state| {
                state.ipc.accept();
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("failed to register IPC listener: {e}"))?;
    Ok(())
}

fn spawn_child(
    command: &str,
    args: &[String],
    x_display: u32,
    standalone: bool,
    state: &mut EmskinState,
) {
    let Some(socket_name) = state.socket_name.to_str() else {
        tracing::error!("Wayland socket name is not valid UTF-8, cannot spawn child");
        return;
    };

    let mut full_args: Vec<String> = Vec::new();

    if standalone {
        if let Some(elisp_dir) = extract_embedded(&ELISP_DIR, "elisp") {
            full_args.push("--directory".to_string());
            full_args.push(elisp_dir.to_string_lossy().into_owned());
            full_args.push("-l".to_string());
            full_args.push("emskin".to_string());
            state.elisp_dir = Some(elisp_dir);
        }
        // `emskin-demo-dir` defaults to `../demo` relative to the loaded
        // emskin.el, so demo scripts must sit alongside the extracted
        // elisp dir: $XDG_RUNTIME_DIR/emskin-<pid>/{elisp,demo}/.
        extract_embedded(&DEMO_DIR, "demo");
    }

    full_args.extend_from_slice(args);

    tracing::info!(
        "Spawning: {command} {full_args:?} (WAYLAND_DISPLAY={socket_name} DISPLAY=:{x_display})"
    );
    match std::process::Command::new(command)
        .args(&full_args)
        .env("WAYLAND_DISPLAY", socket_name)
        .env("DISPLAY", format!(":{x_display}"))
        // Ensure child apps prefer Wayland even when host is X11.
        .env("XDG_SESSION_TYPE", "wayland")
        .env("GDK_BACKEND", "wayland,x11")
        .env("QT_QPA_PLATFORM", "wayland;xcb")
        .env("SDL_VIDEODRIVER", "wayland")
        .env("CLUTTER_BACKEND", "wayland")
        .env("XDG_SESSION_DESKTOP", "emskin")
        .spawn()
    {
        Ok(child) => state.emacs_child = Some(child),
        Err(e) => tracing::error!("Failed to spawn '{command}': {e}"),
    }
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string())
}

/// Extract an embedded `include_dir` tree to
/// `$XDG_RUNTIME_DIR/emskin-<pid>/<subdir>/`.
fn extract_embedded(src: &Dir<'_>, subdir: &str) -> Option<std::path::PathBuf> {
    let dest = std::path::PathBuf::from(format!(
        "{}/emskin-{}/{subdir}",
        runtime_dir(),
        std::process::id(),
    ));
    if let Err(e) = std::fs::create_dir_all(&dest) {
        tracing::error!("Failed to create {subdir} dir {}: {e}", dest.display());
        return None;
    }
    for file in src.files() {
        let out = dest.join(file.path());
        if let Err(e) = std::fs::write(&out, file.contents()) {
            tracing::error!("Failed to write {}: {e}", out.display());
            return None;
        }
    }
    tracing::info!("Extracted embedded {subdir} to {}", dest.display());
    Some(dest)
}

fn default_ipc_path() -> std::path::PathBuf {
    let pid = std::process::id();
    std::path::PathBuf::from(format!("{}/emskin-{pid}.ipc", runtime_dir()))
}

/// Resize and reposition all Emacs frames to account for bar height changes.
/// Called when workspace count transitions (1→2 or 2→1).
fn resize_all_emacs_for_bar(state: &mut EmskinState) {
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

    state.ipc.send(ipc::OutgoingMessage::SurfaceSize {
        width: geo.size.w,
        height: geo.size.h,
    });
}

fn handle_ipc_message(state: &mut EmskinState, msg: ipc::IncomingMessage) {
    use ipc::IncomingMessage;
    match msg {
        IncomingMessage::SetGeometry {
            window_id,
            x,
            y,
            w,
            h,
        } => {
            ipc_set_geometry(state, window_id, x, y, w, h);
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
            x,
            y,
            w,
            h,
        } => {
            ipc_add_mirror(state, window_id, view_id, x, y, w, h);
        }
        IncomingMessage::UpdateMirrorGeometry {
            window_id,
            view_id,
            x,
            y,
            w,
            h,
        } => {
            ipc_update_mirror_geometry(state, window_id, view_id, x, y, w, h);
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
        IncomingMessage::SetCrosshair { enabled } => {
            tracing::debug!("IPC set_crosshair enabled={enabled}");
            state.crosshair.enabled = enabled;
        }
        IncomingMessage::SetSkeleton { enabled, rects } => {
            tracing::debug!("IPC set_skeleton enabled={enabled} rects={}", rects.len());
            state.skeleton.enabled = enabled;
            if enabled {
                state.skeleton.set_rects(rects);
            } else {
                state.skeleton.clear();
            }
        }
        IncomingMessage::SwitchWorkspace { workspace_id } => {
            tracing::debug!("IPC switch_workspace {workspace_id}");
            if state.switch_workspace(workspace_id) {
                state
                    .ipc
                    .send(ipc::OutgoingMessage::WorkspaceSwitched { workspace_id });
            }
        }
    }
}

fn ipc_set_geometry(state: &mut EmskinState, window_id: u64, x: i32, y: i32, w: i32, h: i32) {
    tracing::debug!("IPC set_geometry window={window_id} ({x},{y},{w},{h})");
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC set_geometry: invalid size ({w}x{h}), ignoring");
        return;
    }
    // IPC coordinates are relative to Emacs surface; offset by bar height.
    let bar_h = state.bar_height();
    let ay = y + bar_h;
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
    let Some(saved) = state.prefix_saved_focus.take() else {
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
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    // Compositor auto-binds to active workspace — elisp doesn't need to know.
    let ws_id = state.active_workspace_id;
    tracing::debug!(
        "IPC add_mirror window={window_id} view={view_id} ({x},{y},{w},{h}) ws={ws_id}"
    );
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC add_mirror: invalid size ({w}x{h}), ignoring");
        return;
    }
    let bar_h = state.bar_height();
    let geo = smithay::utils::Rectangle::new((x, y + bar_h).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        tracing::warn!("add_mirror: unknown window_id={window_id}");
        return;
    };
    app.mirrors.insert(
        view_id,
        emskin::apps::MirrorView {
            geometry: geo,
            workspace_id: ws_id,
        },
    );
}

fn ipc_update_mirror_geometry(
    state: &mut EmskinState,
    window_id: u64,
    view_id: u64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    tracing::debug!(
        "IPC update_mirror_geometry window={window_id} view={view_id} ({x},{y},{w},{h})"
    );
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC update_mirror_geometry: invalid size ({w}x{h}), ignoring");
        return;
    }
    let bar_h = state.bar_height();
    let geo = smithay::utils::Rectangle::new((x, y + bar_h).into(), (w, h).into());
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
    state.prefix_saved_focus = None;
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

fn start_xwayland(
    handle: smithay::reexports::calloop::LoopHandle<'static, EmskinState>,
    state: &mut EmskinState,
) {
    use smithay::xwayland::{XWayland, XWaylandEvent};

    let dh = state.display_handle.clone();

    let (xwayland, client) = match XWayland::spawn(
        &dh,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| (),
    ) {
        Ok(res) => res,
        Err(e) => {
            tracing::error!("Failed to start XWayland: {e}");
            return;
        }
    };

    let inner_handle = handle.clone();
    if let Err(e) = handle.insert_source(xwayland, move |event, _, state| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            let wm = smithay::xwayland::X11Wm::start_wm(
                inner_handle.clone(),
                &dh,
                x11_socket,
                client.clone(),
            );
            match wm {
                Ok(wm) => {
                    state.xwm = Some(wm);
                    state.xdisplay = Some(display_number);
                    std::env::set_var("DISPLAY", format!(":{display_number}"));
                    state.ipc.send(ipc::OutgoingMessage::XWaylandReady {
                        display: display_number,
                    });
                    tracing::info!("XWayland ready on :{display_number}");

                    // Replay cached host selections so X11 clients can paste
                    // content that was set before XWM was ready.
                    {
                        use smithay::wayland::selection::SelectionTarget;
                        let pairs = [
                            (SelectionTarget::Clipboard, &state.host_clipboard_mimes),
                            (SelectionTarget::Primary, &state.host_primary_mimes),
                        ];
                        for (target, mimes) in pairs {
                            if !mimes.is_empty() {
                                if let Some(ref mut xwm) = state.xwm {
                                    if let Err(e) = xwm.new_selection(target, Some(mimes.clone())) {
                                        tracing::warn!("X11 replay {target:?} failed: {e}");
                                    }
                                }
                            }
                        }
                    }

                    // Initialize X11 cursor tracker for XWayland cursor forwarding.
                    if let Some(tracker) = cursor_x11::X11CursorTracker::new(display_number) {
                        state.x11_cursor_tracker = Some(tracker);
                    }

                    // Spawn child now that both WAYLAND_DISPLAY and DISPLAY are set.
                    if let Some(pc) = state.pending_command.take() {
                        spawn_child(&pc.command, &pc.args, display_number, pc.standalone, state);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to start X11 WM: {e}");
                }
            }
        }
        XWaylandEvent::Error => {
            tracing::warn!("XWayland crashed on startup");
        }
    }) {
        tracing::error!("Failed to insert XWayland source: {e}");
    }
}

fn register_clipboard_source(
    event_loop: &mut smithay::reexports::calloop::EventLoop<EmskinState>,
    clipboard: &clipboard::HostClipboard,
) -> Result<(), Box<dyn std::error::Error>> {
    use smithay::reexports::calloop::{generic::Generic, Interest, Mode, PostAction};
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let raw_fd = clipboard.connection_fd().as_raw_fd();
    // SAFETY: dup() returns a valid fd that we transfer ownership to File.
    let dup_fd = unsafe { libc::dup(raw_fd) };
    if dup_fd < 0 {
        return Err("dup(clipboard connection fd) failed".into());
    }
    let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };

    event_loop
        .handle()
        .insert_source(
            Generic::new(file, Interest::READ, Mode::Level),
            |_, _, state| {
                if let Some(ref mut clipboard) = state.clipboard {
                    clipboard.dispatch();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("failed to register clipboard source: {e}"))?;
    Ok(())
}

fn handle_clipboard_event(state: &mut EmskinState, event: clipboard::ClipboardEvent) {
    use clipboard::ClipboardEvent;

    match event {
        ClipboardEvent::HostSelectionChanged { target, mime_types } => {
            inject_host_selection(state, target, mime_types);
        }
        ClipboardEvent::HostSendRequest {
            id,
            target,
            mime_type,
            write_fd,
            read_fd,
        } => {
            forward_client_selection(state, target, mime_type, write_fd);
            // Flush immediately so the write_fd reaches the Wayland client
            // before our OwnedFd copy is dropped (closing the write end).
            let _ = state.display_handle.flush_clients();
            if let Some(read_fd) = read_fd {
                if !register_outgoing_pipe(state, id, read_fd) {
                    // Calloop registration failed — clean up and notify X11 requestor.
                    if let Some(ref mut cb) = state.clipboard {
                        cb.complete_outgoing(id, Vec::new());
                    }
                }
            }
        }
        ClipboardEvent::SourceCancelled { target } => {
            tracing::debug!("Host source cancelled ({target:?})");
            match target {
                smithay::wayland::selection::SelectionTarget::Clipboard => {
                    state.clipboard_origin = emskin::state::SelectionOrigin::default();
                }
                smithay::wayland::selection::SelectionTarget::Primary => {
                    state.primary_origin = emskin::state::SelectionOrigin::default();
                }
            }
        }
    }
}

fn inject_host_selection(
    state: &mut EmskinState,
    target: smithay::wayland::selection::SelectionTarget,
    mime_types: Vec<String>,
) {
    use smithay::wayland::selection::data_device::{
        clear_data_device_selection, set_data_device_selection,
    };
    use smithay::wayland::selection::primary_selection::{
        clear_primary_selection, set_primary_selection,
    };
    use smithay::wayland::selection::SelectionTarget;

    // Cache host mime types for replay when XWM becomes ready.
    match target {
        SelectionTarget::Clipboard => state.host_clipboard_mimes = mime_types.clone(),
        SelectionTarget::Primary => state.host_primary_mimes = mime_types.clone(),
    }

    if mime_types.is_empty() {
        tracing::debug!("Host {target:?} cleared");
        match target {
            SelectionTarget::Clipboard => {
                clear_data_device_selection(&state.display_handle, &state.seat)
            }
            SelectionTarget::Primary => clear_primary_selection(&state.display_handle, &state.seat),
        }
        if let Some(ref mut xwm) = state.xwm {
            if let Err(e) = xwm.new_selection(target, None) {
                tracing::warn!("X11 clear {target:?} selection failed: {e}");
            }
        }
    } else {
        tracing::debug!("Host {target:?} changed ({} types)", mime_types.len());
        if let Some(ref mut xwm) = state.xwm {
            if let Err(e) = xwm.new_selection(target, Some(mime_types.clone())) {
                tracing::warn!("X11 set {target:?} selection failed: {e}");
            }
        }
        match target {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&state.display_handle, &state.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&state.display_handle, &state.seat, mime_types, ())
            }
        }
    }
}

/// Register a pipe read_fd with calloop for event-driven reading.
/// Returns `false` if registration fails (caller should clean up).
fn register_outgoing_pipe(state: &mut EmskinState, id: u64, read_fd: std::os::fd::OwnedFd) -> bool {
    use smithay::reexports::calloop::{generic::Generic, Interest, Mode, PostAction};
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

    // SAFETY: into_raw_fd() relinquishes ownership; File takes it over.
    let file = unsafe { std::fs::File::from_raw_fd(read_fd.into_raw_fd()) };
    let mut buf_data: Vec<u8> = Vec::new();

    if let Err(e) = state.loop_handle.insert_source(
        Generic::new(file, Interest::READ, Mode::Level),
        move |_, file, state| {
            let mut buf = [0u8; 65536];
            loop {
                // SAFETY: buf is valid for buf.len() bytes; fd is open and non-blocking.
                let n = unsafe { libc::read(file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    buf_data.extend_from_slice(&buf[..n as usize]);
                } else if n == 0 {
                    let data = std::mem::take(&mut buf_data);
                    if let Some(ref mut clipboard) = state.clipboard {
                        clipboard.complete_outgoing(id, data);
                    }
                    return Ok(PostAction::Remove);
                } else {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(PostAction::Continue);
                    }
                    tracing::warn!("outgoing pipe read error: {err}");
                    return Ok(PostAction::Remove);
                }
            }
        },
    ) {
        tracing::warn!("Failed to register outgoing pipe: {e}");
        return false;
    }
    true
}

fn forward_client_selection(
    state: &mut EmskinState,
    target: smithay::wayland::selection::SelectionTarget,
    mime_type: String,
    fd: std::os::fd::OwnedFd,
) {
    use smithay::wayland::selection::data_device::request_data_device_client_selection;
    use smithay::wayland::selection::primary_selection::request_primary_client_selection;
    use smithay::wayland::selection::SelectionTarget;

    use emskin::state::SelectionOrigin;

    let origin = match target {
        SelectionTarget::Clipboard => state.clipboard_origin,
        SelectionTarget::Primary => state.primary_origin,
    };

    match origin {
        SelectionOrigin::Wayland => {
            let result = match target {
                SelectionTarget::Clipboard => {
                    request_data_device_client_selection(&state.seat, mime_type, fd)
                        .map_err(|e| format!("{e:?}"))
                }
                SelectionTarget::Primary => {
                    request_primary_client_selection(&state.seat, mime_type, fd)
                        .map_err(|e| format!("{e:?}"))
                }
            };
            if let Err(e) = result {
                tracing::warn!("Failed to forward {target:?} selection to host: {e}");
            }
        }
        SelectionOrigin::X11 => {
            if let Some(ref mut xwm) = state.xwm {
                if let Err(e) = xwm.send_selection(target, mime_type, fd) {
                    tracing::warn!("Failed to forward X11 {target:?} selection to host: {e}");
                }
            } else {
                tracing::warn!("X11 {target:?} selection requested but XWM unavailable");
            }
        }
    }
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
