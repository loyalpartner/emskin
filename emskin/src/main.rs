pub mod apps;
mod clipboard;
mod clipboard_x11;
mod crosshair;
mod handlers;
mod input;
pub mod ipc;
mod skeleton;
mod state;
mod utils;
mod winit;

use clap::Parser;
use include_dir::{include_dir, Dir};
use smithay::reexports::wayland_server::Display;
pub use state::EmskinState;

static ELISP_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../elisp");

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

    let ipc = crate::ipc::IpcServer::bind(ipc_path)?;
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
    crate::winit::init_winit(&mut event_loop, &mut state)?;

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
        let has_clipboard_events = !clipboard_events.is_empty();
        for event in clipboard_events {
            handle_clipboard_event(state, event);
        }
        // Flush immediately so Wayland clients see selection changes / send
        // requests without waiting for the next render frame.
        if has_clipboard_events {
            let _ = state.display_handle.flush_clients();
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
        if let Some(elisp_dir) = extract_embedded_elisp() {
            full_args.push("--directory".to_string());
            full_args.push(elisp_dir.to_string_lossy().into_owned());
            full_args.push("-l".to_string());
            full_args.push("emskin".to_string());
            state.elisp_dir = Some(elisp_dir);
        }
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
        .spawn()
    {
        Ok(child) => state.emacs_child = Some(child),
        Err(e) => tracing::error!("Failed to spawn '{command}': {e}"),
    }
}

fn runtime_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string())
}

/// Extract embedded elisp files to `$XDG_RUNTIME_DIR/emskin-<pid>/elisp/`.
fn extract_embedded_elisp() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(format!(
        "{}/emskin-{}/elisp",
        runtime_dir(),
        std::process::id()
    ));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::error!("Failed to create elisp dir {}: {e}", dir.display());
        return None;
    }
    for file in ELISP_DIR.files() {
        let dest = dir.join(file.path());
        if let Err(e) = std::fs::write(&dest, file.contents()) {
            tracing::error!("Failed to write {}: {e}", dest.display());
            return None;
        }
    }
    tracing::info!("Extracted embedded elisp to {}", dir.display());
    Some(dir)
}

fn default_ipc_path() -> std::path::PathBuf {
    let pid = std::process::id();
    std::path::PathBuf::from(format!("{}/emskin-{pid}.ipc", runtime_dir()))
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
        IncomingMessage::RequestActivationToken => {
            ipc_request_activation_token(state);
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
    }
}

fn ipc_set_geometry(state: &mut EmskinState, window_id: u64, x: i32, y: i32, w: i32, h: i32) {
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
        state.space.unmap_elem(&win);
    } else if let Some(geo) = geo {
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
    let geo = smithay::utils::Rectangle::new((x, y).into(), (w, h).into());
    let Some(app) = state.apps.get_mut(window_id) else {
        return;
    };
    if let Some(mv) = app.mirrors.get_mut(&view_id) {
        mv.geometry = geo;
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

fn ipc_request_activation_token(state: &mut EmskinState) {
    let (token, _data) = state.xdg_activation_state.create_external_token(None);
    let token_str = token.to_string();
    tracing::debug!("IPC request_activation_token: {token_str}");
    state
        .ipc
        .send(ipc::OutgoingMessage::ActivationToken { token: token_str });
}

fn ipc_promote_mirror(state: &mut EmskinState, window_id: u64, view_id: u64) {
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
