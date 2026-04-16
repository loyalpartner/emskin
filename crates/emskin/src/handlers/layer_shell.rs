use smithay::{
    delegate_layer_shell,
    desktop::{layer_map_for_output, LayerSurface as DesktopLayerSurface, WindowSurfaceType},
    reexports::wayland_server::{protocol::wl_output::WlOutput, Resource},
    utils::{Size, SERIAL_COUNTER},
    wayland::shell::wlr_layer::{Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState},
};

use crate::EmskinState;

impl WlrLayerShellHandler for EmskinState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.wl.layer_shell_state
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
        let zone_before = map.non_exclusive_zone();
        if let Err(e) = map.map_layer(&desktop_layer) {
            tracing::warn!("layer_shell: map_layer failed: {e}");
            desktop_layer.layer_surface().send_close();
            return;
        }

        tracing::info!("layer_shell: new surface, namespace={namespace}",);

        // map_layer() internally calls arrange() which uses cached_state.
        // At this point cached_state has defaults (no anchors, 0×0), so
        // arrange computed wrong geometry (half-output, centered).
        //
        // Override pending size with the full output size before sending
        // the initial configure. This is correct for all-4-anchor surfaces
        // (launchers, full-screen overlays) and unblocks clients whose
        // event loop won't flush the initial wl_surface.commit until they
        // receive a configure event. After the client's first commit,
        // arrange() with correct cached_state sends the precise configure.
        let output_logical_size = output
            .current_mode()
            .map(|mode| {
                mode.size
                    .to_f64()
                    .to_logical(output.current_scale().fractional_scale())
                    .to_i32_round()
            })
            .unwrap_or_else(|| Size::from((0, 0)));

        desktop_layer.layer_surface().with_pending_state(|state| {
            state.size = Some(output_logical_size);
        });
        desktop_layer.layer_surface().send_pending_configure();
        let zone_after = map.non_exclusive_zone();
        drop(map);

        // Only relayout when the non-exclusive zone actually shifts —
        // an overlay launcher (zofi / rofi) with exclusive_zone=0 must
        // not cause Emacs to resize just by appearing.
        if zone_before != zone_after {
            self.relayout_emacs();
        } else {
            self.needs_redraw = true;
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        let (zone_before, zone_after) = if let Some(output) = self.space.outputs().next().cloned() {
            let mut map = layer_map_for_output(&output);
            let before = map.non_exclusive_zone();
            let found = map
                .layer_for_surface(surface.wl_surface(), WindowSurfaceType::TOPLEVEL)
                .cloned();
            if let Some(layer) = found {
                map.unmap_layer(&layer);
            }
            let after = map.non_exclusive_zone();
            drop(map);
            (before, after)
        } else {
            (Default::default(), Default::default())
        };

        tracing::info!("layer_shell: surface destroyed");

        if zone_before != zone_after {
            self.relayout_emacs();
        } else {
            self.needs_redraw = true;
        }

        // Restore focus to whatever had it before the layer surface took over.
        // Check is_alive() to handle sequential layer surfaces where the saved
        // surface may have been destroyed before this one.
        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus().as_ref() == Some(surface.wl_surface()) {
                let restore = self
                    .focus
                    .layer_saved_focus
                    .take()
                    .filter(|s| s.is_alive())
                    .or(self.emacs_surface.clone());
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, restore, serial);
            }
        }
    }
}

delegate_layer_shell!(EmskinState);
