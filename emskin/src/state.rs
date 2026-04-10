use std::{ffi::OsString, sync::Arc};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{
            generic::Generic, EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction,
        },
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
            Display, DisplayHandle,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::xdg::{decoration::XdgDecorationState, XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

pub struct PendingCommand {
    pub command: String,
    pub args: Vec<String>,
    pub standalone: bool,
}

pub struct EmskinState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub ipc: crate::ipc::IpcServer,
    pub apps: crate::apps::AppManager,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, EmskinState>,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<EmskinState>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub viewporter_state: ViewporterState,
    pub xdg_decoration_state: XdgDecorationState,
    pub xdg_activation_state: XdgActivationState,
    pub xwayland_shell_state: XWaylandShellState,
    pub popups: PopupManager,

    // XWayland
    pub xwm: Option<X11Wm>,
    pub xdisplay: Option<u32>,

    pub seat: Seat<Self>,

    // --- emskin specific ---
    /// The Emacs surface (first toplevel to connect)
    pub emacs_surface: Option<WlSurface>,

    /// Whether the initial size has been configured.
    /// Set to true once Emacs receives the host window size in its first configure.
    /// After this, host Resized events propagate size to Emacs.
    pub initial_size_settled: bool,

    /// Handle to the spawned Emacs process
    pub emacs_child: Option<std::process::Child>,

    /// Path to extracted elisp dir (for cleanup on exit).
    pub elisp_dir: Option<std::path::PathBuf>,

    /// Pending fullscreen request to forward to host window.
    /// Some(true) = request fullscreen, Some(false) = exit fullscreen
    pub pending_fullscreen: Option<bool>,

    /// Pending maximize request to forward to host window.
    pub pending_maximize: Option<bool>,

    /// Emacs window title, forwarded to host toplevel
    pub emacs_title: Option<String>,

    /// Emacs app_id, forwarded to host toplevel
    pub emacs_app_id: Option<String>,

    /// Child command to spawn once XWayland is ready (None = already spawned or --no-spawn).
    pub pending_command: Option<PendingCommand>,

    /// Clipboard synchronization proxy (Wayland or X11 backend, None if unavailable)
    pub clipboard: Option<crate::clipboard::HostClipboard>,

    /// First internal selection per target already seen (and skipped).
    /// Emacs/GTK sets clipboard on startup; we skip that to avoid overriding the host.
    pub clipboard_init_done: bool,
    pub primary_init_done: bool,

    /// Saved keyboard focus before a prefix key redirect (C-x, C-c, M-x).
    /// `Some(focus)` = prefix active, restore `focus` when done; `None` = normal.
    /// Cleared on prefix_done IPC, click, or set_focus.
    pub prefix_saved_focus: Option<Option<WlSurface>>,

    /// Crosshair overlay (caliper tool).
    pub crosshair: crate::crosshair::CrosshairOverlay,

    /// Skeleton overlay (frame layout inspector).
    pub skeleton: crate::skeleton::SkeletonOverlay,

    /// Set to true when a left-button press was swallowed by a skeleton
    /// label hit-test. The matching release must also be swallowed so the
    /// downstream surface never sees an unpaired release.
    pub skeleton_click_absorbed: bool,
}

impl EmskinState {
    pub fn new(
        event_loop: &mut EventLoop<Self>,
        loop_handle: LoopHandle<'static, Self>,
        display: Display<Self>,
        ipc: crate::ipc::IpcServer,
        xkb_config: smithay::input::keyboard::XkbConfig<'_>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let start_time = std::time::Instant::now();
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let popups = PopupManager::default();

        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);

        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        seat.add_keyboard(xkb_config, 200, 25)
            .map_err(|e| format!("failed to initialize keyboard: {e:?}"))?;
        seat.add_pointer();

        let space = Space::default();

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        let loop_signal = event_loop.get_signal();

        Ok(Self {
            start_time,
            display_handle: dh,

            ipc,
            apps: crate::apps::AppManager::default(),

            space,
            loop_signal,
            loop_handle,
            socket_name,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            fractional_scale_manager_state,
            viewporter_state,
            xdg_decoration_state,
            xdg_activation_state,
            xwayland_shell_state,
            popups,
            xwm: None,
            xdisplay: None,
            seat,

            // emskin specific
            emacs_surface: None,
            initial_size_settled: false,
            emacs_child: None,
            elisp_dir: None,
            pending_fullscreen: None,
            pending_maximize: None,
            emacs_title: None,
            emacs_app_id: None,
            pending_command: None,
            clipboard: None,
            clipboard_init_done: false,
            primary_init_done: false,
            prefix_saved_focus: None,
            crosshair: crate::crosshair::CrosshairOverlay::new(),
            skeleton: crate::skeleton::SkeletonOverlay::new(),
            skeleton_click_absorbed: false,
        })
    }

    fn init_wayland_listener(
        display: Display<EmskinState>,
        event_loop: &mut EventLoop<Self>,
    ) -> Result<OsString, Box<dyn std::error::Error>> {
        let listening_socket = ListeningSocketSource::new_auto()?;
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                if let Err(e) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(ClientState::default()))
                {
                    tracing::error!("Failed to insert Wayland client: {}", e);
                }
            })
            .map_err(|e| format!("failed to init wayland event source: {e}"))?;

        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // SAFETY: `display` is owned by the Generic source and lives for
                    // the entire event loop. No other mutable reference to the Display
                    // exists during this callback, as calloop guarantees single-threaded
                    // dispatch. We never drop the display while the source is active.
                    unsafe {
                        if let Err(e) = display.get_mut().dispatch_clients(state) {
                            tracing::error!("dispatch_clients failed: {}", e);
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| format!("failed to init display event source: {e}"))?;

        Ok(socket_name)
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // 1. Check mirror regions first — they overlay Emacs visually.
        //    Use app.geometry (always available) instead of space.element_geometry
        //    because the source window may be unmapped (visible=false).
        if let Some((window_id, _view_id, mapped_pos)) = self.apps.mirror_under(pos) {
            if let Some(app) = self.apps.get(window_id) {
                if let Some(geo) = app.geometry {
                    let local = mapped_pos - geo.loc.to_f64();
                    let result =
                        app.window
                            .surface_under(local, WindowSurfaceType::ALL)
                            .map(|(s, p)| {
                                let surface_global = (p + geo.loc).to_f64();
                                let offset = pos - mapped_pos;
                                (s, surface_global + offset)
                            });
                    tracing::debug!(
                        "mirror surface_under: pos=({:.0},{:.0}) mapped=({:.0},{:.0}) \
                         local=({:.0},{:.0}) geo=({},{}) hit={}",
                        pos.x,
                        pos.y,
                        mapped_pos.x,
                        mapped_pos.y,
                        local.x,
                        local.y,
                        geo.loc.x,
                        geo.loc.y,
                        result.is_some(),
                    );
                    return result;
                }
            }
        }

        // 2. Check real surfaces in the space.
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }
}

/// Data associated with each wayland client connection.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
