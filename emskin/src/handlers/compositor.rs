use crate::{state::ClientState, EmskinState};
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    desktop::{layer_map_for_output, WindowSurfaceType},
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_surface::WlSurface},
        Client,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            get_parent, is_sync_subsurface, CompositorClientState, CompositorHandler,
            CompositorState,
        },
        shm::{ShmHandler, ShmState},
    },
};

use super::xdg_shell;

impl CompositorHandler for EmskinState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("ClientState missing — client was not inserted via our listener")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self
                .space
                .elements()
                .find(|w| w.wl_surface().map(|s| *s == root).unwrap_or(false))
            {
                window.on_commit();
            }

            // Pending → committed geometry transition for embedded app windows.
            // When an embedded app commits a new buffer after a configure, atomically
            // switch its geometry so the new buffer and new position appear together.
            let commit_info = self.apps.get_mut_by_surface(&root).and_then(|app| {
                app.pending_geometry.take().map(|pending| {
                    app.geometry = Some(pending);
                    app.pending_since = None;
                    (app.window.clone(), app.window_id, pending)
                })
            });
            if let Some((window, window_id, geo)) = commit_info {
                self.space.map_element(window, geo.loc, false);
                tracing::debug!("embedded app window_id={window_id} geometry committed: {geo:?}");
            }
        };

        // Layer surface commit: re-arrange and send pending configure.
        // Keyboard focus is set here (not in new_layer_surface) because
        // cached_state only has keyboard_interactivity after initial commit.
        let layer_focus = if let Some(output) = self.space.outputs().next().cloned() {
            let mut map = layer_map_for_output(&output);
            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .cloned();
            if let Some(ref layer) = layer {
                map.arrange();
                drop(map);
                layer.layer_surface().send_pending_configure();

                let needs_focus = layer.can_receive_keyboard_focus();
                let wl = layer.wl_surface().clone();
                Some((needs_focus, wl))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((needs_focus, wl)) = layer_focus {
            if needs_focus {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    if keyboard.current_focus().as_ref() != Some(&wl) {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(wl), serial);
                        tracing::debug!("layer surface received keyboard focus");
                    }
                }
            }
            return;
        }

        xdg_shell::handle_surface_commit(&mut self.popups, &self.space, surface);
    }
}

impl BufferHandler for EmskinState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for EmskinState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(EmskinState);
delegate_shm!(EmskinState);

smithay::delegate_viewporter!(EmskinState);
impl smithay::wayland::fractional_scale::FractionalScaleHandler for EmskinState {
    fn new_fractional_scale(
        &mut self,
        _surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
    }
}
smithay::delegate_fractional_scale!(EmskinState);
