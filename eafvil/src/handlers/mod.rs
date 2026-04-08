mod compositor;
mod xdg_shell;

use crate::EafvilState;

//
// Wl Seat
//

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
use smithay::wayland::selection::SelectionHandler;
use smithay::{delegate_data_device, delegate_output, delegate_seat};

impl SeatHandler for EafvilState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<EafvilState> {
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
        set_data_device_focus(dh, seat, client);
    }
}

delegate_seat!(EafvilState);

//
// Wl Data Device
//

impl SelectionHandler for EafvilState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for EafvilState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for EafvilState {}
impl WaylandDndGrabHandler for EafvilState {
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

delegate_data_device!(EafvilState);

//
// Wl Output & Xdg Output
//

impl OutputHandler for EafvilState {}
delegate_output!(EafvilState);
