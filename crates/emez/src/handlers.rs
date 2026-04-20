//! All Wayland protocol handlers for `Emez`.
//!
//! Kept as a single module because most handlers are stub-grade — emez
//! just advertises the globals, accepts clients, and lets smithay do the
//! heavy lifting. The interesting bits are:
//!
//! - `CompositorHandler::commit` runs the buffer handler so clients
//!   that commit a surface don't get stuck.
//! - `XdgShellHandler::new_toplevel` sends an initial configure so
//!   `emskin`'s winit-wayland backend can finish its handshake.
//! - `data_control` + `primary_selection` + `data_device` delegates are
//!   registered so the clipboard machinery works end-to-end.

use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::io::OwnedFd;

use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_control, delegate_data_device, delegate_ext_data_control,
    delegate_output, delegate_primary_selection, delegate_seat, delegate_shm,
    delegate_xdg_activation, delegate_xdg_shell,
    input::{
        dnd::{DndGrabHandler, GrabType, Source},
        Seat, SeatHandler, SeatState,
    },
    reexports::wayland_server::{
        protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
        Client, Resource,
    },
    utils::Serial,
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        output::OutputHandler,
        selection::{
            data_device::{DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler},
            ext_data_control::{
                DataControlHandler as ExtDataControlHandler,
                DataControlState as ExtDataControlState,
            },
            primary_selection::{PrimarySelectionHandler, PrimarySelectionState},
            wlr_data_control::{
                DataControlHandler as WlrDataControlHandler,
                DataControlState as WlrDataControlState,
            },
            SelectionHandler, SelectionSource, SelectionTarget,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
        xdg_activation::{
            XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
        },
    },
    xwayland::XWaylandClientData,
};

use crate::state::{ClientState, Emez};

impl CompositorHandler for Emez {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // XWayland clients carry their own CompositorClientState on
        // `XWaylandClientData`; all others use emez's ClientState.
        if let Some(xdata) = client.get_data::<XWaylandClientData>() {
            return &xdata.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        // emez does no rendering, but winit-backed clients (e.g. emskin)
        // block their render loop on frame callbacks. Fire them back
        // immediately so clients keep producing frames — this unblocks
        // capture/recording tests that depend on at least one rendered
        // frame reaching the emskin-side GPU readback path.
        use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
        let now = std::time::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_millis() as u32)
            .unwrap_or(0);
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_surface, states, _| {
                states
                    .cached_state
                    .get::<smithay::wayland::compositor::SurfaceAttributes>()
                    .current()
                    .frame_callbacks
                    .drain(..)
                    .for_each(|cb| {
                        cb.done(now);
                    });
            },
            |_, _, _| true,
        );
    }
}

impl BufferHandler for Emez {
    fn buffer_destroyed(&mut self, _: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Emez {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for Emez {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        tracing::debug!("focus_changed → {:?}", focused.map(|s| s.id()));
        // Mirror keyboard focus into the data_device + primary_selection
        // focus state. Without this, smithay's SeatData doesn't know which
        // client to broadcast `.selection(new_offer)` events to.
        use smithay::wayland::selection::{
            data_device::set_data_device_focus, primary_selection::set_primary_focus,
        };
        let client = focused.and_then(|s| self.display_handle.get_client(s.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);

        // Primary fallback: when the focused surface goes away and nobody
        // else is asking for focus (e.g. short-lived wl-copy finishes
        // set_selection and exits), hand focus back to the most recently
        // xdg_activation'd surface that's still alive. This mirrors what
        // real compositors do — focus doesn't evaporate into the void,
        // it falls back to the last in-use window.
        if focused.is_none() {
            if let Some(primary) = self.primary_fallback_focus.clone() {
                if primary.is_alive() {
                    if let Some(keyboard) = seat.get_keyboard() {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        // Recursive set_focus triggers focus_changed(Some(primary)) — safe,
                        // the `focused.is_none()` branch above won't re-fire.
                        keyboard.set_focus(self, Some(primary), serial);
                    }
                } else {
                    // Dead surface — clear our record so we don't keep
                    // trying to re-focus a gone client.
                    self.primary_fallback_focus = None;
                }
            }
        }
    }
}

impl OutputHandler for Emez {}

impl XdgShellHandler for Emez {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Send an immediate configure so winit-backed clients like emskin
        // get past their first round-trip. Size is advertised by the
        // advertised output (1920x1080) but clients can pick their own.
        //
        // emez deliberately does *not* auto-focus new toplevels — that
        // would be a focus-stealing policy unlike real-world Mutter (and
        // even stricter than KWin's default `low` prevention). Clients
        // that need keyboard focus (e.g. wl-copy falling back to
        // wl_data_device) request it via `xdg_activation_v1.activate`
        // with a token from `XDG_ACTIVATION_TOKEN` — see
        // `XdgActivationHandler::request_activation` below.
        surface.send_configure();
    }

    fn new_popup(&mut self, _: PopupSurface, _: PositionerState) {}
    fn grab(&mut self, _: PopupSurface, _: wl_seat::WlSeat, _: Serial) {}
    fn reposition_request(&mut self, _: PopupSurface, _: PositionerState, _: u32) {}
}

impl DataDeviceHandler for Emez {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl WaylandDndGrabHandler for Emez {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        _seat: smithay::input::Seat<Self>,
        _serial: Serial,
        _type_: GrabType,
    ) {
        // emez is a dumb host — we never accept DnD on behalf of any client.
        source.cancel();
    }
}

impl DndGrabHandler for Emez {}

impl SelectionHandler for Emez {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        seat: Seat<Self>,
    ) {
        // Forward wayland-side selection changes to XWayland so outside
        // X clients (xclip) see them. No-op when XWayland isn't running.
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, source.as_ref().map(|s| s.mime_types())) {
                tracing::warn!(?err, ?ty, "emez: forward wayland → X new_selection");
            }
        }

        // Clipboard-manager buffering: when a client publishes a wayland
        // selection, drain every mime offer into memory via
        // `request_data_device_client_selection`, then replace the
        // client-owned selection with a compositor-owned one backed by
        // our buffer. End result: the originating client can exit
        // (e.g. `wl-copy --foreground` with no daemon fork) and the
        // selection survives — exactly what X11's CLIPBOARD_MANAGER /
        // SAVE_TARGETS does, but implemented inside the compositor.
        if ty == SelectionTarget::Clipboard && self.clipboard_manager_enabled {
            match source {
                Some(src) => {
                    let mimes = src.mime_types();
                    tracing::debug!("new_selection(Clipboard) mimes={:?}", mimes);
                    if mimes.is_empty() {
                        self.clipboard_buffer.clear();
                        self.clipboard_mimes.clear();
                    } else {
                        // smithay calls `SelectionHandler::new_selection`
                        // *before* it writes the new selection into
                        // `seat_data` (see smithay's
                        // wayland/selection/data_device/device.rs) —
                        // so `request_data_device_client_selection`
                        // would return `NoSelection` if we called it
                        // right here. Defer to the next event-loop
                        // idle tick: by then smithay has finished the
                        // dispatch, the new selection is in place,
                        // and the subsequent `set_data_device_selection`
                        // replacement is legal.
                        let seat = seat.clone();
                        self.loop_handle.insert_idle(move |state| {
                            drain_and_take_over_clipboard(state, &seat, mimes);
                        });
                    }
                }
                None => {
                    self.clipboard_buffer.clear();
                    self.clipboard_mimes.clear();
                }
            }
        }
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        // Compositor-owned selection first: if the clipboard manager has
        // this mime buffered, we're the data source — write it directly.
        // This must be mutually exclusive with the XWM path below, since
        // both would write to the same pipe and race.
        if ty == SelectionTarget::Clipboard {
            if let Some(bytes) = self.clipboard_buffer.get(&mime_type).cloned() {
                write_bytes_to_pipe(fd, bytes);
                return;
            }
        }
        // Otherwise the selection originates from X (injected via
        // `XwmHandler::new_selection` into smithay's seat data); let XWM
        // pull the bytes from its X client and drive the fd.
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                tracing::warn!(?err, ?ty, "emez: forward wayland → X send_selection");
            }
            return;
        }
        drop(fd);
    }
}

/// Drain every mime from a client-owned clipboard source into
/// `self.clipboard_buffer`, then replace the live selection with a
/// compositor-owned one so the client can safely exit.
fn drain_and_take_over_clipboard(emez: &mut Emez, seat: &Seat<Emez>, mimes: Vec<String>) {
    use smithay::reexports::calloop::{generic::Generic, Interest, Mode, PostAction};
    use smithay::wayland::selection::data_device::request_data_device_client_selection;

    tracing::debug!("drain_and_take_over_clipboard start mimes={:?}", mimes);
    emez.clipboard_buffer.clear();
    emez.clipboard_mimes = mimes.clone();

    // Per-mime small state carried into the async reader.
    for mime in mimes {
        // pipe2(O_CLOEXEC): let the source write to `w`, we read from `r`.
        let mut fds = [0i32; 2];
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if rc != 0 {
            tracing::warn!("clipboard-manager pipe2 failed: {}", std::io::Error::last_os_error());
            continue;
        }
        // SAFETY: pipe2 returned 0 → both fds are valid and owned.
        let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        if let Err(e) =
            request_data_device_client_selection(seat, mime.clone(), write_fd)
        {
            tracing::warn!("request_data_device_client_selection({mime}) failed: {e:?}");
            continue;
        }

        // Seed an empty slot so concurrent paste requests before drain
        // completes don't panic on `get`.
        emez.clipboard_buffer.insert(mime.clone(), Vec::new());

        // SAFETY: read_fd owns a valid pipe read end; File takes ownership.
        let file = unsafe { std::fs::File::from_raw_fd(read_fd.into_raw_fd()) };
        let mime_owned = mime.clone();
        let src = Generic::new(file, Interest::READ, Mode::Level);
        if let Err(e) = emez.loop_handle.insert_source(src, move |_, file, state| {
            let mut chunk = [0u8; 65536];
            loop {
                // SAFETY: chunk is a valid mutable buffer; fd is owned by `file`.
                let n = unsafe {
                    libc::read(
                        file.as_raw_fd(),
                        chunk.as_mut_ptr().cast(),
                        chunk.len(),
                    )
                };
                if n > 0 {
                    if let Some(buf) = state.clipboard_buffer.get_mut(&mime_owned) {
                        buf.extend_from_slice(&chunk[..n as usize]);
                    }
                } else if n == 0 {
                    // EOF. Done reading this mime.
                    return Ok(PostAction::Remove);
                } else {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(PostAction::Continue);
                    }
                    tracing::warn!("clipboard-manager read({mime_owned}) error: {err}");
                    return Ok(PostAction::Remove);
                }
            }
        }) {
            tracing::warn!("clipboard-manager insert_source failed: {e}");
        }
    }

    // Take over the selection with a compositor-owned entry. smithay
    // will cancel() the previous (client) source immediately, so a
    // `wl-copy --foreground` waiting on cancel will exit right away
    // — but we've already kicked off the async reads above, and the
    // reads use OS-level pipes that remain valid even after the wayland
    // source is gone. The bytes keep flowing until wl-copy closes its
    // end of every pipe.
    use smithay::wayland::selection::data_device::set_data_device_selection;
    set_data_device_selection(&emez.display_handle, seat, emez.clipboard_mimes.clone(), ());
    tracing::debug!("drain_and_take_over_clipboard: compositor selection installed");

    // Now that the selection is compositor-owned, hand focus back to
    // the primary surface so smithay re-broadcasts the fresh selection
    // to it. Without this step, focus stays on the soon-to-exit wl-copy
    // and the primary client (emskin) never receives `.selection(offer)`.
    // smithay's keyboard handle doesn't auto-reset focus when the
    // focused surface dies — we have to drive it. Matches real
    // compositor behaviour: a focus-stealing short-lived client
    // returns focus to where it came from after finishing its task.
    if let Some(primary) = emez.primary_fallback_focus.clone() {
        if primary.is_alive() {
            if let Some(keyboard) = emez.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                keyboard.set_focus(emez, Some(primary), serial);
            }
        }
    }
}

/// Blocking write of `bytes` to `fd`, then close. Used from
/// `SelectionHandler::send_selection` for compositor-owned selections.
/// Spawned in a detached thread so we don't stall the event loop when
/// the peer is slow to read.
fn write_bytes_to_pipe(fd: OwnedFd, bytes: Vec<u8>) {
    std::thread::spawn(move || {
        use std::io::Write;
        // SAFETY: fd is a valid OwnedFd; File takes ownership.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        if let Err(e) = file.write_all(&bytes) {
            tracing::debug!("compositor selection write failed: {e}");
        }
        // File::drop closes fd, signalling EOF to reader.
    });
}

impl PrimarySelectionHandler for Emez {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl WlrDataControlHandler for Emez {
    fn data_control_state(&mut self) -> &mut WlrDataControlState {
        &mut self.wlr_data_control_state
    }
}

impl ExtDataControlHandler for Emez {
    fn data_control_state(&mut self) -> &mut ExtDataControlState {
        &mut self.ext_data_control_state
    }
}

impl XdgActivationHandler for Emez {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        _token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Test host: any known token is enough — focus the target
        // surface. Real compositors vet the token's issuing context
        // (app-id, serial, requesting surface) before accepting; emez
        // deliberately doesn't, because its job is to give clients a
        // protocol-legal focus path without emez inventing its own
        // policy.
        //
        // The token is *not* removed after use. Tests that do back-to-
        // back wl-copy / wl-paste reuse the same pre-seeded token, so
        // keeping it alive matches the "permissive test host" stance.
        tracing::debug!("xdg_activation_v1.activate → focus {}", surface.id());

        // Record primary fallback target on the first activate, or when
        // the previous one has died (primary client crash + relaunch
        // should still work). Don't overwrite a live primary with a
        // later short-lived activate — otherwise wl-copy would steal
        // the "primary" label from emskin and the drain-end focus
        // handoff would try to return focus to the already-exiting
        // wl-copy surface.
        let primary_gone = self
            .primary_fallback_focus
            .as_ref()
            .map_or(true, |s| !s.is_alive());
        if primary_gone {
            self.primary_fallback_focus = Some(surface.clone());
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            keyboard.set_focus(self, Some(surface), serial);
        }
    }
}

delegate_compositor!(Emez);
delegate_shm!(Emez);
delegate_seat!(Emez);
delegate_output!(Emez);
delegate_xdg_shell!(Emez);
delegate_xdg_activation!(Emez);
delegate_data_device!(Emez);
delegate_primary_selection!(Emez);
delegate_data_control!(Emez);
delegate_ext_data_control!(Emez);
