//! `BarState` — single-threaded Wayland client state driven by SCTK delegates.
//!
//! Owns every Wayland handle the bar cares about (compositor / shm /
//! layer-shell / seat / output / workspace manager), the workspace list fed
//! by `ext-workspace-v1`, and the current layer surface (if any). Visibility
//! is decided from `workspaces.len()` every time the list transitions.

use cosmic_text::{FontSystem, SwashCache};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::{BindError, GlobalList},
    protocol::{wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, QueueHandle,
};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;

use crate::render::{BAR_HEIGHT, PILL_H_PAD};
use crate::workspace::WorkspaceEntry;

pub struct BarState {
    // --- SCTK delegate state ---
    pub(crate) registry_state: RegistryState,
    pub(crate) output_state: OutputState,
    pub(crate) seat_state: SeatState,
    pub(crate) compositor: CompositorState,
    pub(crate) shm: Shm,
    pub(crate) layer_shell: LayerShell,
    pub(crate) pool: SlotPool,

    // --- Seat / output ---
    pub(crate) pointer: Option<wl_pointer::WlPointer>,

    // --- Text rendering ---
    pub(crate) font_system: FontSystem,
    pub(crate) swash_cache: SwashCache,

    // --- Workspace protocol ---
    /// Manager global. Always live as long as the compositor advertises it.
    pub(crate) workspace_manager: ExtWorkspaceManagerV1,
    /// Workspace entries we've collected so far in this round — promoted to
    /// `workspaces` on the next `done` event.
    pub(crate) pending_workspaces: Vec<WorkspaceEntry>,
    /// Committed snapshot. Visibility decision reads from here.
    pub(crate) workspaces: Vec<WorkspaceEntry>,

    // --- Layer surface (present iff workspaces.len() >= 2) ---
    pub(crate) layer: Option<LayerSurface>,
    /// Current surface size (buffer pixels). Set by `configure`.
    pub(crate) surface_size: Option<(u32, u32)>,
    pub(crate) configured_once: bool,

    // --- Lifecycle ---
    pub(crate) exit: bool,
}

impl BarState {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<Self>) -> Result<Self, BindError> {
        let registry_state = RegistryState::new(globals);
        let output_state = OutputState::new(globals, qh);
        let seat_state = SeatState::new(globals, qh);
        let compositor = CompositorState::bind(globals, qh)?;
        let shm = Shm::bind(globals, qh)?;
        let layer_shell = LayerShell::bind(globals, qh)?;

        // 16 KiB initial — grown by SlotPool as needed when configure tells us
        // the real dimensions.
        let pool = SlotPool::new(BAR_HEIGHT as usize * 1024 * 4, &shm)
            .expect("SlotPool::new failed — shm format argb8888 must be supported");

        // Bind ext-workspace-v1 manager eagerly; if it's not advertised the
        // bar has no reason to exist.
        let workspace_manager = globals.bind::<ExtWorkspaceManagerV1, _, _>(qh, 1..=1, ())?;

        Ok(Self {
            registry_state,
            output_state,
            seat_state,
            compositor,
            shm,
            layer_shell,
            pool,
            pointer: None,
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            workspace_manager,
            pending_workspaces: Vec::new(),
            workspaces: Vec::new(),
            layer: None,
            surface_size: None,
            configured_once: false,
            exit: false,
        })
    }

    pub fn exit_requested(&self) -> bool {
        self.exit
    }

    // ---------------------------------------------------------------------
    // Visibility policy
    // ---------------------------------------------------------------------

    /// Decide whether a layer surface should exist right now; map or unmap
    /// accordingly. Called after every committed workspace snapshot.
    ///
    /// Redraws are issued directly rather than flagging for a frame
    /// callback: callbacks are only scheduled by a prior draw(), so if we
    /// ever skip drawing we stop getting woken up. Commit immediately and
    /// let the compositor pace us — configure covers the not-yet-sized case.
    pub(crate) fn update_visibility(&mut self, qh: &QueueHandle<Self>) {
        let should_show = self.workspaces.len() >= 2;
        match (self.layer.is_some(), should_show) {
            (false, true) => self.create_layer(qh),
            (true, false) => self.destroy_layer(),
            (true, true) if self.configured_once => self.draw(),
            (true, true) => {
                // Not configured yet — the first configure will draw.
            }
            (false, false) => {}
        }
    }

    fn create_layer(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("emskin-bar"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, BAR_HEIGHT);
        layer.set_exclusive_zone(BAR_HEIGHT as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.layer = Some(layer);
        self.configured_once = false;
    }

    fn destroy_layer(&mut self) {
        // Dropping the LayerSurface destroys it on the server.
        self.layer = None;
        self.surface_size = None;
        self.configured_once = false;
    }

    // ---------------------------------------------------------------------
    // Click → ext_workspace_handle.activate → manager.commit
    // ---------------------------------------------------------------------

    fn handle_click(&mut self, pos: (f64, f64)) {
        let Some(hit) = hit_test(&self.workspaces, pos) else {
            return;
        };
        tracing::debug!("click activating workspace id={}", hit.id);
        hit.handle.activate();
        self.workspace_manager.commit();
    }
}

// -------------------------------------------------------------------------
// Hit-test: uses the cached hit_rect from the last render.
// -------------------------------------------------------------------------

fn hit_test(workspaces: &[WorkspaceEntry], pos: (f64, f64)) -> Option<&WorkspaceEntry> {
    let (px, py) = pos;
    workspaces.iter().find(|ws| {
        let (x, y, w, h) = ws.hit_rect;
        let in_x = px >= x as f64 && px < (x + w) as f64;
        // Y tolerance — layer-shell buffer origin is at 0. Keep a loose
        // check so a click anywhere inside the pill row lands.
        let in_y = py >= (y - PILL_H_PAD) as f64 && py < (y + h + PILL_H_PAD) as f64;
        in_x && in_y
    })
}

// =========================================================================
// SCTK delegates
// =========================================================================

impl CompositorHandler for BarState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Frame callbacks are pacing hints only; we draw on state changes.
        // Intentionally empty — re-drawing here would double-render and
        // skipping draw() elsewhere would strand the redraw cycle.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for BarState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for BarState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.pointer = Some(pointer);
            }
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for BarState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // Only react to events on our own surface.
            if let Some(layer) = &self.layer {
                if &event.surface != layer.wl_surface() {
                    continue;
                }
            } else {
                continue;
            }
            // 0x110 = BTN_LEFT from linux/input-event-codes.h.
            if let PointerEventKind::Press { button: 0x110, .. } = event.kind {
                self.handle_click(event.position);
            }
        }
    }
}

impl LayerShellHandler for BarState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        // Compositor dismissed our surface — drop state and either wait for
        // workspace count to drop below 2 or recreate on the next `done`.
        self.layer = None;
        self.surface_size = None;
        self.configured_once = false;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        let w = if w == 0 { 1 } else { w };
        let h = if h == 0 { BAR_HEIGHT } else { h };
        self.surface_size = Some((w, h));
        self.configured_once = true;
        // Always redraw on configure — the compositor is waiting for us to
        // attach a buffer of the configured size.
        self.draw();
    }
}

impl ShmHandler for BarState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for BarState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(BarState);
delegate_output!(BarState);
delegate_shm!(BarState);
delegate_seat!(BarState);
delegate_pointer!(BarState);
delegate_layer!(BarState);
delegate_registry!(BarState);
