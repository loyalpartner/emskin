use smithay::{
    delegate_xdg_shell,
    desktop::{
        find_popup_root_surface, get_popup_toplevel_coords, PopupKeyboardGrab, PopupKind,
        PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space, Window,
    },
    input::{pointer::Focus, Seat},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::{wl_output::WlOutput, wl_seat, wl_surface::WlSurface},
    },
    utils::{Serial, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::EmskinState;

impl XdgShellHandler for EmskinState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        if self.emacs_surface.is_none() {
            // First toplevel = Emacs.
            tracing::info!("Emacs toplevel connected");
            self.emacs_surface = Some(surface.wl_surface().clone());

            if let Some(output) = self.space.outputs().next() {
                if let Some(mode) = output.current_mode() {
                    let scale = output.current_scale().fractional_scale();
                    let logical = mode.size.to_f64().to_logical(scale).to_i32_round();
                    surface.with_pending_state(|state| {
                        state.size = Some(logical);
                        state.states.set(xdg_toplevel::State::Fullscreen);
                    });
                    self.ipc.send(crate::ipc::OutgoingMessage::SurfaceSize {
                        width: logical.w,
                        height: logical.h,
                    });
                }
            }
            self.initial_size_settled = true;

            let window = Window::new_wayland_window(surface);
            self.space.map_element(window, (0, 0), false);

            // Give Emacs initial keyboard focus.
            let serial = SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, self.emacs_surface.clone(), serial);
            }
        } else {
            // Subsequent toplevels = EAF app windows.
            let window_id = self.apps.alloc_id();
            let title =
                Self::get_toplevel_data(&surface, |d| d.lock().ok().and_then(|d| d.title.clone()))
                    .unwrap_or_default();

            tracing::info!("EAF app toplevel connected: window_id={window_id} title={title:?}");

            // Start at 1×1; actual size arrives via set_geometry IPC.
            surface.with_pending_state(|s| {
                s.size = Some((1, 1).into());
            });

            let window = Window::new_wayland_window(surface);
            // Map at 1×1 so on_commit() and initial configure work.
            self.space.map_element(window.clone(), (0, 0), false);
            self.apps.insert(crate::apps::AppWindow {
                window_id,
                window,
                geometry: None,
                pending_geometry: None,
                pending_since: None,
                visible: false,
                mirrors: std::collections::HashMap::new(),
            });

            self.ipc
                .send(crate::ipc::OutgoingMessage::WindowCreated { window_id, title });
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("Failed to track popup: {}", e);
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, _surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Emacs is always fullscreen in emskin — ignore move requests
    }

    fn resize_request(
        &mut self,
        _surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        _edges: xdg_toplevel::ResizeEdge,
    ) {
        // Emacs is always fullscreen in emskin — ignore resize requests
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<EmskinState>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);

        if let Ok(root) = find_popup_root_surface(&kind) {
            let ret = self.popups.grab_popup(root, kind, &seat, serial);

            if let Ok(mut grab) = ret {
                if let Some(keyboard) = seat.get_keyboard() {
                    if keyboard.is_grabbed()
                        && !(keyboard.has_grab(serial)
                            || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    keyboard.set_focus(self, grab.current_grab(), serial);
                    keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
                }
                if let Some(pointer) = seat.get_pointer() {
                    if pointer.is_grabbed()
                        && !(pointer.has_grab(serial)
                            || pointer.has_grab(grab.previous_serial().unwrap_or(serial)))
                    {
                        grab.ungrab(PopupUngrabStrategy::All);
                        return;
                    }
                    pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                }
            }
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        if self.is_emacs_surface(&surface) {
            tracing::info!("Emacs requested fullscreen");
            self.pending_fullscreen = Some(true);
            Self::set_toplevel_state(&surface, xdg_toplevel::State::Fullscreen, true);
        }
    }

    fn unfullscreen_request(&mut self, _surface: ToplevelSurface) {
        // Emacs always fills the compositor window — ignore unfullscreen
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if self.is_emacs_surface(&surface) {
            tracing::info!("Emacs requested maximize");
            self.pending_maximize = Some(true);
            Self::set_toplevel_state(&surface, xdg_toplevel::State::Maximized, true);
        }
    }

    fn unmaximize_request(&mut self, _surface: ToplevelSurface) {
        // Emacs always fills the compositor window — ignore unmaximize
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let title =
            Self::get_toplevel_data(&surface, |d| d.lock().ok().and_then(|d| d.title.clone()));
        if self.is_emacs_surface(&surface) {
            if let Some(title) = title {
                tracing::debug!("Emacs title changed: {title}");
                self.emacs_title = Some(title);
            }
        } else if let Some(window_id) = self.apps.id_for_surface(surface.wl_surface()) {
            if let Some(title) = title {
                self.ipc
                    .send(crate::ipc::OutgoingMessage::TitleChanged { window_id, title });
            }
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        if self.is_emacs_surface(&surface) {
            let app_id =
                Self::get_toplevel_data(&surface, |d| d.lock().ok().and_then(|d| d.app_id.clone()));
            if let Some(app_id) = app_id {
                tracing::debug!("Emacs app_id changed: {}", app_id);
                self.emacs_app_id = Some(app_id);
            }
        }
    }
}

// Xdg Shell
delegate_xdg_shell!(EmskinState);

// Xdg Decoration — always force server-side (no decorations drawn = borderless)
use smithay::delegate_xdg_decoration;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

impl XdgDecorationHandler for EmskinState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_pending_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(Mode::ServerSide);
        });
        toplevel.send_pending_configure();
    }
}

delegate_xdg_decoration!(EmskinState);

pub fn handle_surface_commit(
    popups: &mut PopupManager,
    space: &Space<Window>,
    surface: &WlSurface,
) {
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok())
                .map(|d| d.initial_configure_sent)
                .unwrap_or(true)
        });

        if !initial_configure_sent {
            if let Some(toplevel) = window.toplevel() {
                toplevel.send_configure();
            }
        }
    }

    // Handle popup commits.
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    if let Err(e) = xdg.send_configure() {
                        tracing::warn!("initial popup configure failed: {e}");
                    }
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl EmskinState {
    fn is_emacs_surface(&self, surface: &ToplevelSurface) -> bool {
        Some(surface.wl_surface()) == self.emacs_surface.as_ref()
    }

    fn set_toplevel_state(surface: &ToplevelSurface, state: xdg_toplevel::State, enabled: bool) {
        surface.with_pending_state(|s| {
            if enabled {
                s.states.set(state);
            } else {
                s.states.unset(state);
            }
        });
        surface.send_pending_configure();
    }

    fn get_toplevel_data<T>(
        surface: &ToplevelSurface,
        extractor: impl FnOnce(&XdgToplevelSurfaceData) -> Option<T>,
    ) -> Option<T> {
        with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(extractor)
        })
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let popup_kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&popup_kind) else {
            return;
        };
        let Some(window) = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };

        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(window) else {
            return;
        };

        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&popup_kind);
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
