//! All Wayland protocol handlers for `Emez`.
//!
//! Kept as a single module because most handlers are stub-grade — emez
//! just advertises the globals, accepts clients, and lets smithay do the
//! heavy lifting. The interesting bits are:
//!
//! - `CompositorHandler::commit` runs the buffer handler so clients
//!   that commit a surface don't get stuck.
//! - `XdgShellHandler::new_toplevel` sends an initial configure so
//!   `emskin`'s winit-wayland backend can finish its handshake.
//! - `data_control` + `primary_selection` + `data_device` delegates are
//!   registered so the clipboard machinery works end-to-end.

use std::os::unix::io::OwnedFd;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_control, delegate_data_device, delegate_ext_data_control,
    delegate_output, delegate_primary_selection, delegate_seat, delegate_shm, delegate_xdg_shell,
    input::{
        dnd::{DndGrabHandler, GrabType, Source},
        Seat, SeatHandler, SeatState,
    },
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
        Client, Resource,
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        output::OutputHandler,
        selection::{
            data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler},
            ext_data_control::{
                DataControlHandler as ExtDataControlHandler,
                DataControlState as ExtDataControlState,
            },
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
            wlr_data_control::{
                DataControlHandler as WlrDataControlHandler,
                DataControlState as WlrDataControlState,
            },
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
    xwayland::XWaylandClientData,
};

use crate::state::{ClientState, Emez};

impl CompositorHandler for Emez {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland clients carry their own CompositorClientState on
        // `XWaylandClientData`; all others use emez's ClientState.
        if let Some(xdata) = client.get_data::<XWaylandClientData>() {
            return &xdata.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        // emez does no rendering, but winit-backed clients (e.g. emskin)
        // block their render loop on frame callbacks. Fire them back
        // immediately so clients keep producing frames — this unblocks
        // capture/recording tests that depend on at least one rendered
        // frame reaching the emskin-side GPU readback path.
        use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
        let now = std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_millis() as u32)
            .unwrap_or(0);
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_surface, states, _| {
                states
                    .cached_state
                    .get::<smithay::wayland::compositor::SurfaceAttributes>()
                    .current()
                    .frame_callbacks
                    .drain(..)
                    .for_each(|cb| {
                        cb.done(now);
                    });
            },
            |_, _, _| true,
        );
    }
}

impl BufferHandler for Emez {
    fn buffer_destroyed(&mut self, _: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Emez {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Emez {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // Mirror keyboard focus into the data_device + primary_selection
        // focus state. Without this, smithay's SeatData doesn't know which
        // client to broadcast `.selection(new_offer)` events to — so
        // emskin's `wl_data_device` never sees host selection changes
        // even though the handoff in `new_selection` puts the keyboard
        // back on emskin's surface.
        use smithay::wayland::selection::{
            data_device::set_data_device_focus, primary_selection::set_primary_focus,
        };
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }
}

impl OutputHandler for Emez {}

impl XdgShellHandler for Emez {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Send an immediate configure so winit-backed clients like emskin
        // get past their first round-trip. Size is advertised by the
        // advertised output (1920x1080) but clients can pick their own.
        surface.send_configure();

        // Focus handoff dance to satisfy smithay's
        // `Request::SetSelection` focus check on `wl_data_device`:
        //   1. First toplevel = "primary" (emskin's winit window).
        //   2. Every new toplevel gets focus transiently so transient
        //      `wl_data_device` clients (e.g. wl-copy on no-data-control
        //      hosts) can call `set_selection` legally.
        //   3. After the client fires `set_selection`, we return focus
        //      to primary in `SelectionHandler::new_selection`, which
        //      is where emskin ends up receiving the fresh offer.
        let wl_surface = surface.wl_surface().clone();
        if self.primary_toplevel.is_none() {
            self.primary_toplevel = Some(wl_surface.clone());
        }
        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(wl_surface), serial);
        }
    }

    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}
    fn grab(&mut self, _: PopupSurface, _: wl_seat::WlSeat, _: Serial) {}
    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}

impl DataDeviceHandler for Emez {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for Emez {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        _seat: smithay::input::Seat<Self>,
        _serial: Serial,
        _type_: GrabType,
    ) {
        // emez is a dumb host — we never accept DnD on behalf of any client.
        source.cancel();
    }
}

impl DndGrabHandler for Emez {}

impl SelectionHandler for Emez {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        // Forward wayland-side selection changes to XWayland so outside
        // X clients (xclip) see them. No-op when XWayland isn't running.
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, source.map(|s| s.mime_types())) {
                tracing::warn!(?err, ?ty, "emez: forward wayland → X new_selection");
            }
        }

        // Focus handoff: a transient wl_data_device client (e.g. wl-copy
        // on a no-data-control host) just set a selection while holding
        // focus. Return focus to the primary toplevel (emskin) so the
        // `set_data_device_focus(primary)` call in `SeatHandler::focus_changed`
        // updates smithay's SeatData clipboard focus, then
        // `set_clipboard_selection` broadcasts the fresh
        // `.selection(new_offer)` to emskin's wl_data_device. Without
        // this handoff the selection stays stranded on the transient
        // client and emskin never sees it.
        if ty == SelectionTarget::Clipboard {
            if let Some(primary) = self.primary_toplevel.clone() {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    if keyboard.current_focus().as_ref() != Some(&primary) {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(primary), serial);
                    }
                }
            }
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                tracing::warn!(?err, ?ty, "emez: forward wayland → X send_selection");
            }
        }
    }
}

impl PrimarySelectionHandler for Emez {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl WlrDataControlHandler for Emez {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.wlr_data_control_state
    }
}

impl ExtDataControlHandler for Emez {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

delegate_compositor!(Emez);
delegate_shm!(Emez);
delegate_seat!(Emez);
delegate_output!(Emez);
delegate_xdg_shell!(Emez);
delegate_data_device!(Emez);
delegate_primary_selection!(Emez);
delegate_data_control!(Emez);
delegate_ext_data_control!(Emez);
