use clap::Parser;
use include_dir::{include_dir, Dir};
use smithay::reexports::wayland_server::Display;

use emskin::{clipboard, clipboard_wl, clipboard_x11, cursor_x11, ipc, state, EmskinState};

static ELISP_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../elisp");
static DEMO_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../../demo");

/// Nested Wayland compositor for Emacs Application Framework.
#[derive(Parser, Debug)]
#[command(
    name = "emskin",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("EMSKIN_GIT_SHA"), ")")
)]
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

    /// Pin the Wayland display socket name (default: auto-chosen wayland-N
    /// by smithay). Useful when external Wayland clients (wl-copy, xclip,
    /// E2E tests) need a predictable `WAYLAND_DISPLAY`. Overrides the
    /// `EMSKIN_WAYLAND_SOCKET_NAME` env var if both are set.
    #[arg(long)]
    wayland_socket: Option<String>,

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

    /// Request fullscreen for the host compositor window on startup.
    #[arg(long)]
    fullscreen: bool,

    /// How to launch the external workspace bar:
    ///   * `auto`  — find `emskin-bar` next to this binary or on PATH (default)
    ///   * `none`  — don't launch a bar (user manages their own / doesn't want one)
    ///   * `<path>` — launch the binary at an explicit path (e.g. waybar)
    #[arg(long, default_value = "auto")]
    bar: String,

    /// Write tracing logs to this file instead of stderr.
    /// Useful for E2E tests that want clean test output but preserved
    /// diagnostics on failure.
    #[arg(long)]
    log_file: Option<std::path::PathBuf>,

    /// Pin the XWayland DISPLAY number (passed through to
    /// `XWayland::spawn`). Without this, smithay scans
    /// `/tmp/.X11-unix/X0..X32` for a free slot — which races when
    /// multiple emskin instances start in parallel (e.g. E2E tests with
    /// default `--test-threads`). The test harness uses this to
    /// pre-allocate a unique number per test.
    #[arg(long)]
    xwayland_display: Option<u32>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_logging(cli.log_file.as_deref());

    // --wayland-socket is plumbed through an env var so state.rs's
    // `init_wayland_listener` can stay signature-stable; CLI flag takes
    // precedence over a pre-set env var by overwriting it here.
    if let Some(ref name) = cli.wayland_socket {
        std::env::set_var("EMSKIN_WAYLAND_SOCKET_NAME", name);
    }

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

    register_ipc_source(&mut event_loop, &state)?;

    // Open a Wayland/X11 window for our nested compositor. Must happen
    // before clipboard init — the wl_data_device fallback piggybacks on
    // winit's host Wayland connection to get focused-client selection
    // events without needing our own host surface.
    emskin::winit::init_winit(&mut event_loop, &mut state, cli.fullscreen)?;

    // Initialize clipboard synchronization with host compositor.
    // Fallback chain: Wayland data-control (no-focus, preferred) →
    // wl_data_device via winit's shared connection (focus-gated) →
    // X11 selection (if host is Xorg).
    //
    // Test hook: `EMSKIN_DISABLE_HOST_CLIPBOARD=1` disables host clipboard
    // sync entirely. Kept as a safety valve for debugging; the E2E
    // harness doesn't need it anymore because each test gets its own
    // private host compositor (see `tests/common/mod.rs::NestedHost`).
    if std::env::var_os("EMSKIN_DISABLE_HOST_CLIPBOARD").is_none() {
        state.selection.clipboard = clipboard::ClipboardProxy::new()
            .map(|p| Box::new(p) as Box<dyn clipboard::ClipboardBackend>)
            .or_else(|| {
                // SAFETY: `host_wl_display_ptr` returns the wl_display
                // owned by winit's backend. `state.backend` stays alive
                // for the entire compositor run (dropped when main()
                // returns), and `state.selection.clipboard` is dropped
                // before state.backend as part of the same struct's
                // default field-drop order — so the proxy never outlives
                // the wl_display it borrows.
                let ptr = host_wl_display_ptr(&state)?;
                unsafe { clipboard_wl::WlDataDeviceProxy::new(ptr) }
                    .map(|p| Box::new(p) as Box<dyn clipboard::ClipboardBackend>)
            })
            .or_else(|| {
                clipboard_x11::X11ClipboardProxy::new()
                    .map(|p| Box::new(p) as Box<dyn clipboard::ClipboardBackend>)
            });
        if let Some(ref clipboard) = state.selection.clipboard {
            register_clipboard_source(&mut event_loop, clipboard.as_ref())?;
        }
    } else {
        tracing::info!("EMSKIN_DISABLE_HOST_CLIPBOARD set; host clipboard sync disabled");
    }

    if !cli.no_spawn {
        state.pending_command = Some(state::PendingCommand {
            command: cli.command.clone(),
            args: cli.command_args.clone(),
            standalone: cli.standalone,
        });
    }

    start_xwayland(event_loop.handle(), &mut state, cli.xwayland_display);

    // Launch the external workspace bar (if configured). Done after the
    // Wayland socket exists so the child inherits WAYLAND_DISPLAY via the
    // parent environment.
    spawn_bar(&cli.bar, &mut state);

    event_loop.run(None, &mut state, emskin::tick::event_loop_tick)?;

    // Clean up Emacs child process
    if let Some(mut child) = state.emacs_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Bar child: we originally hoped the Wayland socket close (on state
    // drop) would make the bar exit on its own, but state is still alive
    // here — wait() would deadlock. Kill explicitly, same as Emacs.
    if let Some(mut child) = state.bar_child.take() {
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

/// Launch the external workspace bar according to the `--bar` flag.
///
/// `auto` looks first next to the emskin binary (same directory — matches
/// the AUR package layout and the dev target dir), then falls back to PATH.
/// `none` skips entirely. Any other value is treated as an explicit path —
/// useful for wiring in a third-party bar like waybar.
fn spawn_bar(mode: &str, state: &mut EmskinState) {
    let binary: std::path::PathBuf = match mode {
        "none" => {
            tracing::info!("--bar=none: not launching a workspace bar");
            return;
        }
        "auto" => match locate_bar_binary() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "--bar=auto: emskin-bar not found next to emskin or on PATH; \
                     continuing without a workspace bar"
                );
                return;
            }
        },
        explicit => std::path::PathBuf::from(explicit),
    };

    // The bar must connect to *our* Wayland socket, not the host compositor's
    // — otherwise it'd fail to bind ext-workspace-v1 / wlr-layer-shell and
    // die immediately. `socket_name` is the name emskin advertised in
    // XDG_RUNTIME_DIR.
    let Some(socket_name) = state.socket_name.to_str() else {
        tracing::error!("Wayland socket name is not valid UTF-8, cannot spawn bar");
        return;
    };

    tracing::info!(
        "Spawning workspace bar: {} (WAYLAND_DISPLAY={socket_name})",
        binary.display(),
    );
    match std::process::Command::new(&binary)
        .env("WAYLAND_DISPLAY", socket_name)
        .spawn()
    {
        Ok(child) => state.bar_child = Some(child),
        Err(e) => {
            tracing::warn!("Failed to spawn bar {}: {e}", binary.display());
        }
    }
}

/// Locate `emskin-bar`: prefer the sibling binary next to the current
/// executable, then fall back to whatever `which` finds on `PATH`.
fn locate_bar_binary() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("emskin-bar");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    // PATH lookup. std has no helper — iterate manually.
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("emskin-bar");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
    display: Option<u32>,
) {
    use smithay::xwayland::{XWayland, XWaylandEvent};

    let dh = state.display_handle.clone();

    let (xwayland, client) = match XWayland::spawn(
        &dh,
        display,
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
                            (
                                SelectionTarget::Clipboard,
                                &state.selection.host_clipboard_mimes,
                            ),
                            (
                                SelectionTarget::Primary,
                                &state.selection.host_primary_mimes,
                            ),
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

/// Return the host `wl_display` pointer from winit's backend if it's
/// running on the Wayland platform. Returns `None` on X11 or if no
/// backend has been initialized yet.
fn host_wl_display_ptr(state: &EmskinState) -> Option<*mut std::ffi::c_void> {
    use winit_crate::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    let backend = state.backend.as_ref()?;
    let handle = backend.window().display_handle().ok()?;
    match handle.as_raw() {
        RawDisplayHandle::Wayland(wl) => Some(wl.display.as_ptr()),
        _ => None,
    }
}

fn init_logging(log_file: Option<&std::path::Path>) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    match log_file {
        Some(path) => match std::fs::File::create(path) {
            Ok(file) => tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_env_filter(env_filter)
                .init(),
            Err(e) => eprintln!("failed to open --log-file {}: {e}", path.display()),
        },
        None => tracing_subscriber::fmt().with_env_filter(env_filter).init(),
    }
}
