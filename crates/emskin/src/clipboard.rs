//! Clipboard synchronization between host compositor and internal applications.
//!
//! Uses `ext_data_control_v1` (preferred) or `zwlr_data_control_manager_v1` (fallback)
//! to monitor and control the host's clipboard without requiring keyboard focus.
//! Bridges clipboard content bidirectionally between the host compositor and
//! internal Wayland clients (Emacs, embedded apps).

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use wayland_client::backend::{ObjectData, ObjectId};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use smithay::wayland::selection::SelectionTarget;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Trait for host clipboard backends (Wayland data_control or X11 selection).
pub trait ClipboardBackend {
    /// Read pending events from the host connection.
    fn dispatch(&mut self);

    /// Drain queued clipboard events for processing by the compositor.
    fn take_events(&mut self) -> Vec<ClipboardEvent>;

    /// Borrow the connection fd for calloop registration.
    fn connection_fd(&self) -> std::os::fd::BorrowedFd<'_>;

    /// Forward host clipboard data to an internal client's fd.
    fn receive_from_host(
        &mut self,
        target: SelectionTarget,
        mime_type: &str,
        fd: std::os::fd::OwnedFd,
    );

    /// Advertise an internal client's selection to the host compositor.
    fn set_host_selection(&mut self, target: SelectionTarget, mime_types: &[String]);

    /// Clear our selection on the host.
    fn clear_host_selection(&mut self, target: SelectionTarget);

    /// Complete an outgoing transfer (pipe data fully read by calloop).
    /// Default is no-op (only X11 backend needs this).
    fn complete_outgoing(&mut self, _id: u64, _data: Vec<u8>) {}
}

/// Events from host clipboard that need processing by the compositor.
pub enum ClipboardEvent {
    /// Host selection changed. Empty `mime_types` means selection was cleared.
    HostSelectionChanged {
        target: SelectionTarget,
        mime_types: Vec<String>,
    },
    /// Host requests data from our selection (external paste of internal copy).
    /// `write_fd` goes to smithay (client writes data here).
    /// `read_fd` is `Some` for X11 path (register with calloop), `None` for Wayland.
    HostSendRequest {
        id: u64,
        target: SelectionTarget,
        mime_type: String,
        write_fd: OwnedFd,
        read_fd: Option<OwnedFd>,
    },
    /// Our source on the host was cancelled (replaced by another app).
    SourceCancelled { target: SelectionTarget },
}

// ---------------------------------------------------------------------------
// Protocol abstraction — enum wrappers over ext / wlr variants
// ---------------------------------------------------------------------------

enum DataControlManager {
    ExtDataControl(ExtDataControlManagerV1),
    WlrDataControl(ZwlrDataControlManagerV1),
}

impl DataControlManager {
    fn create_data_source(
        &self,
        qh: &QueueHandle<ClipboardState>,
        role: SourceRole,
    ) -> DataControlSource {
        match self {
            Self::ExtDataControl(m) => {
                DataControlSource::ExtDataControl(m.create_data_source(qh, role))
            }
            Self::WlrDataControl(m) => {
                DataControlSource::WlrDataControl(m.create_data_source(qh, role))
            }
        }
    }

    fn get_data_device(
        &self,
        seat: &wl_seat::WlSeat,
        qh: &QueueHandle<ClipboardState>,
    ) -> DataControlDevice {
        match self {
            Self::ExtDataControl(m) => {
                DataControlDevice::ExtDataControl(m.get_data_device(seat, qh, ()))
            }
            Self::WlrDataControl(m) => {
                DataControlDevice::WlrDataControl(m.get_data_device(seat, qh, ()))
            }
        }
    }

    fn protocol_name(&self) -> &'static str {
        match self {
            Self::ExtDataControl(_) => "ext_data_control_v1",
            Self::WlrDataControl(_) => "zwlr_data_control_v1",
        }
    }
}

enum DataControlDevice {
    ExtDataControl(ExtDataControlDeviceV1),
    WlrDataControl(ZwlrDataControlDeviceV1),
}

impl DataControlDevice {
    fn set_selection(&self, source: Option<&DataControlSource>) {
        match (self, source) {
            (Self::ExtDataControl(d), Some(DataControlSource::ExtDataControl(s))) => {
                d.set_selection(Some(s));
            }
            (Self::ExtDataControl(d), None) => d.set_selection(None),
            (Self::WlrDataControl(d), Some(DataControlSource::WlrDataControl(s))) => {
                d.set_selection(Some(s));
            }
            (Self::WlrDataControl(d), None) => d.set_selection(None),
            _ => unreachable!("protocol variant mismatch"),
        }
    }

    fn set_primary_selection(&self, source: Option<&DataControlSource>) {
        match (self, source) {
            (Self::ExtDataControl(d), Some(DataControlSource::ExtDataControl(s))) => {
                d.set_primary_selection(Some(s));
            }
            (Self::ExtDataControl(d), None) => d.set_primary_selection(None),
            (Self::WlrDataControl(d), Some(DataControlSource::WlrDataControl(s))) => {
                d.set_primary_selection(Some(s));
            }
            (Self::WlrDataControl(d), None) => d.set_primary_selection(None),
            _ => unreachable!("protocol variant mismatch"),
        }
    }
}

enum DataControlOffer {
    ExtDataControl(ExtDataControlOfferV1),
    WlrDataControl(ZwlrDataControlOfferV1),
}

impl DataControlOffer {
    fn id(&self) -> ObjectId {
        match self {
            Self::ExtDataControl(o) => o.id(),
            Self::WlrDataControl(o) => o.id(),
        }
    }

    fn receive(&self, mime_type: String, fd: BorrowedFd<'_>) {
        match self {
            Self::ExtDataControl(o) => o.receive(mime_type, fd),
            Self::WlrDataControl(o) => o.receive(mime_type, fd),
        }
    }

    fn destroy(&self) {
        match self {
            Self::ExtDataControl(o) => o.destroy(),
            Self::WlrDataControl(o) => o.destroy(),
        }
    }
}

enum DataControlSource {
    ExtDataControl(ExtDataControlSourceV1),
    WlrDataControl(ZwlrDataControlSourceV1),
}

impl DataControlSource {
    fn offer(&self, mime_type: String) {
        match self {
            Self::ExtDataControl(s) => s.offer(mime_type),
            Self::WlrDataControl(s) => s.offer(mime_type),
        }
    }

    fn destroy(&self) {
        match self {
            Self::ExtDataControl(s) => s.destroy(),
            Self::WlrDataControl(s) => s.destroy(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Role tag stored as user data on data control source objects.
#[derive(Clone, Debug)]
enum SourceRole {
    Clipboard,
    Primary,
}

/// State for wayland-client Dispatch callbacks.
struct ClipboardState {
    manager: Option<DataControlManager>,
    device: Option<DataControlDevice>,
    seat: Option<wl_seat::WlSeat>,

    clipboard_offer: Option<DataControlOffer>,
    primary_offer: Option<DataControlOffer>,

    /// Offers being assembled (MIME types accumulating via offer events).
    pending_offers: HashMap<ObjectId, Vec<String>>,

    clipboard_source: Option<DataControlSource>,
    primary_source: Option<DataControlSource>,

    events: Vec<ClipboardEvent>,

    /// Anti-loop: number of host selection echo events to suppress.
    /// Incremented each time we call set_selection/set_primary_selection on
    /// the host; decremented when the corresponding echo event arrives.
    /// A counter (not a bool) handles clients that set selection multiple
    /// times rapidly (e.g. Firefox sets twice — once without SAVE_TARGETS,
    /// then again with it).
    suppress_clipboard: u32,
    suppress_primary: u32,
}

impl Drop for ClipboardState {
    fn drop(&mut self) {
        for offer in [self.clipboard_offer.take(), self.primary_offer.take()]
            .into_iter()
            .flatten()
        {
            offer.destroy();
        }
        for source in [self.clipboard_source.take(), self.primary_source.take()]
            .into_iter()
            .flatten()
        {
            source.destroy();
        }
    }
}

// ---------------------------------------------------------------------------
// ClipboardState — shared event handlers (protocol-agnostic logic)
// ---------------------------------------------------------------------------

impl ClipboardState {
    fn source_and_suppress(
        &mut self,
        target: SelectionTarget,
    ) -> (&mut Option<DataControlSource>, &mut u32) {
        match target {
            SelectionTarget::Clipboard => {
                (&mut self.clipboard_source, &mut self.suppress_clipboard)
            }
            SelectionTarget::Primary => (&mut self.primary_source, &mut self.suppress_primary),
        }
    }

    fn on_data_offer(&mut self, id: ObjectId) {
        self.pending_offers.insert(id, Vec::new());
    }

    fn on_offer_mime(&mut self, offer_id: ObjectId, mime_type: String) {
        if let Some(pending) = self.pending_offers.get_mut(&offer_id) {
            pending.push(mime_type);
        }
    }

    fn on_selection(&mut self, target: SelectionTarget, new_offer: Option<DataControlOffer>) {
        let mime_types = new_offer
            .as_ref()
            .and_then(|o| self.pending_offers.remove(&o.id()))
            .unwrap_or_default();

        // Selection event finalizes the offer sequence — any remaining
        // pending_offers entries are stale (e.g. orphaned DnD offers).
        self.pending_offers.clear();

        let (offer_slot, suppress) = match target {
            SelectionTarget::Clipboard => (&mut self.clipboard_offer, &mut self.suppress_clipboard),
            SelectionTarget::Primary => (&mut self.primary_offer, &mut self.suppress_primary),
        };

        if let Some(old) = offer_slot.take() {
            old.destroy();
        }
        *offer_slot = new_offer;

        if *suppress > 0 {
            *suppress -= 1;
            return;
        }

        self.events
            .push(ClipboardEvent::HostSelectionChanged { target, mime_types });
    }

    fn on_device_finished(&mut self) {
        tracing::warn!("Data control device finished (seat destroyed?)");
        self.device = None;
    }

    fn on_source_send(&mut self, role: &SourceRole, mime_type: String, fd: OwnedFd) {
        let target = match role {
            SourceRole::Clipboard => SelectionTarget::Clipboard,
            SourceRole::Primary => SelectionTarget::Primary,
        };
        self.events.push(ClipboardEvent::HostSendRequest {
            id: 0,
            target,
            mime_type,
            write_fd: fd,
            read_fd: None,
        });
    }

    fn on_source_cancelled(&mut self, role: &SourceRole) {
        let (target, source_slot) = match role {
            SourceRole::Clipboard => (SelectionTarget::Clipboard, &mut self.clipboard_source),
            SourceRole::Primary => (SelectionTarget::Primary, &mut self.primary_source),
        };
        if let Some(s) = source_slot.take() {
            s.destroy();
        }
        self.events.push(ClipboardEvent::SourceCancelled { target });
    }
}

// ---------------------------------------------------------------------------
// ClipboardProxy — public API
// ---------------------------------------------------------------------------

/// Proxy for clipboard synchronization with the host compositor.
pub struct ClipboardProxy {
    connection: Connection,
    queue: EventQueue<ClipboardState>,
    inner: ClipboardState,
}

impl ClipboardProxy {
    /// Connect to the host compositor and set up data control protocol.
    ///
    /// Prefers `ext_data_control_v1`, falls back to `zwlr_data_control_manager_v1`.
    /// Returns `None` if neither is supported.
    pub fn new() -> Option<Self> {
        let conn = match Connection::connect_to_env() {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("Cannot connect to host Wayland for clipboard: {e}");
                return None;
            }
        };
        let mut queue = conn.new_event_queue::<ClipboardState>();
        let qh = queue.handle();

        let _registry = conn.display().get_registry(&qh, ());

        let mut state = ClipboardState {
            manager: None,
            device: None,
            seat: None,
            clipboard_offer: None,
            primary_offer: None,
            pending_offers: HashMap::new(),
            clipboard_source: None,
            primary_source: None,
            events: Vec::new(),
            suppress_clipboard: 0,
            suppress_primary: 0,
        };

        // Roundtrip 1: discover globals (manager + seat)
        if let Err(e) = queue.roundtrip(&mut state) {
            tracing::warn!("Clipboard roundtrip 1 failed: {e}");
            return None;
        }

        // Create data control device from manager + seat
        if let (Some(ref manager), Some(ref seat)) = (&state.manager, &state.seat) {
            state.device = Some(manager.get_data_device(seat, &qh));
        }

        if state.device.is_none() {
            tracing::warn!("Host supports neither ext_data_control_v1 nor zwlr_data_control_v1");
            return None;
        }

        let protocol_name = state
            .manager
            .as_ref()
            .map(|m| m.protocol_name())
            .unwrap_or("unknown");

        // Roundtrip 2: receive initial selection events
        if let Err(e) = queue.roundtrip(&mut state) {
            tracing::warn!("Clipboard roundtrip 2 failed: {e}");
            return None;
        }

        tracing::info!("Clipboard sync initialized ({protocol_name})");
        Some(Self {
            connection: conn,
            queue,
            inner: state,
        })
    }

    /// Read pending events from the host Wayland connection.
    pub fn dispatch(&mut self) {
        if let Some(guard) = self.queue.prepare_read() {
            if let Err(e) = guard.read() {
                tracing::warn!("clipboard read error: {e}");
            }
        }
        if let Err(e) = self.queue.dispatch_pending(&mut self.inner) {
            tracing::warn!("clipboard dispatch error: {e}");
        }
        // No flush here — this is a read path. Write paths
        // (receive_from_host, set_host_selection, clear_host_selection)
        // flush after sending their requests.
    }

    /// Drain queued clipboard events for processing by the compositor.
    pub fn take_events(&mut self) -> Vec<ClipboardEvent> {
        std::mem::take(&mut self.inner.events)
    }

    /// Borrow the connection fd for calloop registration.
    pub fn connection_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.connection.as_fd()
    }

    fn flush(&self) {
        if let Err(e) = self.connection.flush() {
            tracing::warn!("clipboard flush error: {e}");
        }
    }

    /// Forward host clipboard data to an internal client's fd.
    pub fn receive_from_host(&mut self, target: SelectionTarget, mime_type: &str, fd: OwnedFd) {
        let offer = match target {
            SelectionTarget::Clipboard => self.inner.clipboard_offer.as_ref(),
            SelectionTarget::Primary => self.inner.primary_offer.as_ref(),
        };
        if let Some(offer) = offer {
            offer.receive(mime_type.to_string(), fd.as_fd());
            self.flush();
        } else {
            tracing::warn!("receive_from_host: no active {target:?} offer, fd dropped");
        }
    }

    /// Advertise an internal client's selection to the host compositor.
    pub fn set_host_selection(&mut self, target: SelectionTarget, mime_types: &[String]) {
        let Some(ref manager) = self.inner.manager else {
            return;
        };
        let Some(ref device) = self.inner.device else {
            return;
        };

        let qh = self.queue.handle();
        let role = match target {
            SelectionTarget::Clipboard => SourceRole::Clipboard,
            SelectionTarget::Primary => SourceRole::Primary,
        };

        let source = manager.create_data_source(&qh, role);
        for mime in mime_types {
            source.offer(mime.clone());
        }

        match target {
            SelectionTarget::Clipboard => device.set_selection(Some(&source)),
            SelectionTarget::Primary => device.set_primary_selection(Some(&source)),
        }
        let (source_slot, suppress) = self.inner.source_and_suppress(target);
        if let Some(old) = source_slot.replace(source) {
            old.destroy();
        }
        *suppress += 1;
        self.flush();
    }

    /// Clear our selection on the host (when an internal client clears selection).
    pub fn clear_host_selection(&mut self, target: SelectionTarget) {
        let Some(ref device) = self.inner.device else {
            return;
        };

        match target {
            SelectionTarget::Clipboard => device.set_selection(None),
            SelectionTarget::Primary => device.set_primary_selection(None),
        }
        let (source_slot, suppress) = self.inner.source_and_suppress(target);
        if let Some(old) = source_slot.take() {
            old.destroy();
        }
        *suppress += 1;
        self.flush();
    }
}

impl ClipboardBackend for ClipboardProxy {
    fn dispatch(&mut self) {
        self.dispatch();
    }

    fn take_events(&mut self) -> Vec<ClipboardEvent> {
        self.take_events()
    }

    fn connection_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.connection_fd()
    }

    fn receive_from_host(
        &mut self,
        target: SelectionTarget,
        mime_type: &str,
        fd: std::os::fd::OwnedFd,
    ) {
        self.receive_from_host(target, mime_type, fd);
    }

    fn set_host_selection(&mut self, target: SelectionTarget, mime_types: &[String]) {
        self.set_host_selection(target, mime_types);
    }

    fn clear_host_selection(&mut self, target: SelectionTarget) {
        self.clear_host_selection(target);
    }
}

// ---------------------------------------------------------------------------
// Dispatch — wl_registry (shared, binds ext or wlr manager)
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for ClipboardState {
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
                "ext_data_control_manager_v1" if state.manager.is_none() => {
                    let proxy = registry.bind::<ExtDataControlManagerV1, _, _>(name, 1, qh, ());
                    state.manager = Some(DataControlManager::ExtDataControl(proxy));
                }
                "zwlr_data_control_manager_v1" if state.manager.is_none() => {
                    let proxy = registry.bind::<ZwlrDataControlManagerV1, _, _>(
                        name,
                        version.min(3),
                        qh,
                        (),
                    );
                    state.manager = Some(DataControlManager::WlrDataControl(proxy));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipboardState {
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

// ---------------------------------------------------------------------------
// Dispatch — ext_data_control_v1 (thin wrappers → shared ClipboardState methods)
// ---------------------------------------------------------------------------

impl Dispatch<ExtDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => state.on_data_offer(id.id()),
            Event::Selection { id } => state.on_selection(
                SelectionTarget::Clipboard,
                id.map(DataControlOffer::ExtDataControl),
            ),
            Event::PrimarySelection { id } => state.on_selection(
                SelectionTarget::Primary,
                id.map(DataControlOffer::ExtDataControl),
            ),
            Event::Finished => state.on_device_finished(),
            _ => {}
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        assert_eq!(opcode, 0, "unexpected child-creating opcode");
        qh.make_data::<ExtDataControlOfferV1, ()>(())
    }
}

impl Dispatch<ExtDataControlOfferV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.on_offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, SourceRole> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        role: &SourceRole,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.on_source_send(role, mime_type, fd)
            }
            ext_data_control_source_v1::Event::Cancelled => state.on_source_cancelled(role),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch — zwlr_data_control_v1 (thin wrappers → same ClipboardState methods)
// ---------------------------------------------------------------------------

impl Dispatch<ZwlrDataControlManagerV1, ()> for ClipboardState {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: zwlr_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => state.on_data_offer(id.id()),
            Event::Selection { id } => state.on_selection(
                SelectionTarget::Clipboard,
                id.map(DataControlOffer::WlrDataControl),
            ),
            Event::PrimarySelection { id } => state.on_selection(
                SelectionTarget::Primary,
                id.map(DataControlOffer::WlrDataControl),
            ),
            Event::Finished => state.on_device_finished(),
            _ => {}
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        assert_eq!(opcode, 0, "unexpected child-creating opcode");
        qh.make_data::<ZwlrDataControlOfferV1, ()>(())
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for ClipboardState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.on_offer_mime(offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, SourceRole> for ClipboardState {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        role: &SourceRole,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                state.on_source_send(role, mime_type, fd)
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.on_source_cancelled(role),
            _ => {}
        }
    }
}
