use std::os::fd::OwnedFd;

use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    utils::{Logical, Rectangle},
    wayland::{
        selection::SelectionTarget,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge, XwmId},
        X11Surface, X11Wm, XwmHandler,
    },
};

use crate::{utils::SizeExt, EmskinState};

impl XwmHandler for EmskinState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .as_mut()
            .expect("xwm_state called before XWayland ready")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::debug!(
            "X11 new_window: title={:?} OR={} geo={:?}",
            window.title(),
            window.is_override_redirect(),
            window.geometry()
        );
    }
    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        tracing::info!(
            "X11 new_override_redirect_window: title={:?} geo={:?}",
            window.title(),
            window.geometry()
        );
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(e) = window.set_mapped(true) {
            tracing::warn!("X11 set_mapped(true) failed: {e}");
            return;
        }

        if !self.initial_size_settled && !window.is_override_redirect() {
            // First non-OR X11 window = Emacs (gtk3 via XWayland).
            tracing::info!("Emacs X11 window connected: title={:?}", window.title());

            if let Some(geo) = self.output_fullscreen_geo() {
                if let Err(e) = window.configure(geo) {
                    tracing::warn!("X11 Emacs configure failed: {e}");
                }
                self.ipc.send(crate::ipc::OutgoingMessage::SurfaceSize {
                    width: geo.size.w,
                    height: geo.size.h,
                });
            }

            // wl_surface may not be available yet — XWayland associates it
            // asynchronously. Store the Window and poll in the event loop.
            self.emacs_surface = window.wl_surface();
            let win = Window::new_x11_window(window);
            self.space.map_element(win.clone(), (0, 0), false);
            self.emacs_x11_window = Some(win);
            self.initial_size_settled = true;

            if self.emacs_surface.is_some() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                if let Some(keyboard) = self.seat.get_keyboard() {
                    keyboard.set_focus(self, self.emacs_surface.clone(), serial);
                }
            }
            return;
        }

        let window_id = self.apps.alloc_id();
        let title = window.title();
        let mut geo = window.geometry();
        geo.size = geo.size.at_least((1, 1));

        tracing::info!("X11 window mapped: window_id={window_id} title={title:?}");

        let win = Window::new_x11_window(window);
        self.space.map_element(win.clone(), geo.loc, false);

        self.apps.insert(crate::apps::AppWindow {
            window_id,
            window: win,
            workspace_id: self.active_workspace_id,
            geometry: Some(geo),
            pending_geometry: None,
            pending_since: None,
            visible: true,
            mirrors: std::collections::HashMap::new(),
        });

        self.ipc
            .send(crate::ipc::OutgoingMessage::WindowCreated { window_id, title });
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let geo = window.geometry();
        let has_surface = window.wl_surface().is_some();
        tracing::info!(
            "X11 OR window mapped: geo={geo:?} has_wl_surface={has_surface} title={:?}",
            window.title()
        );
        let win = Window::new_x11_window(window);
        self.space.map_element(win, geo.loc, true);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // Find and unmap from space. drain_dead() in the event loop handles AppManager cleanup.
        let elem = self
            .space
            .elements()
            .find(|e| e.x11_surface() == Some(&window))
            .cloned();
        if let Some(elem) = elem {
            self.space.unmap_elem(&elem);
        }
        if !window.is_override_redirect() {
            if let Err(e) = window.set_mapped(false) {
                tracing::warn!("X11 set_mapped(false) failed: {e}");
            }
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Emacs X11 window must stay fullscreen — ignore client resize requests.
        let is_emacs = self
            .emacs_x11_window
            .as_ref()
            .is_some_and(|win| win.x11_surface() == Some(&window));
        if is_emacs {
            if let Some(geo) = self.output_fullscreen_geo() {
                if let Err(e) = window.configure(geo) {
                    tracing::warn!("X11 Emacs configure failed: {e}");
                }
            }
            return;
        }

        let mut geo = window.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        geo.size = geo.size.at_least((1, 1));
        if let Err(e) = window.configure(geo) {
            tracing::warn!("X11 configure_request failed: {e}");
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Override-redirect windows manage their own position.
        if window.is_override_redirect() {
            let elem = self
                .space
                .elements()
                .find(|e| e.x11_surface() == Some(&window))
                .cloned();
            if let Some(ref elem) = elem {
                self.space.map_element(elem.clone(), geometry.loc, false);
            }
            tracing::debug!(
                "X11 OR configure_notify: geo={geometry:?} found_in_space={}",
                elem.is_some()
            );
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _edges: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        true
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        tracing::debug!("X11 selection set ({selection:?}): {mime_types:?}");
        if let Some(ref mut clipboard) = self.selection.clipboard {
            if !self.ipc.is_connected() {
                tracing::debug!("Skipping pre-IPC X11 {selection:?} selection");
                return;
            }
            match selection {
                SelectionTarget::Clipboard => {
                    self.selection.clipboard_origin = crate::state::SelectionOrigin::X11
                }
                SelectionTarget::Primary => {
                    self.selection.primary_origin = crate::state::SelectionOrigin::X11
                }
            }
            clipboard.set_host_selection(selection, &mime_types);
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        tracing::debug!("X11 selection cleared ({selection:?})");
        match selection {
            SelectionTarget::Clipboard => {
                self.selection.host_clipboard_mimes.clear();
                self.selection.clipboard_origin = crate::state::SelectionOrigin::default();
            }
            SelectionTarget::Primary => {
                self.selection.host_primary_mimes.clear();
                self.selection.primary_origin = crate::state::SelectionOrigin::default();
            }
        }
        if let Some(ref mut clipboard) = self.selection.clipboard {
            clipboard.clear_host_selection(selection);
        }
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        // X11 client wants to paste — forward from host clipboard.
        if let Some(ref mut clipboard) = self.selection.clipboard {
            tracing::debug!("X11 paste request ({selection:?}, {mime_type})");
            clipboard.receive_from_host(selection, &mime_type, fd);
        }
    }
}

impl XWaylandShellHandler for EmskinState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.wl.xwayland_shell_state
    }
}

delegate_xwayland_shell!(EmskinState);
