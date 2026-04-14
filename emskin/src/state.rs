use std::{collections::HashMap, ffi::OsString, sync::Arc};

use smithay::{
    backend::{renderer::gles::GlesRenderer, winit::WinitGraphicsBackend},
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{pointer::CursorImageStatus, Seat, SeatState},
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
    utils::{Logical, Point, Rectangle},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, ToplevelSurface, XdgShellState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::X11Wm,
};

use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

/// Tracks where the active selection came from, so host paste requests
/// are routed to the correct source (Wayland data_device vs X11 XWM).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionOrigin {
    #[default]
    Wayland,
    X11,
}

/// Focus-related state grouped together for clarity.
#[derive(Default)]
pub struct FocusState {
    /// Saved keyboard focus before a prefix key redirect (C-x, C-c, M-x).
    /// `Some(focus)` = prefix active, restore `focus` when done; `None` = normal.
    pub prefix_saved_focus: Option<Option<WlSurface>>,
    /// Tracks text_input focus for manual enter/leave management.
    pub text_input_focus: Option<WlSurface>,
    /// Deferred `set_ime_allowed` for the winit window.
    pub pending_ime_allowed: Option<bool>,
    /// Saved keyboard focus before a layer surface took it.
    pub layer_saved_focus: Option<WlSurface>,
}

/// Clipboard/selection routing state grouped together.
#[derive(Default)]
pub struct SelectionState {
    /// Clipboard synchronization proxy (Wayland or X11 backend).
    pub clipboard: Option<Box<dyn crate::clipboard::ClipboardBackend>>,
    /// Where the current clipboard selection came from.
    pub clipboard_origin: SelectionOrigin,
    /// Where the current primary selection came from.
    pub primary_origin: SelectionOrigin,
    /// Cached host clipboard mime types for XWM replay.
    pub host_clipboard_mimes: Vec<String>,
    /// Cached host primary mime types for XWM replay.
    pub host_primary_mimes: Vec<String>,
}

/// Smithay Wayland protocol state — pure bookkeeping for compositor protocols.
pub struct WaylandState {
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
    pub layer_shell_state: WlrLayerShellState,
    pub xwayland_shell_state: XWaylandShellState,
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub dmabuf_state: DmabufState,
    /// Keep-alive: dropping this removes the linux-dmabuf global from the display.
    pub dmabuf_global: Option<DmabufGlobal>,
    pub text_input_manager_state: smithay::wayland::text_input::TextInputManagerState,
    pub popups: PopupManager,
}

pub struct PendingCommand {
    pub command: String,
    pub args: Vec<String>,
    pub standalone: bool,
}

/// Stored state for an inactive workspace (swapped out when another is active).
pub struct Workspace {
    pub space: Space<Window>,
    pub emacs_surface: Option<WlSurface>,
    pub emacs_x11_window: Option<Window>,
    /// Display name for the bar (extracted from Emacs frame title).
    pub name: String,
}

pub struct EmskinState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub ipc: crate::ipc::IpcServer,
    pub apps: crate::apps::AppManager,

    // --- Workspace management ---
    /// The active workspace's space (swapped in/out on switch).
    pub space: Space<Window>,
    /// Inactive workspaces, keyed by workspace id.
    pub inactive_workspaces: HashMap<u64, Workspace>,
    /// The id of the currently active workspace.
    pub active_workspace_id: u64,
    /// Display name of the active workspace (from Emacs frame title).
    pub active_workspace_name: String,
    /// Next workspace id to allocate.
    pub next_workspace_id: u64,
    /// Emacs toplevels awaiting parent() check (child frame detection).
    pub pending_emacs_toplevels: Vec<(ToplevelSurface, Window)>,
    /// ext-workspace-v1 protocol state.
    pub workspace_protocol: crate::protocols::workspace::WorkspaceProtocolState,
    /// Whether the built-in workspace bar is enabled (--bar=builtin).
    pub bar_enabled: bool,
    /// Built-in workspace bar renderer.
    pub workspace_bar: crate::workspace_bar::WorkspaceBar,

    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, EmskinState>,

    /// Winit graphics backend (renderer + window). Stored here so
    /// `DmabufHandler::dmabuf_imported` can access the renderer.
    pub backend: Option<WinitGraphicsBackend<GlesRenderer>>,

    // Smithay protocol state (grouped for clarity).
    pub wl: WaylandState,

    // XWayland
    pub xwm: Option<X11Wm>,
    pub xdisplay: Option<u32>,
    pub x11_cursor_tracker: Option<crate::cursor_x11::X11CursorTracker>,

    pub seat: Seat<Self>,

    // --- emskin specific ---
    /// The Emacs surface (first toplevel to connect)
    pub emacs_surface: Option<WlSurface>,

    /// X11 Emacs window — kept to poll for wl_surface after map (XWayland
    /// associates the surface asynchronously via xwayland_shell protocol).
    pub emacs_x11_window: Option<Window>,

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

    /// Clipboard/selection routing state.
    pub selection: SelectionState,

    /// Focus management state.
    pub focus: FocusState,

    /// Crosshair overlay (caliper tool).
    pub crosshair: crate::crosshair::CrosshairOverlay,

    /// Skeleton overlay (frame layout inspector).
    pub skeleton: crate::skeleton::SkeletonOverlay,

    /// Set to true when a left-button press was swallowed by a skeleton
    /// label hit-test. The matching release must also be swallowed so the
    /// downstream surface never sees an unpaired release.
    pub skeleton_click_absorbed: bool,


    /// Current cursor image status. For Named, the host cursor is used;
    /// for Surface (GTK3/Emacs), the cursor is software-rendered each frame.
    pub cursor_status: CursorImageStatus,
    /// Set when cursor_status changes; consumed by apply_pending_state.
    pub cursor_changed: bool,

    /// Coarse damage flag for structural events (IPC, layer shell, input,
    /// workspace switch) that smithay's per-element OutputDamageTracker does
    /// not cover.  When true the next Redraw calls render_frame; cleared after.
    pub needs_redraw: bool,

    /// Startup splash screen (logo + animation), dismissed on Emacs connect.
    pub splash: crate::splash::SplashScreen,
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
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        let text_input_manager_state =
            smithay::wayland::text_input::TextInputManagerState::new::<Self>(&dh);
        let dmabuf_state = DmabufState::new();

        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        seat.add_keyboard(xkb_config, 200, 25)
            .map_err(|e| format!("failed to initialize keyboard: {e:?}"))?;
        seat.add_pointer();

        let space = Space::default();
        let workspace_protocol = crate::protocols::workspace::WorkspaceProtocolState::new(&dh);

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        let loop_signal = event_loop.get_signal();

        Ok(Self {
            start_time,
            display_handle: dh,

            ipc,
            apps: crate::apps::AppManager::default(),

            space,
            inactive_workspaces: HashMap::new(),
            active_workspace_id: 1,
            active_workspace_name: String::new(),
            next_workspace_id: 2,
            pending_emacs_toplevels: Vec::new(),
            workspace_protocol,
            bar_enabled: true,
            workspace_bar: crate::workspace_bar::WorkspaceBar::new(),

            loop_signal,
            loop_handle,
            socket_name,

            backend: None,

            wl: WaylandState {
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
                layer_shell_state,
                xwayland_shell_state,
                cursor_shape_manager_state,
                dmabuf_state,
                dmabuf_global: None,
                text_input_manager_state,
                popups,
            },
            xwm: None,
            xdisplay: None,
            x11_cursor_tracker: None,
            seat,

            // emskin specific
            emacs_surface: None,
            emacs_x11_window: None,
            initial_size_settled: false,
            emacs_child: None,
            elisp_dir: None,
            pending_fullscreen: None,
            pending_maximize: None,
            emacs_title: None,
            emacs_app_id: None,
            pending_command: None,
            selection: SelectionState::default(),
            focus: FocusState::default(),
            crosshair: crate::crosshair::CrosshairOverlay::new(),
            skeleton: crate::skeleton::SkeletonOverlay::new(),
            skeleton_click_absorbed: false,
            cursor_status: CursorImageStatus::default_named(),
            cursor_changed: false,
            needs_redraw: true,
            splash: crate::splash::SplashScreen::new(),
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
                    // Flush responses immediately so clients don't wait until
                    // the next render frame for roundtrip replies (wl_display.sync).
                    let _ = state.display_handle.flush_clients();
                    state.needs_redraw = true;
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| format!("failed to init display event source: {e}"))?;

        Ok(socket_name)
    }

    /// Fullscreen geometry for the primary output (logical pixels).
    pub fn output_fullscreen_geo(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.space.outputs().next()?;
        let mode = output.current_mode()?;
        let scale = output.current_scale().fractional_scale();
        let logical = mode.size.to_f64().to_logical(scale).to_i32_round();
        Some(Rectangle::new((0, 0).into(), logical))
    }

    /// Workspace bar height (only when 2+ workspaces and bar enabled).
    pub fn bar_height(&self) -> i32 {
        if self.bar_enabled && self.workspace_count() > 1 {
            crate::workspace_bar::BAR_HEIGHT
        } else {
            0
        }
    }

    /// Geometry for Emacs frame (accounts for workspace bar height).
    /// Returns (x=0, y=bar_height, w=output_w, h=output_h - bar_height).
    pub fn emacs_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        let mut geo = self.output_fullscreen_geo()?;
        let bar_h = self.bar_height();
        geo.loc.y = bar_h;
        geo.size.h -= bar_h;
        Some(geo)
    }

    /// Migrate an app to the active workspace if it's in a different one.
    /// Unmaps from old space, updates workspace_id. Returns true if migrated.
    pub fn migrate_app_to_active(&mut self, window_id: u64) -> bool {
        let Some(app) = self.apps.get(window_id) else {
            return false;
        };
        let old_ws = app.workspace_id;
        if old_ws == self.active_workspace_id {
            return false;
        }
        let window = app.window.clone();
        tracing::debug!(
            "app {window_id} migrating workspace {old_ws} → {}",
            self.active_workspace_id
        );
        if let Some(old_space) = self.space_for_workspace_mut(old_ws) {
            old_space.unmap_elem(&window);
        }
        if let Some(app) = self.apps.get_mut(window_id) {
            app.workspace_id = self.active_workspace_id;
            // Reset geometry so the next set_geometry immediately maps the app
            // instead of going through the pending path (which would deadlock:
            // app needs frame callbacks to commit, but it's not in any Space).
            app.geometry = None;
            app.pending_geometry = None;
            app.pending_since = None;
        }
        true
    }

    /// Allocate a new workspace id.
    pub fn alloc_workspace_id(&mut self) -> u64 {
        let id = self.next_workspace_id;
        self.next_workspace_id += 1;
        id
    }

    /// Total number of workspaces (active + inactive).
    pub fn workspace_count(&self) -> usize {
        1 + self.inactive_workspaces.len()
    }

    /// Check if a surface belongs to the same Wayland client as the active Emacs.
    pub fn is_emacs_client(&self, surface: &WlSurface) -> bool {
        self.emacs_surface
            .as_ref()
            .is_some_and(|emacs| emacs.same_client_as(&surface.id()))
    }

    /// Check if a surface is any workspace's Emacs surface (active or inactive).
    pub fn is_any_emacs_surface(&self, surface: &WlSurface) -> bool {
        if self.emacs_surface.as_ref() == Some(surface) {
            return true;
        }
        self.inactive_workspaces
            .values()
            .any(|ws| ws.emacs_surface.as_ref() == Some(surface))
    }

    /// Get mutable reference to the space for a given workspace id.
    /// Returns the active space if `ws_id` matches, otherwise looks up inactive.
    pub fn space_for_workspace_mut(&mut self, ws_id: u64) -> Option<&mut Space<Window>> {
        if ws_id == self.active_workspace_id {
            Some(&mut self.space)
        } else {
            self.inactive_workspaces
                .get_mut(&ws_id)
                .map(|ws| &mut ws.space)
        }
    }

    /// Sorted list of all workspace ids.
    pub fn all_workspace_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = std::iter::once(self.active_workspace_id)
            .chain(self.inactive_workspaces.keys().copied())
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Switch the active workspace. Returns false if target is already active
    /// or doesn't exist.
    pub fn switch_workspace(&mut self, target_id: u64) -> bool {
        if target_id == self.active_workspace_id {
            return false;
        }
        let Some(mut target) = self.inactive_workspaces.remove(&target_id) else {
            return false;
        };

        // Swap: current active → inactive, target → active.
        let old_space = std::mem::take(&mut self.space);
        let old_emacs = self.emacs_surface.take();
        let old_x11 = self.emacs_x11_window.take();
        let old_name = std::mem::take(&mut self.active_workspace_name);
        self.inactive_workspaces.insert(
            self.active_workspace_id,
            Workspace {
                space: old_space,
                emacs_surface: old_emacs,
                emacs_x11_window: old_x11,
                name: old_name,
            },
        );

        self.space = target.space;
        self.emacs_surface = target.emacs_surface.take();
        self.emacs_x11_window = target.emacs_x11_window.take();
        self.active_workspace_name = target.name;
        self.active_workspace_id = target_id;

        // App migration is handled by IPC set_geometry from Emacs (sync-all).
        // The compositor does NOT auto-migrate because it doesn't know which
        // apps are displayed in which Emacs frame.

        // Reset state that references the old workspace's surfaces.
        self.focus.prefix_saved_focus = None;
        self.focus.layer_saved_focus = None;
        self.focus.text_input_focus = None;
        self.focus.pending_ime_allowed = Some(false);
        self.skeleton.clear();
        self.skeleton.enabled = false;
        self.skeleton_click_absorbed = false;

        if matches!(self.cursor_status, CursorImageStatus::Surface(_)) {
            self.cursor_status = CursorImageStatus::default_named();
            self.cursor_changed = true;
        }

        // Reset keyboard and pointer focus to the new workspace's Emacs.
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, self.emacs_surface.clone(), serial);
        }
        // Clear pointer focus so stale hover events don't go to old workspace surfaces.
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.motion(
                self,
                None,
                &smithay::input::pointer::MotionEvent {
                    location: pointer.current_location(),
                    serial,
                    time: 0,
                },
            );
            pointer.frame(self);
        }

        tracing::info!(
            "switched to workspace {target_id} (total={})",
            self.workspace_count()
        );
        true
    }

    /// Remove an inactive workspace and its embedded apps.
    pub fn destroy_workspace(&mut self, workspace_id: u64) -> Option<Workspace> {
        let ws = self.inactive_workspaces.remove(&workspace_id)?;
        // Remove all apps belonging to this workspace.
        let dead_app_ids: Vec<u64> = self
            .apps
            .windows()
            .filter(|a| a.workspace_id == workspace_id)
            .map(|a| a.window_id)
            .collect();
        for id in dead_app_ids {
            if let Some(app) = self.apps.remove(id) {
                self.ipc.send(crate::ipc::OutgoingMessage::WindowDestroyed {
                    window_id: app.window_id,
                });
            }
        }
        tracing::info!(
            "destroyed workspace {workspace_id} (total={})",
            self.workspace_count()
        );
        Some(ws)
    }

    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        // 1. Check mirror regions first — they overlay Emacs visually.
        //    Use app.geometry (always available) instead of space.element_geometry
        //    because the source window may be unmapped (visible=false).
        if let Some((window_id, _view_id, mapped_pos)) =
            self.apps.mirror_under(pos, self.active_workspace_id)
        {
            if let Some(app) = self.apps.get(window_id) {
                if let Some(geo) = app.geometry {
                    // Compensate window_geometry (CSD shadow offset) — same
                    // as the space path where render_location = space_loc - wg.
                    let wg = app.window.geometry().loc;
                    let local = mapped_pos - geo.loc.to_f64() + wg.to_f64();
                    let result =
                        app.window
                            .surface_under(local, WindowSurfaceType::ALL)
                            .map(|(s, p)| {
                                let surface_global = (p + geo.loc - wg).to_f64();
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

        // 2. Layer surfaces take priority over space (launchers must intercept input).
        if let Some(hit) = self.layer_surface_under(pos) {
            return Some(hit);
        }

        // 3. Space elements.
        self.space
            .element_under(pos)
            .and_then(|(window, location)| {
                window
                    .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }

    fn layer_surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        use smithay::desktop::layer_map_for_output;
        use smithay::wayland::shell::wlr_layer::Layer;

        let output = self.space.outputs().next()?;
        let map = layer_map_for_output(output);

        for layer in [Layer::Overlay, Layer::Top, Layer::Bottom, Layer::Background] {
            if let Some(surface) = map.layer_under(layer, pos) {
                let Some(layer_geo) = map.layer_geometry(surface) else {
                    continue;
                };
                let local = pos - layer_geo.loc.to_f64();
                if let Some((wl_surface, offset)) =
                    surface.surface_under(local, WindowSurfaceType::ALL)
                {
                    return Some((wl_surface, (offset + layer_geo.loc).to_f64()));
                }
            }
        }

        None
    }
}

/// Resize and reposition the Emacs window in a given space.
/// Handles both Wayland (pgtk) and X11 (gtk3 via XWayland) paths.
pub fn resize_emacs_in_space(
    space: &mut Space<Window>,
    emacs_surface: &Option<WlSurface>,
    emacs_x11_window: &Option<Window>,
    geo: Rectangle<i32, Logical>,
) {
    // Wayland (pgtk) path.
    if let Some(ref emacs) = emacs_surface {
        let win = space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == emacs))
            .cloned();
        if let Some(window) = win {
            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|s| {
                    s.size = Some(geo.size);
                });
                toplevel.send_pending_configure();
            }
            space.map_element(window, geo.loc, false);
            return;
        }
    }
    // X11 (gtk3) path.
    if let Some(ref win) = emacs_x11_window {
        if let Some(x11) = win.x11_surface() {
            if let Err(e) = x11.configure(geo) {
                tracing::warn!("X11 Emacs resize failed: {e}");
            }
        }
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
