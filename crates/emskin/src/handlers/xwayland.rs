use std::os::fd::OwnedFd;

use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    utils::{Logical, Rectangle},
    wayland::{
        selection::{
            data_device::{
                clear_data_device_selection, request_data_device_client_selection,
                set_data_device_selection,
            },
            primary_selection::{
                clear_primary_selection, request_primary_client_selection, set_primary_selection,
            },
            SelectionTarget,
        },
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

        if self.detect_emacs && !self.initial_size_settled && !window.is_override_redirect() {
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
            self.emacs_x11_window = Some(win.clone());
            self.initial_size_settled = true;

            // Focus the Emacs `Window` directly. smithay's `X11Surface`
            // `KeyboardTarget` impl queues the enter in `pending_enter`
            // when `wl_surface` hasn't been associated yet, so we don't
            // have to wait for the `post_render` poll — as soon as the
            // association resolves, the queued enter fires automatically.
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, Some(win.into()), serial);
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
            window: win.clone(),
            workspace_id: self.active_workspace_id,
            geometry: Some(geo),
            pending_geometry: None,
            pending_since: None,
            visible: true,
            mirrors: std::collections::HashMap::new(),
        });

        self.ipc
            .send(crate::ipc::OutgoingMessage::WindowCreated { window_id, title });

        self.auto_focus_new_window(win, window_id);
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

        // Internal bridge (anvil pattern): advertise the X11 selection on the
        // wayland data_device so wayland clients (wl-paste, Emacs pgtk) can
        // see the offer directly, without depending on a host clipboard
        // manager to echo the change back. SelectionHandler::send_selection
        // will route paste fds back to X via xwm.send_selection.
        match selection {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), ());
                self.selection.clipboard_origin = crate::state::SelectionOrigin::X11;
            }
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types.clone(), ());
                self.selection.primary_origin = crate::state::SelectionOrigin::X11;
            }
        }

        // Still push to the host proxy so the user's real desktop clipboard
        // manager stays in sync. Gated on IPC connectivity because GTK's
        // startup selection announcement would otherwise clobber host
        // clipboard before the user ever types anything.
        if self.ipc.is_connected() {
            if let Some(ref mut clipboard) = self.selection.clipboard {
                clipboard.set_host_selection(selection, &mime_types);
            }
        } else {
            tracing::debug!("Skipping pre-IPC host push of X11 {selection:?} selection");
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        tracing::debug!("X11 selection cleared ({selection:?})");
        match selection {
            SelectionTarget::Clipboard => {
                self.selection.host_clipboard_mimes.clear();
                self.selection.clipboard_origin = crate::state::SelectionOrigin::default();
                clear_data_device_selection(&self.display_handle, &self.seat);
            }
            SelectionTarget::Primary => {
                self.selection.host_primary_mimes.clear();
                self.selection.primary_origin = crate::state::SelectionOrigin::default();
                clear_primary_selection(&self.display_handle, &self.seat);
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
        tracing::debug!("X11 paste request ({selection:?}, {mime_type})");
        let origin = match selection {
            SelectionTarget::Clipboard => self.selection.clipboard_origin,
            SelectionTarget::Primary => self.selection.primary_origin,
        };
        use crate::state::SelectionOrigin;
        match origin {
            SelectionOrigin::Wayland => match selection {
                SelectionTarget::Clipboard => {
                    if let Err(e) = request_data_device_client_selection(&self.seat, mime_type, fd)
                    {
                        tracing::warn!("X11 paste from wayland clipboard source failed: {e}");
                    }
                }
                SelectionTarget::Primary => {
                    if let Err(e) = request_primary_client_selection(&self.seat, mime_type, fd) {
                        tracing::warn!("X11 paste from wayland primary source failed: {e}");
                    }
                }
            },
            SelectionOrigin::X11 => {
                // Another X client on our own XWayland owns the selection
                // — X server handles paste directly without needing emskin
                // to mediate. If we're asked, drop fd so peer gets EOF.
                tracing::debug!("X11 paste origin=X11 — XWayland handles directly, dropping fd");
                drop(fd);
            }
            SelectionOrigin::Host => {
                if let Some(ref mut clipboard) = self.selection.clipboard {
                    clipboard.receive_from_host(selection, &mime_type, fd);
                } else {
                    tracing::warn!("X11 paste origin=Host but no ClipboardProxy; dropping fd");
                    drop(fd);
                }
            }
        }
    }
}

impl XWaylandShellHandler for EmskinState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.wl.xwayland_shell_state
    }
}

delegate_xwayland_shell!(EmskinState);
