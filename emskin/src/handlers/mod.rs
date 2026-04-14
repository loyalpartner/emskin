mod compositor;
mod dmabuf;
mod layer_shell;
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
        &mut self.wl.seat_state
    }

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        image: smithay::input::pointer::CursorImageStatus,
    ) {
        self.cursor_status = image;
        self.cursor_changed = true;
        self.needs_redraw = true;
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);

        // Bridge text_input enter/leave — smithay's keyboard handler
        // gates these behind has_instance() which is always false here.
        use smithay::wayland::text_input::TextInputSeat;
        let ti = seat.text_input();
        let old = self.focus.text_input_focus.take();
        let new = focused.cloned();
        if old.as_ref() != new.as_ref() {
            if old.is_some() {
                ti.set_focus(old);
                ti.leave();
            }
            ti.set_focus(new.clone());
            if new.is_some() {
                ti.enter();
            }
        }
        self.focus.text_input_focus = new;

        // Only enable host IME when the focused client has bound text_input_v3.
        // Apps using their own IM module (fcitx5-gtk via DBus) don't bind it
        // and need raw keyboard events from wl_keyboard instead.
        let mut has_ti = false;
        ti.with_focused_text_input(|_, _| {
            has_ti = true;
        });
        if self.focus.pending_ime_allowed != Some(has_ti) {
            self.focus.pending_ime_allowed = Some(has_ti);
        }
    }
}

delegate_seat!(EmskinState);
smithay::delegate_text_input_manager!(EmskinState);

impl smithay::wayland::tablet_manager::TabletSeatHandler for EmskinState {}
smithay::delegate_cursor_shape!(EmskinState);

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
        let Some(ref mut clipboard) = self.selection.clipboard else {
            return;
        };
        if let Some(source) = source {
            let mime_types = source.mime_types();

            let ipc_connected = self.ipc.is_connected();
            tracing::debug!(
                "selection {ty:?}: ipc={ipc_connected} mimes={mime_types:?} age={:.1}s",
                self.start_time.elapsed().as_secs_f32(),
            );

            // Skip selections that arrive before Emacs IPC connects —
            // GTK/Emacs announces clipboard ownership on startup which
            // would clear the host clipboard. Real user copies only
            // happen after emskin.el connects.
            if !ipc_connected {
                return;
            }
            match ty {
                SelectionTarget::Clipboard => {
                    self.selection.clipboard_origin = crate::state::SelectionOrigin::Wayland
                }
                SelectionTarget::Primary => {
                    self.selection.primary_origin = crate::state::SelectionOrigin::Wayland
                }
            }
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
        if let Some(ref mut clipboard) = self.selection.clipboard {
            tracing::debug!("Forwarding host selection to internal client ({ty:?}, {mime_type})");
            clipboard.receive_from_host(ty, &mime_type, fd);
        }
    }
}

impl DataDeviceHandler for EmskinState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.wl.data_device_state
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
        &mut self.wl.primary_selection_state
    }
}

delegate_primary_selection!(EmskinState);

//
// Wl Output & Xdg Output
//

impl OutputHandler for EmskinState {}
delegate_output!(EmskinState);
