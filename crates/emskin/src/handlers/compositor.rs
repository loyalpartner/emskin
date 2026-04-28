use std::time::Duration;

use crate::{state::ClientState, EmskinState};
use smithay::wayland::seat::WaylandFocus;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    desktop::{layer_map_for_output, utils::send_frames_surface_tree, WindowSurfaceType},
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
        &mut self.wl.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
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
            let committed_window = self
                .workspace
                .active_space
                .elements()
                .find(|w| w.wl_surface().map(|s| *s == root).unwrap_or(false))
                .cloned();
            if let Some(ref window) = committed_window {
                window.on_commit();
            }
            // Floating dialog: re-center on every commit until the
            // client's natural size lands (`promote_floating_dialog`
            // configures 0×0, the buffer that finally arrives sets the
            // real size, and we want it visually centered). Idempotent
            // — `re_center_dialog` is just `map_element` with a fresh
            // location.
            if let Some(window) = committed_window {
                if window
                    .user_data()
                    .get::<crate::tick::FloatingDialogTag>()
                    .is_some()
                    && window.geometry().size.w > 0
                    && window.geometry().size.h > 0
                {
                    crate::tick::re_center_dialog(self, &window);
                }
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
                self.workspace
                    .active_space
                    .map_element(window, geo.loc, false);
                tracing::debug!("embedded app window_id={window_id} geometry committed: {geo:?}");
            }
        };

        // Layer surface commit: re-arrange and send pending configure.
        // Keyboard focus is set here (not in new_layer_surface) because
        // cached_state only has keyboard_interactivity after initial commit.
        let layer_focus =
            if let Some(output) = self.workspace.active_space.outputs().next().cloned() {
                let mut map = layer_map_for_output(&output);
                let layer = map
                    .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                    .cloned();
                if let Some(ref layer) = layer {
                    // Capture the non-exclusive zone *before* and *after*
                    // arrange so we only relayout Emacs when the usable area
                    // actually shifts. `arrange()` returns true on any
                    // layout change — including a non-exclusive overlay
                    // (rofi / zofi launcher) simply moving into place,
                    // which must *not* trigger an Emacs resize.
                    let zone_before = map.non_exclusive_zone();
                    map.arrange();
                    let zone_after = map.non_exclusive_zone();
                    drop(map);
                    layer.layer_surface().send_pending_configure();

                    let needs_focus = layer.can_receive_keyboard_focus();
                    Some((needs_focus, layer.clone(), zone_before != zone_after))
                } else {
                    None
                }
            } else {
                None
            };
        if let Some((needs_focus, layer, zone_changed)) = layer_focus {
            if needs_focus {
                if let Some(keyboard) = self.seat.get_keyboard() {
                    let target = crate::KeyboardFocusTarget::from(layer);
                    if keyboard.current_focus().as_ref() != Some(&target) {
                        // Save current focus so layer_destroyed can restore it.
                        self.focus
                            .enter(crate::state::FocusOverride::Layer, keyboard.current_focus());
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(target), serial);
                        tracing::debug!("layer surface received keyboard focus");
                    }
                }
            }
            // Only relayout when the non-exclusive zone — the rect Emacs
            // tiles into — actually changes. Launchers with no exclusive
            // zone commit frequently while fuzzing their input and must
            // not cause Emacs to resize on every keystroke.
            if zone_changed {
                self.relayout_emacs();
            }
            return;
        }

        xdg_shell::handle_surface_commit(
            &mut self.wl.popups,
            &self.workspace.active_space,
            surface,
        );

        // Fire frame callbacks for surfaces not tracked in space or layer map
        // (e.g., temporary Vulkan test surfaces created during GPU init).
        // Without this, Vulkan WSI's vkQueuePresentKHR stalls waiting for
        // wl_surface.frame callbacks that never arrive.
        let is_space_element = self
            .workspace
            .active_space
            .elements()
            .any(|w| w.wl_surface().is_some_and(|s| *s == *surface));
        if !is_space_element {
            tracing::trace!("untracked surface commit: {surface:?}");
            if let Some(output) = self.workspace.active_space.outputs().next() {
                send_frames_surface_tree(
                    surface,
                    output,
                    self.start_time.elapsed(),
                    Some(Duration::ZERO),
                    |_, _| None,
                );
            }
        }
    }
}

impl BufferHandler for EmskinState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for EmskinState {
    fn shm_state(&self) -> &ShmState {
        &self.wl.shm_state
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
