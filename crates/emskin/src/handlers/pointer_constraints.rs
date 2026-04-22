// Pointer constraints + relative pointer — required by FPS games and 3D
// editors (Minecraft, Blender, browser Pointer Lock). Smithay does not
// auto-activate constraints; the compositor must call `activate()` both
// here (when target surface is already focused) and on pointer-enter in
// `input.rs`. Xwayland ≥ 22.1 proxies both protocols to X clients as
// XI_RawMotion, so wiring them server-side unblocks Xwayland games too.

use smithay::input::pointer::PointerHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point};
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsHandler};
use smithay::{delegate_pointer_constraints, delegate_relative_pointer};

use crate::EmskinState;

impl PointerConstraintsHandler for EmskinState {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    constraint.activate();
                }
            });
        }
    }

    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let active =
            with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.is_active()));
        if !active {
            return;
        }
        // Hint is surface-local; translate by the surface's space origin.
        // Layer shell / x11 surfaces aren't in `space.elements()` — fall
        // back to (0,0), matching anvil's behaviour for that path.
        use smithay::wayland::seat::WaylandFocus;
        let origin = self
            .workspace
            .active_space
            .elements()
            .find_map(|window| {
                (window.wl_surface().as_deref() == Some(surface))
                    .then(|| self.workspace.active_space.element_location(window))
                    .flatten()
            })
            .map(|loc| loc.to_f64())
            .unwrap_or_default();
        pointer.set_location(origin + location);
    }
}

delegate_pointer_constraints!(EmskinState);
delegate_relative_pointer!(EmskinState);
