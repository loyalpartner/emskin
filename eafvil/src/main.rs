pub mod apps;
mod clipboard;
mod handlers;
mod input;
pub mod ipc;
mod state;
mod winit;

use clap::Parser;
use smithay::reexports::wayland_server::Display;
pub use state::EafvilState;

/// Nested Wayland compositor for Emacs Application Framework.
#[derive(Parser, Debug)]
#[command(name = "eafvil")]
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

    /// Explicit IPC socket path (default: $XDG_RUNTIME_DIR/eafvil-<pid>.ipc).
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
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    let cli = Cli::parse();

    let mut event_loop: smithay::reexports::calloop::EventLoop<'static, EafvilState> =
        smithay::reexports::calloop::EventLoop::try_new()?;

    let display: Display<EafvilState> = Display::new()?;

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

    let ipc = crate::ipc::IpcServer::bind(ipc_path)?;
    let mut state = EafvilState::new(&mut event_loop, display, ipc, xkb_config)?;

    // Initialize clipboard synchronization with host compositor
    state.clipboard = clipboard::ClipboardProxy::new();
    if let Some(ref clipboard) = state.clipboard {
        register_clipboard_source(&mut event_loop, clipboard)?;
    }

    register_ipc_source(&mut event_loop, &state)?;

    // Open a Wayland/X11 window for our nested compositor
    crate::winit::init_winit(&mut event_loop, &mut state)?;

    if !cli.no_spawn {
        state.pending_command = Some((cli.command.clone(), cli.command_args.clone()));
    }

    start_xwayland(event_loop.handle(), &mut state);

    event_loop.run(None, &mut state, |state| {
        if let Some(ref mut child) = state.emacs_child {
            if let Ok(Some(status)) = child.try_wait() {
                tracing::info!("Emacs exited with {status}, stopping compositor");
                state.loop_signal.stop();
            }
        }

        // Clean up EAF app windows whose Wayland surface was destroyed.
        for app in state.apps.drain_dead() {
            state.space.unmap_elem(&app.window);
            state.ipc.send(ipc::OutgoingMessage::WindowDestroyed {
                window_id: app.window_id,
            });
            tracing::info!("EAF app window_id={} destroyed", app.window_id);
        }

        // Dispatch incoming IPC messages from Emacs.
        if let Some(msgs) = state.ipc.recv_all() {
            for msg in msgs {
                handle_ipc_message(state, msg);
            }
        }

        // Process clipboard events from host compositor.
        let clipboard_events = state
            .clipboard
            .as_mut()
            .map(|c| c.take_events())
            .unwrap_or_default();
        for event in clipboard_events {
            handle_clipboard_event(state, event);
        }

        // Evict activation tokens older than 30s to prevent unbounded growth.
        state
            .xdg_activation_state
            .retain_tokens(|_, data| data.timestamp.elapsed().as_secs() < 30);

        // Force-commit pending geometries that have timed out (100ms).
        for (window_id, window, geo) in state
            .apps
            .collect_timed_out(std::time::Duration::from_millis(100))
        {
            state.space.map_element(window, geo.loc, false);
            tracing::debug!("EAF app window_id={window_id} geometry force-committed (timeout)");
        }
    })?;

    // Clean up Emacs child process
    if let Some(mut child) = state.emacs_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }

    Ok(())
}

fn register_ipc_source(
    event_loop: &mut smithay::reexports::calloop::EventLoop<EafvilState>,
    state: &EafvilState,
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

fn spawn_child(command: &str, args: &[String], x_display: u32, state: &mut EafvilState) {
    let Some(socket_name) = state.socket_name.to_str() else {
        tracing::error!("Wayland socket name is not valid UTF-8, cannot spawn child");
        return;
    };
    tracing::info!("Spawning: {command} {args:?} (WAYLAND_DISPLAY={socket_name} DISPLAY=:{x_display})");
    match std::process::Command::new(command)
        .args(args)
        .env("WAYLAND_DISPLAY", socket_name)
        .env("DISPLAY", format!(":{x_display}"))
        .spawn()
    {
        Ok(child) => state.emacs_child = Some(child),
        Err(e) => tracing::error!("Failed to spawn '{command}': {e}"),
    }
}

fn default_ipc_path() -> std::path::PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let pid = std::process::id();
    std::path::PathBuf::from(format!("{runtime_dir}/eafvil-{pid}.ipc"))
}

fn handle_ipc_message(state: &mut EafvilState, msg: ipc::IncomingMessage) {
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
        IncomingMessage::ForwardKey {
            window_id,
            keycode,
            state: key_state,
            modifiers,
        } => {
            ipc_forward_key(state, window_id, keycode, key_state, modifiers);
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
        IncomingMessage::RequestActivationToken => {
            ipc_request_activation_token(state);
        }
    }
}

fn ipc_set_geometry(state: &mut EafvilState, window_id: u64, x: i32, y: i32, w: i32, h: i32) {
    tracing::debug!("IPC set_geometry window={window_id} ({x},{y},{w},{h})");
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC set_geometry: invalid size ({w}x{h}), ignoring");
        return;
    }
    let new_geo = smithay::utils::Rectangle::new((x, y).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    app.visible = true;

    if let Some(toplevel) = app.window.toplevel() {
        // Wayland path — async configure + pending geometry.
        toplevel.with_pending_state(|s| {
            s.size = Some((w, h).into());
        });
        toplevel.send_pending_configure();

        if app.geometry.is_none() {
            app.geometry = Some(new_geo);
            let window = app.window.clone();
            state.space.map_element(window, (x, y), false);
        } else {
            app.pending_geometry = Some(new_geo);
            app.pending_since = Some(std::time::Instant::now());
        }
    } else if let Some(x11) = app.window.x11_surface() {
        // X11 path — configure takes effect immediately.
        if let Err(e) = x11.configure(new_geo) {
            tracing::warn!("X11 configure failed for window_id={window_id}: {e}");
        }
        app.geometry = Some(new_geo);
        let window = app.window.clone();
        state.space.map_element(window, (x, y), false);
    }
}

fn ipc_close(state: &mut EafvilState, window_id: u64) {
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

fn ipc_set_visibility(state: &mut EafvilState, window_id: u64, visible: bool) {
    tracing::debug!("IPC set_visibility window={window_id} visible={visible}");
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    app.visible = visible;
    let win = app.window.clone();
    let geo = app.geometry;
    if !visible {
        state.space.unmap_elem(&win);
    } else if let Some(geo) = geo {
        state.space.map_element(win, geo.loc, false);
    }
}

fn ipc_forward_key(
    state: &mut EafvilState,
    window_id: u64,
    keycode: u32,
    key_state: u32,
    // TODO: modifiers parameter is currently ignored; the injected key event
    // uses whatever modifier state is already active on the keyboard.
    // For correct Shift+Tab etc., apply via keyboard.set_modifiers() first.
    _modifiers: u32,
) {
    tracing::debug!("IPC forward_key window={window_id} key={keycode} state={key_state}");

    // Clone the target surface to release the borrow on state.apps.
    let target = state.apps.get(window_id).and_then(|app| {
        app.window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .or_else(|| app.window.x11_surface().and_then(|x| x.wl_surface()))
    });
    let Some(target) = target else {
        tracing::warn!("forward_key: unknown window_id={window_id}");
        return;
    };

    // Validate key_state before touching focus to avoid leaking focus state.
    let press_state = match key_state {
        1 => smithay::backend::input::KeyState::Pressed,
        0 => smithay::backend::input::KeyState::Released,
        other => {
            tracing::warn!("forward_key: invalid key_state={other}, ignoring");
            return;
        }
    };

    let Some(keyboard) = state.seat.get_keyboard() else {
        tracing::warn!("forward_key: keyboard not available");
        return;
    };
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    let time = (state.start_time.elapsed().as_millis() & 0xFFFF_FFFF) as u32;

    // Temporarily switch focus to the EAF app, inject the key, then restore.
    let saved_focus = keyboard.current_focus();
    keyboard.set_focus(state, Some(target), serial);

    keyboard.input::<(), _>(
        state,
        keycode.into(),
        press_state,
        serial,
        time,
        |_, _, _| smithay::input::keyboard::FilterResult::Forward,
    );

    // Restore keyboard focus.
    let restore_serial = smithay::utils::SERIAL_COUNTER.next_serial();
    keyboard.set_focus(state, saved_focus, restore_serial);
}

fn ipc_add_mirror(
    state: &mut EafvilState,
    window_id: u64,
    view_id: u64,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    tracing::debug!("IPC add_mirror window={window_id} view={view_id} ({x},{y},{w},{h})");
    if w <= 0 || h <= 0 {
        tracing::warn!("IPC add_mirror: invalid size ({w}x{h}), ignoring");
        return;
    }
    let geo = smithay::utils::Rectangle::new((x, y).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        tracing::warn!("add_mirror: unknown window_id={window_id}");
        return;
    };
    app.mirrors.insert(
        view_id,
        crate::apps::MirrorView {
            geometry: geo,
            render_id: smithay::backend::renderer::element::Id::new(),
            popup_render_ids: Vec::new(),
        },
    );
}

fn ipc_update_mirror_geometry(
    state: &mut EafvilState,
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
    let geo = smithay::utils::Rectangle::new((x, y).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    if let Some(mv) = app.mirrors.get_mut(&view_id) {
        mv.geometry = geo;
    }
}

fn ipc_remove_mirror(state: &mut EafvilState, window_id: u64, view_id: u64) {
    tracing::debug!("IPC remove_mirror window={window_id} view={view_id}");
    if let Some(app) = state.apps.get_mut(window_id) {
        app.mirrors.remove(&view_id);
    }
}

fn ipc_request_activation_token(state: &mut EafvilState) {
    let (token, _data) = state.xdg_activation_state.create_external_token(None);
    let token_str = token.to_string();
    tracing::debug!("IPC request_activation_token: {token_str}");
    state
        .ipc
        .send(ipc::OutgoingMessage::ActivationToken { token: token_str });
}

fn ipc_promote_mirror(state: &mut EafvilState, window_id: u64, view_id: u64) {
    tracing::debug!("IPC promote_mirror window={window_id} view={view_id}");
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    // The promoted mirror becomes the source — its geometry becomes
    // the app's source geometry. Surface is NOT resized (resize only
    // happens when the user manually adjusts the window size).
    if let Some(mv) = app.mirrors.remove(&view_id) {
        app.geometry = Some(mv.geometry);
        let window = app.window.clone();
        state.space.map_element(window, mv.geometry.loc, false);
    }
}

fn start_xwayland(
    handle: smithay::reexports::calloop::LoopHandle<'static, EafvilState>,
    state: &mut EafvilState,
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

                    // Spawn child now that both WAYLAND_DISPLAY and DISPLAY are set.
                    if let Some((cmd, args)) = state.pending_command.take() {
                        spawn_child(&cmd, &args, display_number, state);
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
    event_loop: &mut smithay::reexports::calloop::EventLoop<EafvilState>,
    clipboard: &clipboard::ClipboardProxy,
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

fn handle_clipboard_event(state: &mut EafvilState, event: clipboard::ClipboardEvent) {
    use clipboard::ClipboardEvent;

    match event {
        ClipboardEvent::HostSelectionChanged { target, mime_types } => {
            inject_host_selection(state, target, mime_types);
        }
        ClipboardEvent::HostSendRequest {
            target,
            mime_type,
            fd,
        } => {
            forward_client_selection(state, target, mime_type, fd);
        }
        ClipboardEvent::SourceCancelled { target } => {
            tracing::debug!("Host source cancelled ({target:?})");
        }
    }
}

fn inject_host_selection(
    state: &mut EafvilState,
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

    if mime_types.is_empty() {
        tracing::debug!("Host {target:?} cleared");
        match target {
            SelectionTarget::Clipboard => {
                clear_data_device_selection(&state.display_handle, &state.seat)
            }
            SelectionTarget::Primary => clear_primary_selection(&state.display_handle, &state.seat),
        }
    } else {
        tracing::debug!("Host {target:?} changed ({} types)", mime_types.len());
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

fn forward_client_selection(
    state: &mut EafvilState,
    target: smithay::wayland::selection::SelectionTarget,
    mime_type: String,
    fd: std::os::fd::OwnedFd,
) {
    use smithay::wayland::selection::data_device::request_data_device_client_selection;
    use smithay::wayland::selection::primary_selection::request_primary_client_selection;
    use smithay::wayland::selection::SelectionTarget;

    let result = match target {
        SelectionTarget::Clipboard => {
            request_data_device_client_selection(&state.seat, mime_type, fd)
                .map_err(|e| format!("{e:?}"))
        }
        SelectionTarget::Primary => request_primary_client_selection(&state.seat, mime_type, fd)
            .map_err(|e| format!("{e:?}")),
    };
    if let Err(e) = result {
        tracing::warn!("Failed to forward {target:?} selection to host: {e}");
    }
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
