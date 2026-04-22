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
        &mut self.wl.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        if self.emacs.should_claim_main() {
            // First toplevel = Emacs (Wayland/pgtk path only).
            // X11 Emacs sets initial_size_settled in map_window_request.
            tracing::info!("Emacs toplevel connected");
            self.emacs.set_surface(Some(surface.wl_surface().clone()));

            if let Some(output) = self.workspace.active_space.outputs().next() {
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
            self.emacs.mark_size_settled();

            let window = Window::new_wayland_window(surface);
            self.workspace
                .active_space
                .map_element(window.clone(), (0, 0), false);
            // Emacs is the fullscreen host and must stay at the bottom of
            // the stack so later app toplevels (and any remap via
            // resize_emacs_in_space) never cover them.
            self.workspace.active_space.lower_element(&window);

            // Give Emacs initial keyboard focus.
            let serial = SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(window.into()), serial);
            }
        } else if self.is_emacs_client(surface.wl_surface()) {
            // Same Wayland client as Emacs — could be a new frame (C-x 5 2) or
            // a child frame (posframe, company-posframe, etc.).
            //
            // We can't tell yet: set_parent() hasn't been processed at this point
            // (GTK sends get_toplevel + set_parent in the same Wayland batch, but
            // set_parent is processed after new_toplevel). Defer the decision to
            // the event loop idle callback where surface.parent() is available.
            //
            // Configure Fullscreen + output size and send immediately.
            // Don't wait for handle_surface_commit — sending now ensures GTK
            // sees Fullscreen as the very first configure (no CSD flash).
            // GTK ignores Fullscreen on transient (child) windows, so this is
            // safe even if this turns out to be a child frame.
            if let Some(geo) = self.output_fullscreen_geo() {
                surface.with_pending_state(|s| {
                    s.size = Some(geo.size);
                    s.states.set(xdg_toplevel::State::Fullscreen);
                });
                surface.send_configure();
            }
            let window = Window::new_wayland_window(surface.clone());
            self.workspace
                .active_space
                .map_element(window.clone(), (0, 0), false);
            // Keep Emacs at the bottom while it sits briefly in the active
            // space before `process_pending_toplevels` decides whether to
            // move it into a new workspace.
            self.workspace.active_space.lower_element(&window);
            self.workspace
                .pending_emacs_toplevels
                .push((surface, window));
            tracing::info!("Emacs client toplevel detected — deferred for parent check");
        } else {
            // Subsequent toplevels from other clients = embedded app windows.
            let window_id = self.apps.alloc_id();
            let title =
                Self::get_toplevel_data(&surface, |d| d.lock().ok().and_then(|d| d.title.clone()))
                    .unwrap_or_default();

            tracing::info!(
                "embedded app toplevel connected: window_id={window_id} title={title:?}"
            );

            // Start at 1×1; actual size arrives via set_geometry IPC.
            surface.with_pending_state(|s| {
                s.size = Some((1, 1).into());
            });

            let window = Window::new_wayland_window(surface);
            // Map at 1×1 so on_commit() and initial configure work.
            self.workspace
                .active_space
                .map_element(window.clone(), (0, 0), false);
            self.apps.insert(crate::apps::AppWindow {
                window_id,
                window: window.clone(),
                workspace_id: self.workspace.active_id,
                geometry: None,
                pending_geometry: None,
                pending_since: None,
                visible: false,
                mirrors: std::collections::HashMap::new(),
            });

            self.ipc
                .send(crate::ipc::OutgoingMessage::WindowCreated { window_id, title });

            self.auto_focus_new_window(window, window_id);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(e) = self.wl.popups.track_popup(PopupKind::Xdg(surface)) {
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
        tracing::debug!("popup grab requested, serial={:?}", serial);
        let Some(seat) = Seat::<EmskinState>::from_resource(&seat) else {
            tracing::warn!("popup grab: seat not found");
            return;
        };
        let kind = PopupKind::Xdg(surface);

        if let Ok(root) = find_popup_root_surface(&kind) {
            // PopupGrab needs the root as our KeyboardFocusTarget, not a bare
            // wl_surface. Map it back through the space.
            let Some(root_target) = self.focus_target_for_surface(&root) else {
                tracing::warn!("popup grab: root surface has no known focus target");
                return;
            };
            let ret = self.wl.popups.grab_popup(root_target, kind, &seat, serial);

            match ret {
                Ok(mut grab) => {
                    if let Some(keyboard) = seat.get_keyboard() {
                        if keyboard.is_grabbed()
                            && !(keyboard.has_grab(serial)
                                || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
                        {
                            tracing::debug!("popup grab: keyboard already grabbed, ungrabbing");
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
                            tracing::debug!("popup grab: pointer already grabbed, ungrabbing");
                            grab.ungrab(PopupUngrabStrategy::All);
                            return;
                        }
                        pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
                        tracing::debug!("popup grab: pointer grab set successfully");
                    }
                }
                Err(e) => {
                    tracing::warn!("popup grab failed: {:?}", e);
                }
            }
        } else {
            tracing::warn!("popup grab: could not find root surface");
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        if self.is_emacs_surface(&surface) {
            tracing::info!("Emacs requested fullscreen");
            self.emacs.request_fullscreen(true);
            Self::set_toplevel_state(&surface, xdg_toplevel::State::Fullscreen, true);
        } else if self.apps.id_for_surface(surface.wl_surface()).is_some() {
            // Embedded app fullscreen: set state so the client hides its
            // toolbar/chrome, but keep the window sized to its Emacs buffer.
            Self::set_toplevel_state(&surface, xdg_toplevel::State::Fullscreen, true);
            tracing::debug!("embedded app fullscreen request acknowledged");
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if self.is_any_emacs_surface(surface.wl_surface()) {
            return;
        }
        if self.apps.id_for_surface(surface.wl_surface()).is_some() {
            Self::set_toplevel_state(&surface, xdg_toplevel::State::Fullscreen, false);
            tracing::debug!("embedded app unfullscreen request acknowledged");
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if self.is_emacs_surface(&surface) {
            tracing::info!("Emacs requested maximize");
            self.emacs.request_maximize(true);
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
            // Active workspace Emacs — forward title to host window + update bar name.
            if let Some(title) = title {
                tracing::debug!("Emacs title changed: {title}");
                self.workspace.active_name = extract_bar_name(&title);
                self.emacs.set_title(title);
            }
        } else if self.is_any_emacs_surface(surface.wl_surface()) {
            // Inactive workspace Emacs frame — update its workspace name.
            if let Some(title) = &title {
                let short = extract_bar_name(title);
                for ws in self.workspace.inactive.values_mut() {
                    if ws
                        .emacs_surface
                        .as_ref()
                        .is_some_and(|s| s == surface.wl_surface())
                    {
                        ws.name = short;
                        break;
                    }
                }
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
                self.emacs.set_app_id(app_id);
            }
        }
        // Inactive workspace Emacs or other surfaces: ignore app_id changes.
    }
}

/// Extract a short display name from an Emacs frame title for the workspace bar.
/// Strips " - GNU Emacs ..." suffix and "*eaf: " prefix.
/// e.g. "*eaf: firefox* - GNU Emacs at home" → "firefox*"
///      "*scratch* - GNU Emacs at home" → "*scratch*"
fn extract_bar_name(title: &str) -> String {
    let base = title.split(" - GNU Emacs").next().unwrap_or(title).trim();
    let base = base.strip_prefix("*eaf: ").unwrap_or(base);
    base.to_string()
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
        self.emacs.is_main_surface(surface.wl_surface())
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
            .workspace
            .active_space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root))
        else {
            return;
        };

        let Some(output) = self.workspace.active_space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.workspace.active_space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.workspace.active_space.element_geometry(window) else {
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
