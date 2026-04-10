mod compositor;
mod xdg_activation;
mod xdg_shell;
mod xwayland;

use crate::EmskinState;

//
// Wl Seat
//

use std::os::fd::OwnedFd;

use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Serial;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::{delegate_data_device, delegate_output, delegate_primary_selection, delegate_seat};

impl SeatHandler for EmskinState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<EmskinState> {
        &mut self.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

delegate_seat!(EmskinState);

//
// Wl Data Device
//

impl SelectionHandler for EmskinState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        let Some(ref mut clipboard) = self.clipboard else {
            return;
        };
        if let Some(source) = source {
            let mime_types = source.mime_types();

            // Skip the first selection set per target — Emacs/GTK initializes
            // clipboard on startup which would override the host's clipboard.
            let init_done = match ty {
                SelectionTarget::Clipboard => &mut self.clipboard_init_done,
                SelectionTarget::Primary => &mut self.primary_init_done,
            };
            if !*init_done {
                *init_done = true;
                tracing::debug!("Skipping initial {ty:?} selection (startup)");
                return;
            }

            tracing::debug!("Internal selection set ({ty:?}): {mime_types:?}");
            clipboard.set_host_selection(ty, &mime_types);
        } else {
            tracing::debug!("Internal selection cleared ({ty:?})");
            clipboard.clear_host_selection(ty);
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
        // Internal client wants to paste our compositor-injected (host) selection.
        // Forward the fd directly to the host so the host source writes into it.
        if let Some(ref mut clipboard) = self.clipboard {
            tracing::debug!("Forwarding host selection to internal client ({ty:?}, {mime_type})");
            clipboard.receive_from_host(ty, &mime_type, fd);
        }
    }
}

impl DataDeviceHandler for EmskinState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for EmskinState {}
impl WaylandDndGrabHandler for EmskinState {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let Some(ptr) = seat.get_pointer() else {
                    source.cancel();
                    return;
                };
                let Some(start_data) = ptr.grab_start_data() else {
                    source.cancel();
                    return;
                };
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => {
                source.cancel();
            }
        }
    }
}

delegate_data_device!(EmskinState);

//
// Primary Selection
//

impl PrimarySelectionHandler for EmskinState {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

delegate_primary_selection!(EmskinState);

//
// Wl Output & Xdg Output
//

impl OutputHandler for EmskinState {}
delegate_output!(EmskinState);
