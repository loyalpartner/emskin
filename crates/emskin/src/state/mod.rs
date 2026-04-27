pub mod apps;
pub mod cursor;
pub mod dbus;
pub mod effects;
pub mod emacs;
pub mod focus;
pub mod ime;
pub mod workspace;
pub mod xwayland;

// Type re-exports for common shorthands (kept for historical call sites
// that used `crate::KeyboardFocusTarget` before state/ existed).
pub use focus::KeyboardFocusTarget;

use std::{collections::HashMap, ffi::OsString, sync::Arc};

use smithay::{
    backend::{renderer::gles::GlesRenderer, winit::WinitGraphicsBackend},
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
            xdg::{decoration::XdgDecorationState, XdgShellState},
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

/// Kind of focus override currently in effect. Multiple may be active
/// concurrently (e.g. user Alt+Tab away from emskin while in middle of
/// an Emacs prefix chord).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusOverride {
    /// Emacs C-x / C-c / M-x chord redirected focus to Emacs.
    Prefix,
    /// A layer-shell client (zofi / wofi / rofi) grabbed focus.
    Layer,
    /// Emskin's window itself lost host-level focus (Alt+Tab away).
    Host,
}

/// Focus-override state — one saved focus per active override kind.
///
/// Replaces three independent `Option<KeyboardFocusTarget>` slots with
/// a typed `enter`/`exit`/`is_active` API so callers don't manipulate
/// raw fields. Saved focus is itself `Option` because there may not
/// have been a focused surface at the moment the override fired.
#[derive(Default)]
pub struct FocusState {
    saves: std::collections::HashMap<FocusOverride, Option<crate::KeyboardFocusTarget>>,
}

impl FocusState {
    /// Save `current` as the focus to restore when this override exits.
    /// **Always overwrites.** Callers that need idempotence (the
    /// `prefix_done`-may-be-lost guard in input.rs) must check
    /// `is_active(kind)` first.
    pub fn enter(&mut self, kind: FocusOverride, current: Option<crate::KeyboardFocusTarget>) {
        self.saves.insert(kind, current);
    }

    /// Exit override; returns saved focus to restore. Outer `Some`
    /// means the override was active; inner `Option` is the actual
    /// saved focus (which itself may be `None` if nothing was focused
    /// when the override fired).
    pub fn exit(&mut self, kind: FocusOverride) -> Option<Option<crate::KeyboardFocusTarget>> {
        self.saves.remove(&kind)
    }

    pub fn is_active(&self, kind: FocusOverride) -> bool {
        self.saves.contains_key(&kind)
    }

    /// Clear every saved-focus slot. Called on workspace switch: the
    /// saved targets may reference surfaces in the departing workspace,
    /// which become stale the moment `switch_workspace` swaps the
    /// active `Space`. Without this, an Alt+Tab-away → workspace-switch
    /// → Alt+Tab-back sequence would restore focus to a surface in the
    /// now-inactive workspace (sending `wl_keyboard.enter` to an
    /// unmapped client).
    pub fn reset_on_workspace_switch(&mut self) {
        self.saves.clear();
    }
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
    /// Exposes `zwp_pointer_constraints_v1` — lock/confine pointer for games
    /// (Minecraft, Blender, browser Pointer Lock).
    pub pointer_constraints_state: PointerConstraintsState,
    /// Exposes `zwp_relative_pointer_manager_v1` — delivers raw mouse deltas
    /// to clients that bind the protocol (required for FPS camera control).
    pub relative_pointer_manager_state: RelativePointerManagerState,
    pub popups: PopupManager,
}

/// Re-export so pre-extraction call sites (main.rs spawning the
/// `--command` child) keep compiling without qualifying the path.
pub use xwayland::PendingCommand;

pub struct EmskinState {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub ipc: crate::ipc::IpcServer,
    pub apps: crate::apps::AppManager,

    /// Workspace model: active + inactive Emacs frames.
    pub workspace: crate::workspace::WorkspaceState,

    pub loop_signal: LoopSignal,
    pub loop_handle: LoopHandle<'static, EmskinState>,

    /// Winit graphics backend (renderer + window). Stored here so
    /// `DmabufHandler::dmabuf_imported` can access the renderer.
    pub backend: Option<WinitGraphicsBackend<GlesRenderer>>,

    // Smithay protocol state (grouped for clarity).
    pub wl: WaylandState,

    /// XWayland supervisor state — display number, xwayland-satellite
    /// integration handle, and the `--command` deferred-spawn mailbox.
    pub xwayland: xwayland::XwaylandState,

    pub seat: Seat<Self>,

    // --- emskin specific ---
    /// Emacs host-process state — main surface, child process, title /
    /// app_id metadata, detection and size-settle latches, plus the
    /// `request_fullscreen` / `request_maximize` mailboxes the Emacs
    /// toplevel populates via `xdg_toplevel.set_fullscreen` etc.
    pub emacs: emacs::EmacsState,

    /// Handle to the spawned emskin-bar process (None = `--bar=none` or the
    /// binary couldn't be located). Kept alive so it's reaped on shutdown.
    pub bar_child: Option<std::process::Child>,

    /// Path to extracted elisp dir (for cleanup on exit).
    pub elisp_dir: Option<std::path::PathBuf>,

    /// Clipboard/selection routing state.
    pub selection: SelectionState,

    /// Focus management state.
    pub focus: FocusState,

    /// IME (text_input_v3) bridge — host IME ↔ embedded Wayland clients.
    pub ime: crate::ime::ImeBridge,

    /// Built-in visual overlays + their host-side edge-detect flags.
    /// Grouped so the render loop and workspace-switch reset both see
    /// a single cohesive module instead of eleven flat fields.
    pub effects: effects::EffectsState,

    /// Cursor image tracking (Named / Surface) + raw pointer location
    /// for `zwp_relative_pointer_v1` delta synthesis.
    pub cursor: cursor::CursorState,

    /// Coarse damage flag for structural events (IPC, layer shell, input,
    /// workspace switch) that smithay's per-element OutputDamageTracker does
    /// not cover.  When true the next Redraw calls render_frame; cleared after.
    pub needs_redraw: bool,

    /// Screenshot / screen-record state machine. Driven by IPC
    /// (`TakeScreenshot { path }`) and consumed by the winit render loop.
    pub recorder: crate::recording::Recorder,

    /// Bridge to the child `emskin-dbus-proxy` process that rewrites IME
    /// cursor-position calls for embedded apps. Populated in `main.rs`
    /// after `init_winit`; stays [`DbusBridge::default`] (inert) if the
    /// proxy binary is missing or the host has no session bus.
    pub dbus: dbus::DbusBridge,
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
        let ime = crate::ime::ImeBridge::new(&dh);
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

        Ok(Self {
            start_time,
            display_handle: dh,

            ipc,
            apps: crate::apps::AppManager::default(),

            workspace: crate::workspace::WorkspaceState {
                active_space: space,
                inactive: HashMap::new(),
                active_id: 1,
                active_name: String::new(),
                next_id: 2,
                pending_emacs_toplevels: Vec::new(),
                protocol: workspace_protocol,
            },

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
                pointer_constraints_state,
                relative_pointer_manager_state,
                popups,
            },
            xwayland: xwayland::XwaylandState::default(),
            seat,

            // emskin specific
            emacs: emacs::EmacsState::new(
                std::env::var_os("EMSKIN_DISABLE_EMACS_DETECTION").is_none(),
            ),
            bar_child: None,
            elisp_dir: None,
            selection: SelectionState::default(),
            focus: FocusState::default(),
            ime,
            effects: effects::EffectsState::default(),
            cursor: cursor::CursorState::default(),
            needs_redraw: true,
            recorder: crate::recording::Recorder::new(),
            dbus: dbus::DbusBridge::default(),
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
        let output = self.workspace.active_space.outputs().next()?;
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
        let Some(output) = self.workspace.active_space.outputs().next() else {
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
        self.workspace.active_space.outputs().next()?;
        Some(self.usable_area())
    }

    /// The Emacs toplevel `Window`, looked up by its `wl_surface` in the
    /// active workspace `Space`. Under xwayland-satellite every client —
    /// including gtk3 Emacs over XWayland — presents as a Wayland
    /// toplevel, so there is no separate X11 branch.
    pub fn emacs_window(&self) -> Option<Window> {
        let surface = self.emacs.surface()?;
        self.workspace
            .active_space
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
        if self.focus.is_active(FocusOverride::Prefix) {
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
        if let Some(output) = self.workspace.active_space.outputs().next() {
            let map = smithay::desktop::layer_map_for_output(output);
            if let Some(layer) = map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL) {
                return Some(crate::KeyboardFocusTarget::from(layer.clone()));
            }
        }
        if let Some(window) = self
            .workspace
            .active_space
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
        if old_ws == self.workspace.active_id {
            return false;
        }
        let window = app.window.clone();
        tracing::debug!(
            "app {window_id} migrating workspace {old_ws} → {}",
            self.workspace.active_id
        );
        if let Some(old_space) = self.workspace.space_for_mut(old_ws) {
            old_space.unmap_elem(&window);
        }
        if let Some(app) = self.apps.get_mut(window_id) {
            app.workspace_id = self.workspace.active_id;
            // Reset geometry so the next set_geometry immediately maps the app
            // instead of going through the pending path (which would deadlock:
            // app needs frame callbacks to commit, but it's not in any Space).
            app.geometry = None;
            app.pending_geometry = None;
            app.pending_since = None;
        }
        true
    }

    /// Check if a surface belongs to the same Wayland client as the active Emacs.
    pub fn is_emacs_client(&self, surface: &WlSurface) -> bool {
        self.emacs
            .surface()
            .is_some_and(|emacs| emacs.same_client_as(&surface.id()))
    }

    /// Check if a surface is any workspace's Emacs surface (active or inactive).
    pub fn is_any_emacs_surface(&self, surface: &WlSurface) -> bool {
        if self.emacs.is_main_surface(surface) {
            return true;
        }
        self.workspace
            .inactive
            .values()
            .any(|ws| ws.emacs_surface.as_ref() == Some(surface))
    }

    /// Switch the active workspace. Returns false if target is already active
    /// or doesn't exist.
    pub fn switch_workspace(&mut self, target_id: u64) -> bool {
        if target_id == self.workspace.active_id {
            return false;
        }
        let Some(mut target) = self.workspace.inactive.remove(&target_id) else {
            return false;
        };

        // Swap: current active → inactive, target → active.
        let old_space = std::mem::take(&mut self.workspace.active_space);
        let old_emacs = self.emacs.take_surface();
        let old_name = std::mem::take(&mut self.workspace.active_name);
        self.workspace.inactive.insert(
            self.workspace.active_id,
            crate::workspace::Workspace {
                space: old_space,
                emacs_surface: old_emacs,
                name: old_name,
            },
        );

        self.workspace.active_space = target.space;
        self.emacs.set_surface(target.emacs_surface.take());
        self.workspace.active_name = target.name;
        self.workspace.active_id = target_id;

        // App migration is handled by IPC set_geometry from Emacs (sync-all).
        // The compositor does NOT auto-migrate because it doesn't know which
        // apps are displayed in which Emacs frame.

        // Reset state that references the old workspace's surfaces.
        // `host_saved_focus` MUST be cleared alongside the prefix/layer
        // slots — otherwise an Alt+Tab-away → workspace-switch → Alt+Tab-
        // back sequence restores focus to a surface in the now-inactive
        // workspace (sending `wl_keyboard.enter` to an unmapped client).
        // Centralising this in `FocusState::reset_on_workspace_switch`
        // makes future field additions self-documenting.
        self.focus.reset_on_workspace_switch();
        self.ime.reset_on_workspace_switch();
        self.effects
            .reset_on_workspace_switch(self.start_time.elapsed());

        self.cursor.reset_on_workspace_switch();

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
            self.workspace.count()
        );
        true
    }

    /// Remove an inactive workspace and its embedded apps.
    pub fn destroy_workspace(&mut self, workspace_id: u64) -> Option<crate::workspace::Workspace> {
        let ws = self.workspace.inactive.remove(&workspace_id)?;
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
            self.workspace.count()
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
            self.apps.mirror_under(pos, self.workspace.active_id)
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
        self.workspace
            .active_space
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

        let output = self.workspace.active_space.outputs().next()?;
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
        self.xwayland.integration_mut()
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

        // Active workspace's Emacs surface lives in self.workspace.active_space.
        let active_emacs = self.emacs.surface().cloned();
        resize_emacs_in_space(&mut self.workspace.active_space, &active_emacs, geo);

        // Inactive workspaces each hold their own space + Emacs.
        for ws in self.workspace.inactive.values_mut() {
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

#[cfg(test)]
mod focus_state_tests {
    use super::*;

    // KeyboardFocusTarget wraps smithay types that need a live Wayland
    // Display+Client to construct, so we test with `None` saved-focus
    // (which is itself a valid sentinel: "override active, nothing was
    // focused at the moment it fired").

    #[test]
    fn default_has_no_active_overrides() {
        let f = FocusState::default();
        assert!(!f.is_active(FocusOverride::Prefix));
        assert!(!f.is_active(FocusOverride::Layer));
        assert!(!f.is_active(FocusOverride::Host));
    }

    #[test]
    fn enter_then_is_active() {
        let mut f = FocusState::default();
        f.enter(FocusOverride::Prefix, None);
        assert!(f.is_active(FocusOverride::Prefix));
        assert!(!f.is_active(FocusOverride::Layer));
    }

    #[test]
    fn exit_returns_saved_and_clears() {
        let mut f = FocusState::default();
        f.enter(FocusOverride::Prefix, None);
        let saved = f.exit(FocusOverride::Prefix);
        assert_eq!(saved, Some(None), "exit returns the saved Option");
        assert!(!f.is_active(FocusOverride::Prefix));
    }

    #[test]
    fn exit_inactive_returns_none() {
        let mut f = FocusState::default();
        assert_eq!(f.exit(FocusOverride::Prefix), None);
    }

    #[test]
    fn reset_clears_all_overrides() {
        let mut f = FocusState::default();
        f.enter(FocusOverride::Prefix, None);
        f.enter(FocusOverride::Layer, None);
        f.enter(FocusOverride::Host, None);
        f.reset_on_workspace_switch();
        assert!(!f.is_active(FocusOverride::Prefix));
        assert!(!f.is_active(FocusOverride::Layer));
        assert!(!f.is_active(FocusOverride::Host));
    }

    #[test]
    fn overrides_stack_independently() {
        let mut f = FocusState::default();
        f.enter(FocusOverride::Prefix, None);
        f.enter(FocusOverride::Host, None);
        // Exit prefix; host stays.
        f.exit(FocusOverride::Prefix);
        assert!(!f.is_active(FocusOverride::Prefix));
        assert!(f.is_active(FocusOverride::Host));
    }
}
