use std::{
    collections::HashMap,
    ffi::OsString,
    sync::Arc,
};

use smithay::{
    delegate_xwayland_shell,
    desktop::{PopupManager, Space, Window},
    input::{Seat, SeatState},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode as CMode,
            PostAction,
        },
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            Client, Display, DisplayHandle,
        },
    },
    utils::Transform,
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::{
            data_device::DataDeviceState,
            ext_data_control::DataControlState as ExtDataControlState,
            primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState as WlrDataControlState,
        },
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
        xdg_activation::XdgActivationState,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::X11Wm,
};

#[allow(dead_code)] // Several fields are held purely to keep their state alive for the Wayland runtime.
pub struct Emez {
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub space: Space<Window>,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, Self>,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub wlr_data_control_state: WlrDataControlState,
    pub ext_data_control_state: ExtDataControlState,
    pub xdg_activation_state: XdgActivationState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,

    /// When true, drain wl_data_device selections into compositor memory
    /// so short-lived clients (wl-copy) can exit cleanly without losing
    /// the selection. Enabled only when data-control is hidden (the
    /// simulated KDE/GNOME scenario where wl_data_device is the only
    /// clipboard path); under data-control mode every clipboard client
    /// is expected to be long-lived and running the manager is
    /// counterproductive — it would hijack selections set by well-
    /// behaved DC clients like emskin itself.
    pub clipboard_manager_enabled: bool,

    /// Buffered clipboard contents held on behalf of short-lived client
    /// sources. When a client (e.g. `wl-copy --foreground`) publishes a
    /// selection, emez reads every offered mime into memory via
    /// `request_data_device_client_selection` and then takes over with
    /// `set_data_device_selection(..., ())` so the selection survives
    /// after the originating client exits. This is the wayland
    /// equivalent of X11's CLIPBOARD_MANAGER / SAVE_TARGETS mechanism
    /// — without it, wl_data_source ownership is tied to the client's
    /// lifetime, forcing tools like wl-copy to fork a daemon.
    pub clipboard_buffer: HashMap<String, Vec<u8>>,

    /// Mime types currently published by the compositor-owned selection
    /// (matches `clipboard_buffer.keys()` but preserves offer order).
    pub clipboard_mimes: Vec<String>,

    /// "Primary" focus target — the first surface to get focus through
    /// `xdg_activation_v1.activate`. When the current focused surface
    /// is destroyed or explicitly unfocused, emez falls back to this
    /// one. Mirrors what real compositors do: focus doesn't just evaporate
    /// when a window closes, it returns to the previous in-use window.
    pub primary_fallback_focus: Option<
        smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    >,

    // XWayland state. `xwayland_shell_state` advertises the xwayland_shell
    // global unconditionally; the rest are populated by
    // `start_xwayland` (see `src/xwayland.rs`).
    pub xwayland_shell_state: XWaylandShellState,
    pub xwm: Option<X11Wm>,
    pub xdisplay: Option<u32>,
    pub xwayland_client: Option<Client>,
    pub xwayland_ready_file: Option<std::path::PathBuf>,
}

impl Emez {
    pub fn new(
        event_loop: &mut EventLoop<'static, Self>,
        display: Display<Self>,
        socket: Option<&str>,
        hide_data_control: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        // Both wlr- and ext- data-control protocols — emskin's ClipboardProxy
        // tries ext first then falls back to wlr, so advertising both means
        // either path works. The `hide_data_control` filter returns false
        // for every client when the CLI flag is set, simulating a KDE- or
        // GNOME-style host that doesn't expose data-control at all.
        let show_data_control = !hide_data_control;
        let wlr_data_control_state =
            WlrDataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), move |_| {
                show_data_control
            });
        let ext_data_control_state =
            ExtDataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), move |_| {
                show_data_control
            });
        // xdg_activation_v1: how clients (e.g. wl-copy) legitimately
        // acquire keyboard focus on compositors that don't auto-focus
        // new toplevels (GNOME Mutter, strict focus-stealing-prevention
        // KWin configs). emez accepts any token — the tests rely on
        // clients reading `XDG_ACTIVATION_TOKEN` from the env and
        // calling `xdg_activation_v1.activate` to pull focus themselves.
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let popups = PopupManager::default();

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "emez");
        seat.add_keyboard(Default::default(), 200, 25)?;
        seat.add_pointer();

        // Advertise one fake output so clients that bind wl_output see something.
        let output = Output::new(
            "emez".into(),
            PhysicalProperties {
                size: (1920, 1080).into(),
                subpixel: Subpixel::Unknown,
                make: "emez".into(),
                model: "headless".into(),
                serial_number: "0".into(),
            },
        );
        output.create_global::<Self>(&dh);
        let mode = Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            None,
            Some((0, 0).into()),
        );

        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);

        let space = Space::default();
        let loop_signal = event_loop.get_signal();
        let loop_handle = event_loop.handle();
        let socket_name = init_wayland_listener(display, event_loop, socket)?;

        Ok(Self {
            socket_name,
            display_handle: dh,
            space,
            loop_signal,
            loop_handle,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            wlr_data_control_state,
            ext_data_control_state,
            xdg_activation_state,
            popups,
            seat,
            clipboard_manager_enabled: hide_data_control,
            clipboard_buffer: HashMap::new(),
            clipboard_mimes: Vec::new(),
            primary_fallback_focus: None,
            xwayland_shell_state,
            xwm: None,
            xdisplay: None,
            xwayland_client: None,
            xwayland_ready_file: None,
        })
    }
}

impl XWaylandShellHandler for Emez {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

delegate_xwayland_shell!(Emez);

fn init_wayland_listener(
    display: Display<Emez>,
    event_loop: &mut EventLoop<Emez>,
    socket: Option<&str>,
) -> Result<OsString, Box<dyn std::error::Error>> {
    let listening_socket = match socket {
        Some(name) => ListeningSocketSource::with_name(name)?,
        None => ListeningSocketSource::new_auto()?,
    };
    let socket_name = listening_socket.socket_name().to_os_string();

    let loop_handle = event_loop.handle();
    loop_handle
        .insert_source(listening_socket, move |client_stream, _, state| {
            state
                .display_handle
                .insert_client(client_stream, Arc::new(ClientState::default()))
                .unwrap();
        })
        .map_err(|e| format!("wayland listener insert_source: {e}"))?;

    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, CMode::Level),
            |_, display, state| {
                // SAFETY: display is owned by the Generic source and lives
                // the whole event loop; no aliasing mutable reference exists.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                // Flush outgoing messages so clients see our replies to
                // get_registry / sync / bind requests; without this,
                // wayland-info and wl-copy sit in the handshake forever.
                let _ = state.display_handle.flush_clients();
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("wayland display insert_source: {e}"))?;

    Ok(socket_name)
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
}
