use clap::Parser;
use include_dir::{include_dir, Dir};
use smithay::reexports::wayland_server::Display;

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
    state.selection.clipboard = clipboard::ClipboardProxy::new()
        .map(|p| Box::new(p) as Box<dyn clipboard::ClipboardBackend>)
        .or_else(|| {
            clipboard_x11::X11ClipboardProxy::new()
                .map(|p| Box::new(p) as Box<dyn clipboard::ClipboardBackend>)
        });
    if let Some(ref clipboard) = state.selection.clipboard {
        register_clipboard_source(&mut event_loop, clipboard.as_ref())?;
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

    event_loop.run(None, &mut state, emskin::tick::event_loop_tick)?;

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
                            (SelectionTarget::Clipboard, &state.selection.host_clipboard_mimes),
                            (SelectionTarget::Primary, &state.selection.host_primary_mimes),
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
    clipboard: &dyn clipboard::ClipboardBackend,
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
                if let Some(ref mut clipboard) = state.selection.clipboard {
                    clipboard.dispatch();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("failed to register clipboard source: {e}"))?;
    Ok(())
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
