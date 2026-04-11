use smithay::{
    delegate_layer_shell,
    desktop::{layer_map_for_output, LayerSurface as DesktopLayerSurface, WindowSurfaceType},
    reexports::wayland_server::protocol::wl_output::WlOutput,
    utils::SERIAL_COUNTER,
    wayland::shell::wlr_layer::{Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState},
};

use crate::EmskinState;

impl WlrLayerShellHandler for EmskinState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let desktop_layer = DesktopLayerSurface::new(surface, namespace.clone());

        let Some(output) = self.space.outputs().next().cloned() else {
            tracing::warn!("layer_shell: no output, closing surface (namespace={namespace})");
            desktop_layer.layer_surface().send_close();
            return;
        };

        let mut map = layer_map_for_output(&output);
        if let Err(e) = map.map_layer(&desktop_layer) {
            tracing::warn!("layer_shell: map_layer failed: {e}");
            desktop_layer.layer_surface().send_close();
            return;
        }

        tracing::info!(
            "layer_shell: new surface, namespace={namespace} layer={:?}",
            desktop_layer.layer(),
        );

        desktop_layer.layer_surface().send_pending_configure();
        drop(map);

        // Keyboard focus is deferred to the compositor commit handler:
        // new_layer_surface fires on get_layer_surface (before initial commit),
        // so cached_state has no keyboard_interactivity yet.
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        if let Some(output) = self.space.outputs().next().cloned() {
            let mut map = layer_map_for_output(&output);
            let found = map
                .layer_for_surface(surface.wl_surface(), WindowSurfaceType::TOPLEVEL)
                .cloned();
            if let Some(layer) = found {
                map.unmap_layer(&layer);
            }
            drop(map);
        }

        tracing::info!("layer_shell: surface destroyed");

        // Only reclaim focus if this surface actually held it.
        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus().as_ref() == Some(surface.wl_surface()) {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, self.emacs_surface.clone(), serial);
            }
        }
    }
}

delegate_layer_shell!(EmskinState);
