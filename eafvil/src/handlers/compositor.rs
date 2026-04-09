use crate::{state::ClientState, EafvilState};
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
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

impl CompositorHandler for EafvilState {
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

            // Pending → committed geometry transition for EAF app windows.
            // When an EAF app commits a new buffer after a configure, atomically
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
                tracing::debug!("EAF app window_id={window_id} geometry committed: {geo:?}");
            }
        };

        xdg_shell::handle_surface_commit(&mut self.popups, &self.space, surface);
    }
}

impl BufferHandler for EafvilState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for EafvilState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

delegate_compositor!(EafvilState);
delegate_shm!(EafvilState);

smithay::delegate_viewporter!(EafvilState);
impl smithay::wayland::fractional_scale::FractionalScaleHandler for EafvilState {
    fn new_fractional_scale(
        &mut self,
        _surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
    }
}
smithay::delegate_fractional_scale!(EafvilState);
