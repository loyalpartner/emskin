use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::{
    delegate_xdg_activation,
    utils::SERIAL_COUNTER,
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};

use crate::EmskinState;

impl XdgActivationHandler for EmskinState {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Accept tokens that are less than 10 seconds old.
        data.timestamp.elapsed().as_secs() < 10
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if token_data.timestamp.elapsed().as_secs() >= 10 {
            tracing::debug!("xdg_activation: token expired, ignoring");
            self.xdg_activation_state.remove_token(&token);
            return;
        }

        tracing::info!("xdg_activation: activating surface");

        // Look up window_id before set_focus consumes the surface.
        if let Some(window_id) = self.apps.id_for_surface(&surface) {
            self.ipc.send(crate::ipc::OutgoingMessage::FocusView {
                window_id,
                view_id: 0,
            });
        }

        self.prefix_saved_focus = None;
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(surface), serial);
        }

        self.xdg_activation_state.remove_token(&token);
    }
}

delegate_xdg_activation!(EmskinState);
