use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    utils::{Logical, Rectangle},
    wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
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

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        window.set_mapped(true).unwrap();

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
            if let Some(elem) = elem {
                self.space.map_element(elem, geometry.loc, false);
            }
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
}

impl XWaylandShellHandler for EmskinState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

delegate_xwayland_shell!(EmskinState);
