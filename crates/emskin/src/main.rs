use clap::Parser;
use include_dir::{include_dir, Dir};
use smithay::reexports::wayland_server::Display;

use emskin::{ipc, state, EmskinState};
use emskin_clipboard::{BackendHint, ClipboardBackend};

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

    /// Pin the XWayland DISPLAY number that emskin asks
    /// xwayland-satellite to claim. Without this, the supervisor
    /// scans `/tmp/.X{0..N}-lock` from 0 for a free slot — which races
    /// when multiple emskin instances start in parallel (E2E tests with
    /// default `--test-threads`). The test harness uses this to
    /// pre-allocate a unique number per test.
    #[arg(long)]
    xwayland_display: Option<u32>,

    /// Path to the `xwayland-satellite` binary. Defaults to the binary
    /// found on `$PATH`.
    #[arg(long, default_value = "xwayland-satellite")]
    xwayland_satellite_bin: std::path::PathBuf,
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

    // Claim keyboard focus on the host via xdg_activation_v1 if we
    // inherited an XDG_ACTIVATION_TOKEN / DESKTOP_STARTUP_ID — real
    // GNOME/KWin startup-notification path, and the only way to get
    // focus on hosts that don't auto-focus new toplevels (Mutter).
    // No-op if env is empty or host lacks xdg_activation_v1.
    activate_main_surface_if_env_token(&state);

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
        // Fallback chain: data-control (owns its own host connection, focus-free)
        // → wl_data_device on winit's shared connection (focus-gated) → X11
        // selection (only meaningful when the host is Xorg). See
        // emskin_clipboard::BackendHint for per-variant semantics.
        let mut hints: Vec<BackendHint> = vec![BackendHint::DataControl];
        if let Some(ptr) = host_wl_display_ptr(&state) {
            // SAFETY: the wl_display is owned by winit's backend, which
            // lives in `state.backend` for the entire compositor run. The
            // returned clipboard backend sits in `state.selection.clipboard`
            // on the same struct, so default field-drop order guarantees
            // the backend drops before the wl_display.
            hints.push(unsafe { BackendHint::wl_data_device(ptr) });
        }
        hints.push(BackendHint::X11);

        state.selection.clipboard = emskin_clipboard::init(&hints);
        if let Some(ref clipboard) = state.selection.clipboard {
            register_clipboard_source(&mut event_loop, clipboard.as_ref())?;
        }
    } else {
        tracing::info!("EMSKIN_DISABLE_HOST_CLIPBOARD set; host clipboard sync disabled");
    }

    if !cli.no_spawn {
        state.xwayland.set_pending_command(state::PendingCommand {
            command: cli.command.clone(),
            args: cli.command_args.clone(),
            standalone: cli.standalone,
        });
    }

    start_xwayland_satellite(
        event_loop.handle(),
        &mut state,
        cli.xwayland_display.unwrap_or(0),
        &cli.xwayland_satellite_bin,
    );

    // Launch the external workspace bar (if configured). Done after the
    // Wayland socket exists so the child inherits WAYLAND_DISPLAY via the
    // parent environment.
    spawn_bar(&cli.bar, &mut state);

    event_loop.run(None, &mut state, emskin::tick::event_loop_tick)?;

    // Clean up Emacs child process
    if let Some(mut child) = state.emacs.take_child() {
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
        .env("XDG_SESSION_DESKTOP", "emskin")
        // .env("GTK_IM_MODULE","wayland")
        // .env("QT_IM_MODULE","wayland")
        .spawn()
    {
        Ok(child) => state.emacs.set_child(child),
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

/// niri-style xwayland-satellite integration.
///
/// emskin pre-binds the X11 display sockets and only spawns the external
/// `xwayland-satellite` process when an X11 client first connects.
/// satellite crashes are handled transparently: the spawner thread
/// observes the exit, sends `ToMain::Rearm` through a calloop channel,
/// and the main loop re-installs the socket watch.
fn start_xwayland_satellite(
    handle: smithay::reexports::calloop::LoopHandle<'static, EmskinState>,
    state: &mut EmskinState,
    display_start: u32,
    binary: &std::path::Path,
) {
    use emskin::xwayland_satellite::{
        setup_connection, test_ondemand, SpawnConfig, ToMain, XwlsIntegration,
    };
    use smithay::reexports::calloop::channel;

    // Niri pattern: probe the binary first. A missing / incompatible
    // satellite disables the XWayland integration rather than crashing
    // the compositor.
    if !test_ondemand(binary) {
        tracing::warn!(
            "xwayland-satellite at {} not available or lacks --test-listenfd-support; \
             XWayland disabled",
            binary.display()
        );
        return;
    }

    let sockets = match setup_connection(display_start) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("xwayland-satellite: failed to bind X11 sockets: {e}");
            return;
        }
    };
    let display = sockets.display;
    let display_name = sockets.display_name.clone();

    let Some(socket_name) = state.socket_name.to_str() else {
        tracing::error!(
            "xwayland-satellite: wayland socket name is not valid UTF-8; aborting setup"
        );
        return;
    };
    let spawn_cfg = SpawnConfig {
        binary: binary.to_path_buf(),
        wayland_socket: std::path::PathBuf::from(socket_name),
        xdg_runtime_dir: std::path::PathBuf::from(runtime_dir()),
    };

    let (tx, rx) = channel::channel::<ToMain>();
    state
        .xwayland
        .set_integration(XwlsIntegration::new(sockets, spawn_cfg, tx));

    // Rearm handler: when the spawner thread reports child exit, drain
    // pending connections and re-install the socket watch.
    let rearm_handle = handle.clone();
    if let Err(e) = handle.insert_source(rx, move |event, _, st| {
        if let channel::Event::Msg(ToMain::Rearm) = event {
            if let Some(x) = st.xwayland.integration_mut() {
                if let Err(e) = x.on_rearm(&rearm_handle) {
                    tracing::warn!("xwayland-satellite rearm failed: {e}");
                }
            }
        }
    }) {
        tracing::error!("xwayland-satellite: failed to install rearm channel: {e}");
        state.xwayland.clear_integration();
        return;
    }

    if let Err(e) = state
        .xwayland
        .integration_mut()
        .expect("set_integration above guarantees Some")
        .arm(&handle)
    {
        tracing::error!("xwayland-satellite: arm() failed: {e}");
        state.xwayland.clear_integration();
        return;
    }

    // Socket is ready — export DISPLAY and notify Emacs. First X client
    // connect will trigger the on-demand satellite spawn automatically.
    std::env::set_var("DISPLAY", &display_name);
    state.xwayland.set_display(display);
    state
        .ipc
        .send(ipc::OutgoingMessage::XWaylandReady { display });
    tracing::info!("xwayland-satellite: socket ready on {display_name}");

    // Spawn pending Emacs now that both WAYLAND_DISPLAY and DISPLAY are set.
    if let Some(pc) = state.xwayland.take_pending_command() {
        spawn_child(&pc.command, &pc.args, display, pc.standalone, state);
    }
}

fn register_clipboard_source(
    event_loop: &mut smithay::reexports::calloop::EventLoop<EmskinState>,
    clipboard: &dyn ClipboardBackend,
) -> Result<(), Box<dyn std::error::Error>> {
    use emskin_clipboard::Driver;
    use smithay::reexports::calloop::{generic::Generic, Interest, Mode, PostAction};
    use std::os::unix::io::{AsRawFd, FromRawFd};

    // Piggyback backends (wl_data_device on a foreign wl_display) are drained
    // every tick from tick.rs — no owned fd to register here.
    let raw_fd = match clipboard.driver() {
        Driver::OwnedFd(fd) => fd.as_raw_fd(),
        Driver::Piggyback => return Ok(()),
    };

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

/// Return the host `wl_surface` pointer of emskin's winit main window
/// if running on Wayland. Used together with `host_wl_display_ptr` to
/// call `xdg_activation_v1.activate` on the main surface.
fn host_wl_surface_ptr(state: &EmskinState) -> Option<*mut std::ffi::c_void> {
    use winit_crate::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let backend = state.backend.as_ref()?;
    let handle = backend.window().window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Wayland(wl) => Some(wl.surface.as_ptr()),
        _ => None,
    }
}

/// If `XDG_ACTIVATION_TOKEN` (or `DESKTOP_STARTUP_ID`) is present in the
/// environment and the host compositor advertises
/// `xdg_activation_v1`, call `activate(token, main_surface)` to move
/// keyboard focus to emskin's main window. This is the protocol-legal
/// startup-notification path real Mutter / KWin both honour (and the
/// only focus path emez gives us now that it no longer auto-focuses
/// new toplevels).
///
/// Does nothing on X11, when the token is absent, or when the host
/// lacks `xdg_activation_v1`. Runs as a short self-contained event
/// loop (bind → activate → roundtrip → drop) — the activation token
/// itself stays valid for the emez / host compositor run; we don't
/// need to keep our wayland connection alive.
fn activate_main_surface_if_env_token(state: &EmskinState) {
    use wayland_client::backend::{Backend, ObjectId};
    use wayland_client::protocol::wl_registry;
    use wayland_client::protocol::wl_surface::WlSurface;
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::{
        self, XdgActivationV1,
    };

    let Some(token) = std::env::var("XDG_ACTIVATION_TOKEN")
        .ok()
        .or_else(|| std::env::var("DESKTOP_STARTUP_ID").ok())
    else {
        return;
    };
    let Some(display_ptr) = host_wl_display_ptr(state) else {
        return;
    };
    let Some(surface_ptr) = host_wl_surface_ptr(state) else {
        return;
    };

    // SAFETY: display_ptr + surface_ptr come from winit's raw-window-handle,
    // both valid for at least the duration of this short sync.
    let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
    let connection = Connection::from_backend(backend);

    struct State {
        activation: Option<XdgActivationV1>,
    }
    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == "xdg_activation_v1" && state.activation.is_none() {
                    state.activation = Some(registry.bind(name, version.min(1), qh, ()));
                }
            }
        }
    }
    impl Dispatch<XdgActivationV1, ()> for State {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: xdg_activation_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    let mut queue = connection.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut st = State { activation: None };

    if let Err(e) = queue.roundtrip(&mut st) {
        tracing::debug!("xdg_activation roundtrip failed: {e}");
        return;
    }

    let Some(activation) = st.activation.as_ref() else {
        tracing::debug!("host does not advertise xdg_activation_v1; skip self-activate");
        return;
    };

    // Wrap the raw wl_surface pointer from winit into a proxy on this
    // connection. SAFETY: surface_ptr is a live wl_surface proxy from
    // winit, and we only use this wrapped handle to issue one request
    // (`activate`) that doesn't destroy or mutate its state.
    let Ok(surface_id) =
        (unsafe { ObjectId::from_ptr(WlSurface::interface(), surface_ptr.cast()) })
    else {
        tracing::debug!("failed to wrap wl_surface ptr into proxy id");
        return;
    };
    let Ok(surface) = WlSurface::from_id(&connection, surface_id) else {
        tracing::debug!("failed to construct WlSurface proxy from id");
        return;
    };

    activation.activate(token.clone(), &surface);
    if let Err(e) = connection.flush() {
        tracing::warn!("xdg_activation flush failed: {e}");
    }
    // One roundtrip so the activate request actually leaves our queue
    // before we drop the connection.
    let _ = queue.roundtrip(&mut st);
    tracing::info!(
        "requested self-activation via xdg_activation_v1 (token bytes={})",
        token.len()
    );
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
