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
        pointer_constraints::PointerConstraintsState,
        relative_pointer::RelativePointerManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        selection::{
            ext_data_control::DataControlState as ExtDataControlState,
            wlr_data_control::DataControlState as WlrDataControlState,
        },
        shell::{
            wlr_layer::WlrLayerShellState,
            xdg::{decoration::XdgDecorationState, ToplevelSurface, XdgShellState},
        },
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};

use smithay::reexports::wayland_server::Resource;
use smithay::wayland::seat::WaylandFocus;

/// Tracks where the active selection came from, so paste requests are
/// routed to the correct data source.
///
/// - `Wayland`: a wayland client on emskin owns a data source that can
///   be pulled via `request_data_device_client_selection`. X clients
///   running under `xwayland-satellite` also fall into this variant —
///   satellite translates X selections into Wayland data sources before
///   they ever reach emskin.
/// - `Host`: emskin received the selection from the host compositor
///   via `inject_host_selection` and holds only an offer — actual data
///   must be pulled back from the host via `ClipboardProxy`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SelectionOrigin {
    #[default]
    Wayland,
    Host,
}

/// Focus-related state grouped together for clarity.
#[derive(Default)]
pub struct FocusState {
    /// Saved keyboard focus before a prefix key redirect (C-x, C-c, M-x).
    /// `Some(focus)` = prefix active, restore `focus` when done; `None` = normal.
    pub prefix_saved_focus: Option<Option<crate::KeyboardFocusTarget>>,
    /// Tracks text_input focus for manual enter/leave management. Kept as
    /// `WlSurface` because text_input_v3 is a Wayland-only protocol.
    pub text_input_focus: Option<WlSurface>,
    /// Deferred `set_ime_allowed` for the winit window.
    pub pending_ime_allowed: Option<bool>,
    /// Saved keyboard focus before a layer surface took it.
    pub layer_saved_focus: Option<crate::KeyboardFocusTarget>,
}

/// Clipboard/selection routing state grouped together.
#[derive(Default)]
pub struct SelectionState {
    /// Clipboard synchronization proxy (Wayland or X11 backend).
    pub clipboard: Option<Box<dyn emskin_clipboard::ClipboardBackend>>,
    /// Where the current clipboard selection came from.
    pub clipboard_origin: SelectionOrigin,
    /// Where the current primary selection came from.
    pub primary_origin: SelectionOrigin,
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
    pub cursor_shape_manager_state: CursorShapeManagerState,
    /// Advertise `zwlr_data_control_v1` and `ext_data_control_v1` to
    /// emskin's own internal clients so they can exchange selections
    /// without needing keyboard focus. Mirrors what real wlroots /
    /// cosmic / KDE ≥ 6.2 do for their clients, and lets tools like
    /// wl-copy / wl-paste inside emskin skip the wl_data_device focus
    /// dance entirely.
    pub wlr_data_control_state: WlrDataControlState,
    pub ext_data_control_state: ExtDataControlState,
    pub dmabuf_state: DmabufState,
    /// Keep-alive: dropping this removes the linux-dmabuf global from the display.
    pub dmabuf_global: Option<DmabufGlobal>,
    pub text_input_manager_state: smithay::wayland::text_input::TextInputManagerState,
    /// Exposes `zwp_pointer_constraints_v1` — lock/confine pointer for games
    /// (Minecraft, Blender, browser Pointer Lock).
    pub pointer_constraints_state: PointerConstraintsState,
    /// Exposes `zwp_relative_pointer_manager_v1` — delivers raw mouse deltas
    /// to clients that bind the protocol (required for FPS camera control).
    pub relative_pointer_manager_state: RelativePointerManagerState,
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

    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, EmskinState>,

    /// Winit graphics backend (renderer + window). Stored here so
    /// `DmabufHandler::dmabuf_imported` can access the renderer.
    pub backend: Option<WinitGraphicsBackend<GlesRenderer>>,

    // Smithay protocol state (grouped for clarity).
    pub wl: WaylandState,

    // XWayland: `xwls` owns the pre-bound X11 sockets and lazily
    // spawns an external `xwayland-satellite` process on X client
    // connect. `xdisplay` caches the `:N` number for convenience
    // (exported as DISPLAY, sent to Emacs via IPC).
    pub xdisplay: Option<u32>,
    pub xwls: Option<crate::xwayland_satellite::XwlsIntegration>,

    pub seat: Seat<Self>,

    // --- emskin specific ---
    /// The Emacs surface (first toplevel to connect)
    pub emacs_surface: Option<WlSurface>,

    /// Whether the initial size has been configured.
    /// Set to true once Emacs receives the host window size in its first configure.
    /// After this, host Resized events propagate size to Emacs.
    pub initial_size_settled: bool,

    /// When false, skip the "first toplevel == Emacs" heuristic and the
    /// "last Emacs frame died → stop" shutdown path. Set from the
    /// `EMSKIN_DISABLE_EMACS_DETECTION` env var at startup; intended
    /// for E2E tests that spawn transient Wayland clients (wl-copy,
    /// xclip, …) without a real Emacs ever attaching.
    pub detect_emacs: bool,

    /// Handle to the spawned Emacs process
    pub emacs_child: Option<std::process::Child>,

    /// Handle to the spawned emskin-bar process (None = `--bar=none` or the
    /// binary couldn't be located). Kept alive so it's reaped on shutdown.
    pub bar_child: Option<std::process::Child>,

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

    /// Registered overlays driven by effect-core's `EffectChain`.
    pub effect_chain: effect_core::EffectChain,

    /// Typed handles to each overlay — same instance is also registered in
    /// `effect_chain` via `EffectHandle`. Lets window-manager code (input
    /// routing, IPC dispatch, workspace-switch reset) call the overlay's
    /// typed setters directly without going through the trait.
    pub measure: std::rc::Rc<std::cell::RefCell<effect_plugins::measure::MeasureOverlay>>,
    pub skeleton: std::rc::Rc<std::cell::RefCell<effect_plugins::skeleton::SkeletonOverlay>>,
    pub splash: std::rc::Rc<std::cell::RefCell<effect_plugins::splash::SplashScreen>>,
    pub cursor_trail: std::rc::Rc<std::cell::RefCell<effect_plugins::cursor_trail::CursorTrail>>,
    pub jelly_cursor: std::rc::Rc<std::cell::RefCell<effect_plugins::jelly_cursor::JellyCursor>>,
    pub recorder_overlay:
        std::rc::Rc<std::cell::RefCell<effect_plugins::recorder::RecorderOverlay>>,
    pub key_cast: std::rc::Rc<std::cell::RefCell<effect_plugins::key_cast::KeyCastOverlay>>,

    /// Whether a skeleton label-click was swallowed — matching release must
    /// also be swallowed. Lives in the window manager, not the overlay.
    pub skeleton_click_absorbed: bool,

    /// Edge-detect latch for "Emacs just connected" → triggers `splash.dismiss()`
    /// exactly once. Initialised false; set to true the first frame Emacs's
    /// surface is present.
    pub last_emacs_connected: bool,

    /// Edge-detect latch for "recording state changed" → toggles
    /// `key_cast` overlay on/off so screencasts always show keystrokes
    /// without the user having to enable it separately.
    pub last_recording_active: bool,

    /// Current cursor image status. For Named, the host cursor is used;
    /// for Surface (GTK3/Emacs), the cursor is software-rendered each frame.
    pub cursor_status: CursorImageStatus,
    /// Set when cursor_status changes; consumed by apply_pending_state.
    pub cursor_changed: bool,

    /// Last raw absolute pointer location from the host, in compositor
    /// coords. Used to synthesize relative-motion deltas for
    /// `zwp_relative_pointer_v1` (required by FPS games) — the winit
    /// backend only emits absolute positions, so we diff consecutive
    /// absolutes to produce the delta. `None` on first event.
    pub last_pointer_raw_loc: Option<Point<f64, Logical>>,

    /// Coarse damage flag for structural events (IPC, layer shell, input,
    /// workspace switch) that smithay's per-element OutputDamageTracker does
    /// not cover.  When true the next Redraw calls render_frame; cleared after.
    pub needs_redraw: bool,

    /// Screenshot / screen-record state machine. Driven by IPC
    /// (`TakeScreenshot { path }`) and consumed by the winit render loop.
    pub recorder: crate::recording::Recorder,
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
        let cursor_shape_manager_state = CursorShapeManagerState::new::<Self>(&dh);
        let text_input_manager_state =
            smithay::wayland::text_input::TextInputManagerState::new::<Self>(&dh);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&dh);
        let relative_pointer_manager_state = RelativePointerManagerState::new::<Self>(&dh);
        let dmabuf_state = DmabufState::new();

        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        // Always expose DC to internal clients — internal clients
        // (Firefox, Electron apps, wl-clipboard, screen-grabs) prefer
        // DC over wl_data_device and therefore never need keyboard
        // focus to exchange selections with one another. Mirrors what
        // wlroots / cosmic / KDE ≥ 6.2 do for their clients.
        let wlr_data_control_state =
            WlrDataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true);
        let ext_data_control_state =
            ExtDataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_| true);

        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        seat.add_keyboard(xkb_config, 200, 25)
            .map_err(|e| format!("failed to initialize keyboard: {e:?}"))?;
        seat.add_pointer();

        let space = Space::default();
        let workspace_protocol = crate::protocols::workspace::WorkspaceProtocolState::new(&dh);

        let socket_name = Self::init_wayland_listener(display, event_loop)?;

        let loop_signal = event_loop.get_signal();

        // Overlays: same instance shared between the typed handle kept on
        // `EmskinState` (for input routing, IPC dispatch, workspace-switch
        // reset) and the `EffectHandle` wrapper registered into the chain
        // (for rendering). `register_overlay` does both in one step.
        let mut effect_chain = effect_core::EffectChain::default();
        let splash = register_overlay(
            &mut effect_chain,
            effect_plugins::splash::SplashScreen::new(),
        );
        let skeleton = register_overlay(
            &mut effect_chain,
            effect_plugins::skeleton::SkeletonOverlay::new(),
        );
        let measure = register_overlay(
            &mut effect_chain,
            effect_plugins::measure::MeasureOverlay::new(),
        );
        let cursor_trail = register_overlay(
            &mut effect_chain,
            effect_plugins::cursor_trail::CursorTrail::new(),
        );
        let jelly_cursor = register_overlay(
            &mut effect_chain,
            effect_plugins::jelly_cursor::JellyCursor::new(),
        );
        let recorder_overlay = register_overlay(
            &mut effect_chain,
            effect_plugins::recorder::RecorderOverlay::new(),
        );
        let key_cast = register_overlay(
            &mut effect_chain,
            effect_plugins::key_cast::KeyCastOverlay::new(),
        );

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
                cursor_shape_manager_state,
                wlr_data_control_state,
                ext_data_control_state,
                dmabuf_state,
                dmabuf_global: None,
                text_input_manager_state,
                pointer_constraints_state,
                relative_pointer_manager_state,
                popups,
            },
            xdisplay: None,
            xwls: None,
            seat,

            // emskin specific
            emacs_surface: None,
            initial_size_settled: false,
            detect_emacs: std::env::var_os("EMSKIN_DISABLE_EMACS_DETECTION").is_none(),
            emacs_child: None,
            bar_child: None,
            elisp_dir: None,
            pending_fullscreen: None,
            pending_maximize: None,
            emacs_title: None,
            emacs_app_id: None,
            pending_command: None,
            selection: SelectionState::default(),
            focus: FocusState::default(),
            effect_chain,
            measure,
            skeleton,
            splash,
            cursor_trail,
            jelly_cursor,
            recorder_overlay,
            key_cast,
            skeleton_click_absorbed: false,
            last_emacs_connected: false,
            last_recording_active: false,
            cursor_status: CursorImageStatus::default_named(),
            cursor_changed: false,
            last_pointer_raw_loc: None,
            needs_redraw: true,
            recorder: crate::recording::Recorder::new(),
        })
    }

    fn init_wayland_listener(
        display: Display<EmskinState>,
        event_loop: &mut EventLoop<Self>,
    ) -> Result<OsString, Box<dyn std::error::Error>> {
        // Pin the socket name when `--wayland-socket <NAME>` was passed on
        // the CLI (main.rs copies the flag value into this env var) or
        // when `EMSKIN_WAYLAND_SOCKET_NAME` is set directly. Used by E2E
        // tests so external Wayland clients (wl-copy, xclip, …) have a
        // predictable WAYLAND_DISPLAY. Otherwise fall through to `new_auto()`
        // which picks wayland-N.
        let listening_socket = match std::env::var_os("EMSKIN_WAYLAND_SOCKET_NAME") {
            Some(name) => ListeningSocketSource::with_name(&name.to_string_lossy())?,
            None => ListeningSocketSource::new_auto()?,
        };
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

    /// Translate an Emacs surface-local rect into canvas coordinates by
    /// adding the current usable-area origin. Used by every IPC geometry
    /// handler — a top-anchored layer surface (bar) shifts the origin and
    /// all rects must track.
    pub fn emacs_rect_to_canvas(&self, rect: crate::ipc::IpcRect) -> Rectangle<i32, Logical> {
        let crate::ipc::IpcRect { x, y, w, h } = rect;
        let origin = self.emacs_geometry().map(|g| g.loc).unwrap_or_default();
        Rectangle::new(
            smithay::utils::Point::from((x + origin.x, y + origin.y)),
            smithay::utils::Size::from((w, h)),
        )
    }

    /// Rect available for tiled clients (Emacs) after subtracting exclusive
    /// zones of anchored layer surfaces (e.g. the external workspace bar).
    /// Delegates to smithay's `LayerMap::non_exclusive_zone()`; falls back to
    /// full output when no layers or no output.
    pub fn usable_area(&self) -> Rectangle<i32, Logical> {
        let Some(output) = self.space.outputs().next() else {
            return Rectangle::default();
        };
        smithay::desktop::layer_map_for_output(output).non_exclusive_zone()
    }

    /// Geometry for Emacs frame — fills the non-exclusive zone.
    ///
    /// Returns `None` only when there's no output. Otherwise returns whatever
    /// `LayerMap` reports as the tiled-client region; external bars can claim
    /// space by setting `exclusive_zone` on their layer surfaces and the
    /// geometry adjusts automatically.
    pub fn emacs_geometry(&self) -> Option<Rectangle<i32, Logical>> {
        self.space.outputs().next()?;
        Some(self.usable_area())
    }

    /// The Emacs toplevel `Window`, looked up by its `wl_surface` in the
    /// active workspace `Space`. Under xwayland-satellite every client —
    /// including gtk3 Emacs over XWayland — presents as a Wayland
    /// toplevel, so there is no separate X11 branch.
    pub fn emacs_window(&self) -> Option<Window> {
        let surface = self.emacs_surface.as_ref()?;
        self.space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    /// The Emacs focus target.
    pub fn emacs_focus_target(&self) -> Option<crate::KeyboardFocusTarget> {
        self.emacs_window().map(crate::KeyboardFocusTarget::from)
    }

    /// Apply the window-manager's auto-focus policy when a new embedded
    /// toplevel maps: grant keyboard focus + notify Emacs — unless a
    /// prefix-key sequence is in flight (C-x / C-c / M-x).
    ///
    /// Mirrors sway's `view_map()` → `input_manager_set_focus()`
    /// pipeline. Single entry point for xdg_shell `new_toplevel`.
    pub fn auto_focus_new_window(&mut self, window: Window, window_id: u64) {
        let focus_view = crate::ipc::OutgoingMessage::FocusView {
            window_id,
            view_id: 0,
        };

        // Prefix sequence active: the user is typing C-x ... , any focus
        // steal would break the sequence. Still inform Emacs so its
        // buffer-level "focused window" tracking stays correct.
        if self.focus.prefix_saved_focus.is_some() {
            self.ipc.send(focus_view);
            return;
        }

        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(window.into()), serial);
        }
        self.ipc.send(focus_view);
    }

    /// Resolve a `wl_surface` to the keyboard focus target that owns it.
    /// Searches layer-shell surfaces, toplevels in the active `Space`, and
    /// tracked popups — returning the first hit in that order.
    pub fn focus_target_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<crate::KeyboardFocusTarget> {
        if let Some(output) = self.space.outputs().next() {
            let map = smithay::desktop::layer_map_for_output(output);
            if let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
                return Some(crate::KeyboardFocusTarget::from(layer.clone()));
            }
        }
        if let Some(window) = self
            .space
            .elements()
            .find(|w| w.wl_surface().as_deref().is_some_and(|s| s == surface))
            .cloned()
        {
            return Some(crate::KeyboardFocusTarget::from(window));
        }
        if let Some(popup) = self.wl.popups.find_popup(surface) {
            return Some(crate::KeyboardFocusTarget::from(popup));
        }
        None
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
        let old_name = std::mem::take(&mut self.active_workspace_name);
        self.inactive_workspaces.insert(
            self.active_workspace_id,
            Workspace {
                space: old_space,
                emacs_surface: old_emacs,
                name: old_name,
            },
        );

        self.space = target.space;
        self.emacs_surface = target.emacs_surface.take();
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
        // Reset skeleton state for the new workspace (window manager drives this,
        // not the effect trait).
        {
            let mut sk = self.skeleton.borrow_mut();
            sk.set_enabled(false);
            sk.clear();
        }
        self.skeleton_click_absorbed = false;
        // Reset caret tracking so the jelly overlay doesn't animate from
        // the previous workspace's caret position to the new one. The
        // new workspace's Emacs will send fresh SetCursorRect messages
        // after focus stabilizes.
        let now = self.start_time.elapsed();
        self.jelly_cursor.borrow_mut().update(None, now);

        if matches!(self.cursor_status, CursorImageStatus::Surface(_)) {
            self.cursor_status = CursorImageStatus::default_named();
            self.cursor_changed = true;
        }

        // Notify Emacs BEFORE changing keyboard focus. IPC is flushed
        // immediately (same syscall), while wl_keyboard.enter is buffered
        // until the next flush_clients(). This ensures Emacs updates
        // active-workspace-id before GTK's focus-change hooks fire,
        // preventing stale sync-all from sending wrong visibility/geometry.
        self.ipc
            .send(crate::ipc::OutgoingMessage::WorkspaceSwitched {
                workspace_id: target_id,
            });

        // Reset keyboard and pointer focus to the new workspace's Emacs.
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let emacs_target = self.emacs_focus_target();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, emacs_target, serial);
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

impl crate::xwayland_satellite::HasXwls for EmskinState {
    fn xwls_mut(&mut self) -> Option<&mut crate::xwayland_satellite::XwlsIntegration> {
        self.xwls.as_mut()
    }
}

impl EmskinState {
    /// Reposition + resize all Emacs frames (active + inactive workspaces) to
    /// match the current non-exclusive zone, and broadcast the new size to
    /// elisp. Call whenever layer surfaces claim/release space (new layer
    /// mapped, layer destroyed, exclusive_zone changed on commit).
    pub fn relayout_emacs(&mut self) {
        let Some(geo) = self.emacs_geometry() else {
            return;
        };
        tracing::debug!(
            "relayout_emacs: usable area ({},{}) {}x{}",
            geo.loc.x,
            geo.loc.y,
            geo.size.w,
            geo.size.h,
        );

        // Active workspace's Emacs surface lives in self.space.
        let active_emacs = self.emacs_surface.clone();
        resize_emacs_in_space(&mut self.space, &active_emacs, geo);

        // Inactive workspaces each hold their own space + Emacs.
        for ws in self.inactive_workspaces.values_mut() {
            resize_emacs_in_space(&mut ws.space, &ws.emacs_surface, geo);
        }

        // Tell Emacs its new surface size so elisp's sync path picks up the
        // new window-body dimensions. Wire format unchanged — Emacs only
        // cares about its own window size, not whether a bar sits above.
        self.ipc.send(crate::ipc::OutgoingMessage::SurfaceSize {
            width: geo.size.w,
            height: geo.size.h,
        });

        self.needs_redraw = true;
    }
}

/// Resize and reposition the Emacs window in a given space. Both pgtk
/// and gtk3 Emacs present as Wayland toplevels (gtk3 goes through
/// xwayland-satellite which translates X11 into Wayland before emskin
/// ever sees the client), so there is a single code path here.
pub fn resize_emacs_in_space(
    space: &mut Space<Window>,
    emacs_surface: &Option<WlSurface>,
    geo: Rectangle<i32, Logical>,
) {
    let Some(ref emacs) = emacs_surface else {
        return;
    };
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
        space.map_element(window.clone(), geo.loc, false);
        // smithay's `map_element` removes + re-appends, pushing Emacs to
        // the top of the stack every time. Since Emacs is fullscreen host,
        // that would cover every embedded app (visible as a white screen
        // on rapid host resize). Keep Emacs at the bottom so apps stay on
        // top without per-app raise.
        space.lower_element(&window);
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

/// Register an overlay into the chain and return a typed handle to the same
/// instance. Lets `EmskinState::new` construct each overlay with one line.
fn register_overlay<T: effect_core::Effect + 'static>(
    chain: &mut effect_core::EffectChain,
    value: T,
) -> std::rc::Rc<std::cell::RefCell<T>> {
    let rc = std::rc::Rc::new(std::cell::RefCell::new(value));
    chain.register(effect_core::EffectHandle::new(rc.clone()));
    rc
}
