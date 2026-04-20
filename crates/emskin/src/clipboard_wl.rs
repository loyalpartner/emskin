//! wl_data_device fallback for hosts without wlr/ext data-control.
//!
//! Piggybacks on winit's existing Wayland connection (shared via
//! `Backend::from_foreign_display`) so selection events fire whenever
//! emskin's window has host keyboard focus. Covers the main user
//! scenario: user is actively interacting with emskin when copy/paste
//! happens on the host, so focus is on emskin at the moment.
//!
//! Unlike data-control, `wl_data_device.set_selection` requires an
//! input-event serial. We bind our own `wl_keyboard` on the host seat
//! to snoop the latest focus/key serial; subsequent `set_selection`
//! calls reuse it.
//!
//! Primary selection (middle-click) is not implemented here — the bug
//! this backend addresses is the CLIPBOARD path. Primary can be added
//! later via `zwp_primary_selection_device_manager_v1` when needed.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::Arc;

use wayland_client::backend::{Backend, ObjectData, ObjectId};
use wayland_client::protocol::{
    wl_data_device::{self, WlDataDevice},
    wl_data_device_manager::{self, WlDataDeviceManager},
    wl_data_offer::{self, WlDataOffer},
    wl_data_source::{self, WlDataSource},
    wl_keyboard::{self, WlKeyboard},
    wl_registry, wl_seat,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use smithay::wayland::selection::SelectionTarget;

use crate::clipboard::{ClipboardBackend, ClipboardEvent};

/// Role tag stored as user data on wl_data_source.
#[derive(Clone, Debug)]
enum SourceRole {
    Clipboard,
}

/// Internal state for wayland-client Dispatch callbacks.
struct State {
    manager: Option<WlDataDeviceManager>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<WlKeyboard>,
    device: Option<WlDataDevice>,

    /// Latest input-event serial from wl_keyboard (enter / key / modifiers).
    /// Compositors require this for set_selection; without it we can't
    /// publish our selection to the host.
    latest_serial: Option<u32>,

    clipboard_offer: Option<WlDataOffer>,
    pending_offers: HashMap<ObjectId, Vec<String>>,
    clipboard_source: Option<WlDataSource>,

    events: Vec<ClipboardEvent>,

    /// Anti-loop: number of host-selection echo events to suppress
    /// after we call set_selection ourselves.
    suppress_clipboard: u32,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(o) = self.clipboard_offer.take() {
            o.destroy();
        }
        if let Some(s) = self.clipboard_source.take() {
            s.destroy();
        }
        if let Some(d) = self.device.take() {
            d.release();
        }
        if let Some(k) = self.keyboard.take() {
            k.release();
        }
    }
}

pub struct WlDataDeviceProxy {
    connection: Connection,
    queue: EventQueue<State>,
    inner: State,
    /// Placeholder fd handed to calloop via `connection_fd()`. We don't
    /// own the Wayland fd (winit does), so calloop can't wake us on
    /// Wayland events — instead `dispatch()` is driven from tick.rs.
    /// This pipe-read end is never written to, so the calloop source
    /// registered on it will simply never fire, which is what we want.
    dummy_fd: OwnedFd,
    /// Write end of the placeholder pipe. Kept open *on purpose*:
    /// closing it would flip `dummy_fd` into a permanent `POLLHUP`
    /// state, and calloop's level-triggered source would then fire on
    /// every poll iteration, busy-looping emskin's event loop. Holding
    /// the write end keeps the pipe "normal" — empty, not readable,
    /// never signalled.
    _pipe_write: OwnedFd,
}

impl WlDataDeviceProxy {
    /// Create a proxy that shares `display_ptr` (winit's `wl_display`)
    /// as its underlying Wayland connection. Returns None if the host
    /// doesn't expose `wl_data_device_manager` + `wl_seat` or any
    /// required roundtrip fails.
    ///
    /// # Safety
    /// `display_ptr` must be a valid `*mut wl_display` that stays live
    /// for the proxy's entire lifetime. emskin enforces this by keeping
    /// `state.backend` (which owns the wl_display) alive for the whole
    /// compositor run.
    pub unsafe fn new(display_ptr: *mut c_void) -> Option<Self> {
        // SAFETY: caller guarantees display_ptr is valid.
        let backend = unsafe { Backend::from_foreign_display(display_ptr.cast()) };
        let connection = Connection::from_backend(backend);
        let mut queue = connection.new_event_queue::<State>();
        let qh = queue.handle();

        let _registry = connection.display().get_registry(&qh, ());

        let mut inner = State {
            manager: None,
            seat: None,
            keyboard: None,
            device: None,
            latest_serial: None,
            clipboard_offer: None,
            pending_offers: HashMap::new(),
            clipboard_source: None,
            events: Vec::new(),
            suppress_clipboard: 0,
        };

        // Roundtrip 1: discover globals.
        if let Err(e) = queue.roundtrip(&mut inner) {
            tracing::debug!("wl_data_device roundtrip 1 failed: {e}");
            return None;
        }

        if let (Some(ref manager), Some(ref seat)) = (&inner.manager, &inner.seat) {
            inner.device = Some(manager.get_data_device(seat, &qh, ()));
            inner.keyboard = Some(seat.get_keyboard(&qh, ()));
        }

        if inner.device.is_none() {
            tracing::debug!(
                "Host has wl_data_device_manager? {}, wl_seat? {}",
                inner.manager.is_some(),
                inner.seat.is_some()
            );
            return None;
        }

        // Roundtrip 2: let initial selection event arrive if emskin's
        // surface already has keyboard focus. On most hosts it doesn't
        // (window just created), so this typically no-ops.
        if let Err(e) = queue.roundtrip(&mut inner) {
            tracing::warn!("wl_data_device roundtrip 2 failed: {e}");
            return None;
        }

        // Placeholder fd pair. See comments on `dummy_fd` / `_pipe_write`.
        let mut pipe_fds = [0i32; 2];
        // SAFETY: pipe2 fills both fds or returns -1; we check both.
        let rc = unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) };
        if rc != 0 {
            tracing::warn!("wl_data_device: pipe2 failed, cannot allocate placeholder fd");
            return None;
        }
        // SAFETY: pipe2 returned 0, so both fds are valid and owned by us.
        let dummy_fd = unsafe { OwnedFd::from_raw_fd(pipe_fds[0]) };
        // SAFETY: same — valid fd on pipe2 success, stored for the
        // proxy's lifetime so `dummy_fd` never flips to POLLHUP.
        let pipe_write = unsafe { OwnedFd::from_raw_fd(pipe_fds[1]) };

        tracing::info!(
            "Clipboard sync initialized (wl_data_device_manager v{}, shared connection)",
            inner.manager.as_ref().map(|m| m.version()).unwrap_or(0)
        );

        Some(Self {
            connection,
            queue,
            inner,
            dummy_fd,
            _pipe_write: pipe_write,
        })
    }

    fn flush(&self) {
        if let Err(e) = self.connection.flush() {
            tracing::warn!("wl_data_device flush error: {e}");
        }
    }
}

impl ClipboardBackend for WlDataDeviceProxy {
    fn dispatch(&mut self) {
        // Winit owns the connection fd and already calls prepare_read +
        // read on its own dispatch cycle. By the time we get here,
        // events for our queue are already in libwayland-client's
        // internal per-queue buffer; we just need to drain them into
        // our `State` via dispatch callbacks.
        if let Err(e) = self.queue.dispatch_pending(&mut self.inner) {
            tracing::warn!("wl_data_device dispatch_pending error: {e}");
        }
    }

    fn take_events(&mut self) -> Vec<ClipboardEvent> {
        std::mem::take(&mut self.inner.events)
    }

    fn connection_fd(&self) -> BorrowedFd<'_> {
        self.dummy_fd.as_fd()
    }

    fn receive_from_host(&mut self, target: SelectionTarget, mime_type: &str, fd: OwnedFd) {
        if target != SelectionTarget::Clipboard {
            // Primary selection not supported on this backend yet.
            return;
        }
        if let Some(ref offer) = self.inner.clipboard_offer {
            offer.receive(mime_type.to_string(), fd.as_fd());
            self.flush();
        } else {
            tracing::warn!("wl_data_device receive_from_host: no active offer, fd dropped");
        }
    }

    fn set_host_selection(&mut self, target: SelectionTarget, mime_types: &[String]) {
        if target != SelectionTarget::Clipboard {
            return;
        }
        let Some(ref manager) = self.inner.manager else {
            return;
        };
        let Some(ref device) = self.inner.device else {
            return;
        };
        let Some(serial) = self.inner.latest_serial else {
            tracing::debug!("wl_data_device set_host_selection: no input serial yet, deferring");
            return;
        };

        let qh = self.queue.handle();
        let source = manager.create_data_source(&qh, SourceRole::Clipboard);
        for mime in mime_types {
            source.offer(mime.clone());
        }
        device.set_selection(Some(&source), serial);
        if let Some(old) = self.inner.clipboard_source.replace(source) {
            old.destroy();
        }
        self.inner.suppress_clipboard += 1;
        self.flush();
    }

    fn clear_host_selection(&mut self, target: SelectionTarget) {
        if target != SelectionTarget::Clipboard {
            return;
        }
        let Some(ref device) = self.inner.device else {
            return;
        };
        let Some(serial) = self.inner.latest_serial else {
            return;
        };
        device.set_selection(None, serial);
        if let Some(old) = self.inner.clipboard_source.take() {
            old.destroy();
        }
        self.inner.suppress_clipboard += 1;
        self.flush();
    }
}

// ---------------------------------------------------------------------------
// State helpers
// ---------------------------------------------------------------------------

impl State {
    fn on_selection(&mut self, new_offer: Option<WlDataOffer>) {
        let mime_types = new_offer
            .as_ref()
            .and_then(|o| self.pending_offers.remove(&o.id()))
            .unwrap_or_default();

        // Any stale pending offers (leftovers) can be dropped — wayland
        // sends a data_offer for each selection change and the selection
        // event finalizes which offer applies.
        self.pending_offers.clear();

        if let Some(old) = self.clipboard_offer.take() {
            old.destroy();
        }
        self.clipboard_offer = new_offer;

        if self.suppress_clipboard > 0 {
            self.suppress_clipboard -= 1;
            return;
        }

        self.events.push(ClipboardEvent::HostSelectionChanged {
            target: SelectionTarget::Clipboard,
            mime_types,
        });
    }

    fn on_source_send(&mut self, mime_type: String, fd: OwnedFd) {
        self.events.push(ClipboardEvent::HostSendRequest {
            id: 0,
            target: SelectionTarget::Clipboard,
            mime_type,
            write_fd: fd,
            read_fd: None,
        });
    }

    fn on_source_cancelled(&mut self) {
        if let Some(s) = self.clipboard_source.take() {
            s.destroy();
        }
        self.events.push(ClipboardEvent::SourceCancelled {
            target: SelectionTarget::Clipboard,
        });
    }
}

// ---------------------------------------------------------------------------
// Dispatch impls
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_data_device_manager" if state.manager.is_none() => {
                    let bound =
                        registry.bind::<WlDataDeviceManager, _, _>(name, version.min(3), qh, ());
                    state.manager = Some(bound);
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(5), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_keyboard::Event;
        // Cache any serial we see — set_selection requires one from an
        // input event. Enter / key / modifiers all carry a serial that
        // the compositor accepts.
        match event {
            Event::Enter { serial, .. } => state.latest_serial = Some(serial),
            Event::Leave { serial, .. } => state.latest_serial = Some(serial),
            Event::Key { serial, .. } => state.latest_serial = Some(serial),
            Event::Modifiers { serial, .. } => state.latest_serial = Some(serial),
            _ => {}
        }
    }
}

impl Dispatch<WlDataDeviceManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device::Event;
        match event {
            Event::DataOffer { id } => {
                state.pending_offers.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => state.on_selection(id),
            // DnD events (Enter/Leave/Motion/Drop) not handled — emskin
            // doesn't bridge drag-and-drop between host and embedded
            // clients (yet).
            _ => {}
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        assert_eq!(
            opcode,
            wl_data_device::EVT_DATA_OFFER_OPCODE,
            "unexpected child-creating opcode for wl_data_device"
        );
        qh.make_data::<WlDataOffer, ()>(())
    }
}

impl Dispatch<WlDataOffer, ()> for State {
    fn event(
        state: &mut Self,
        offer: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if let Some(pending) = state.pending_offers.get_mut(&offer.id()) {
                pending.push(mime_type);
            }
        }
    }
}

impl Dispatch<WlDataSource, SourceRole> for State {
    fn event(
        state: &mut Self,
        _: &WlDataSource,
        event: wl_data_source::Event,
        _role: &SourceRole,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => state.on_source_send(mime_type, fd),
            wl_data_source::Event::Cancelled => state.on_source_cancelled(),
            // Target / Action / DndFinished / DndDropPerformed are DnD
            // concerns — ignore for selection-only usage.
            _ => {}
        }
    }
}
