//! X11 host clipboard synchronization.
//!
//! When emskin runs on an X11 host (no Wayland compositor), this module bridges
//! the host X11 selection protocol with internal Wayland clients via the same
//! `ClipboardEvent` interface used by the Wayland clipboard proxy.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use smithay::reexports::x11rb;
use x11rb::connection::Connection;
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesExt, SelectionEventMask};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use smithay::wayland::selection::SelectionTarget;

use crate::clipboard::ClipboardEvent;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const INCR_CHUNK_SIZE: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Atom table
// ---------------------------------------------------------------------------

x11rb::atom_manager! {
    ClipboardAtoms: ClipboardAtomsCookie {
        CLIPBOARD,
        PRIMARY,
        TARGETS,
        TIMESTAMP,
        INCR,
        UTF8_STRING,
        TEXT,
        _EMSKIN_CLIP_SEL,
        _EMSKIN_PRIM_SEL,
        _EMSKIN_CLIP_INIT,
        _EMSKIN_PRIM_INIT,
    }
}

// ---------------------------------------------------------------------------
// MIME <-> Atom helpers
// ---------------------------------------------------------------------------

fn mime_from_atom(atom: Atom, conn: &RustConnection, atoms: &ClipboardAtoms) -> Option<String> {
    match atom {
        a if a == atoms.TEXT => Some("text/plain".into()),
        a if a == atoms.UTF8_STRING => Some("text/plain;charset=utf-8".into()),
        a => conn
            .get_atom_name(a)
            .ok()?
            .reply()
            .ok()
            .and_then(|r| String::from_utf8(r.name).ok()),
    }
}

fn atom_from_mime(mime: &str, conn: &RustConnection, atoms: &ClipboardAtoms) -> Option<Atom> {
    match mime {
        "text/plain" => Some(atoms.TEXT),
        "text/plain;charset=utf-8" => Some(atoms.UTF8_STRING),
        m => conn
            .intern_atom(false, m.as_bytes())
            .ok()?
            .reply()
            .ok()
            .map(|r| r.atom),
    }
}

fn selection_atom(target: SelectionTarget, atoms: &ClipboardAtoms) -> Atom {
    match target {
        SelectionTarget::Clipboard => atoms.CLIPBOARD,
        SelectionTarget::Primary => atoms.PRIMARY,
    }
}

fn property_atom(target: SelectionTarget, atoms: &ClipboardAtoms) -> Atom {
    match target {
        SelectionTarget::Clipboard => atoms._EMSKIN_CLIP_SEL,
        SelectionTarget::Primary => atoms._EMSKIN_PRIM_SEL,
    }
}

fn selection_target_from_atom(atom: Atom, atoms: &ClipboardAtoms) -> Option<SelectionTarget> {
    if atom == atoms.CLIPBOARD {
        Some(SelectionTarget::Clipboard)
    } else if atom == atoms.PRIMARY {
        Some(SelectionTarget::Primary)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Transfer types
// ---------------------------------------------------------------------------

/// Pending ConvertSelection response — either TARGETS query or data fetch.
enum PendingConvert {
    /// Waiting for TARGETS list after XFixes notification.
    Targets { selection_target: SelectionTarget },
    /// Waiting for actual data to forward to an internal client fd.
    Data {
        fd: OwnedFd,
        incr: bool,
        data: Vec<u8>,
    },
}

/// Active INCR outgoing transfer (only exists after pipe data is fully read).
struct IncrOutgoing {
    request: SelectionRequestEvent,
    data: Vec<u8>,
    flush_on_delete: bool,
}

// ---------------------------------------------------------------------------
// X11ClipboardProxy
// ---------------------------------------------------------------------------

pub struct X11ClipboardProxy {
    conn: RustConnection,
    atoms: ClipboardAtoms,
    window: Window,

    // Our advertised MIME types (when we own the selection).
    our_clipboard_mimes: Vec<String>,
    our_primary_mimes: Vec<String>,

    // Timestamps of the last selection ownership we obtained.
    our_clipboard_ts: Timestamp,
    our_primary_ts: Timestamp,

    // Pending ConvertSelection responses keyed by (selection, property).
    pending_converts: HashMap<(Atom, Atom), PendingConvert>,

    // Outgoing transfer state.
    next_outgoing_id: u64,
    /// X11 requests waiting for pipe data (calloop reads the pipe).
    outgoing_requests: HashMap<u64, SelectionRequestEvent>,
    /// Active INCR transfers (pipe data already received, sending chunks).
    incr_outgoing: Vec<IncrOutgoing>,

    events: Vec<ClipboardEvent>,
    suppress_clipboard: u32,
    suppress_primary: u32,
}

impl X11ClipboardProxy {
    /// Connect to the X11 display and set up selection monitoring.
    /// Returns `None` if connection or setup fails.
    pub fn new() -> Option<Self> {
        let (conn, screen_num) = match RustConnection::connect(None) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("Cannot connect to X11 for clipboard: {e}");
                return None;
            }
        };

        let atoms = match ClipboardAtoms::new(&conn) {
            Ok(cookie) => match cookie.reply() {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("Failed to intern clipboard atoms: {e}");
                    return None;
                }
            },
            Err(e) => {
                tracing::warn!("Failed to send atom requests: {e}");
                return None;
            }
        };

        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;
        let visual = screen.root_visual;

        // Create a hidden proxy window for selection ownership and events.
        let window = match conn.generate_id() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Failed to generate X11 window id: {e}");
                return None;
            }
        };
        if let Err(e) = conn.create_window(
            depth,
            window,
            root,
            0,
            0,
            10,
            10,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        ) {
            tracing::warn!("Failed to create clipboard proxy window: {e}");
            return None;
        }

        // Query XFixes extension.
        // We only need XFixes v1 for selection monitoring.
        if let Err(e) = conn.xfixes_query_version(1, 0) {
            tracing::warn!("XFixes query failed: {e}");
            return None;
        }

        // Monitor CLIPBOARD and PRIMARY selection owner changes.
        let mask = SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE;

        for sel in [atoms.CLIPBOARD, atoms.PRIMARY] {
            if let Err(e) = conn.xfixes_select_selection_input(window, sel, mask) {
                tracing::warn!("xfixes_select_selection_input failed: {e}");
                return None;
            }
        }

        if let Err(e) = conn.flush() {
            tracing::warn!("X11 flush failed: {e}");
            return None;
        }

        // Query initial clipboard state using separate property atoms
        // so responses don't collide with later XFixes-triggered queries.
        let mut pending_converts = HashMap::new();
        for (sel, target, init_prop) in [
            (
                atoms.CLIPBOARD,
                SelectionTarget::Clipboard,
                atoms._EMSKIN_CLIP_INIT,
            ),
            (
                atoms.PRIMARY,
                SelectionTarget::Primary,
                atoms._EMSKIN_PRIM_INIT,
            ),
        ] {
            let owner = conn
                .get_selection_owner(sel)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.owner)
                .unwrap_or(0);
            if owner != 0 && owner != window {
                if let Err(e) = conn.convert_selection(
                    window,
                    sel,
                    atoms.TARGETS,
                    init_prop,
                    x11rb::CURRENT_TIME,
                ) {
                    tracing::warn!("Initial ConvertSelection(TARGETS) failed: {e}");
                    continue;
                }
                pending_converts.insert(
                    (sel, init_prop),
                    PendingConvert::Targets {
                        selection_target: target,
                    },
                );
            }
        }
        let _ = conn.flush();

        tracing::info!("X11 clipboard sync initialized");
        Some(Self {
            conn,
            atoms,
            window,
            our_clipboard_mimes: Vec::new(),
            our_primary_mimes: Vec::new(),
            our_clipboard_ts: x11rb::CURRENT_TIME,
            our_primary_ts: x11rb::CURRENT_TIME,
            pending_converts,
            next_outgoing_id: 0,
            outgoing_requests: HashMap::new(),
            incr_outgoing: Vec::new(),
            events: Vec::new(),
            suppress_clipboard: 0,
            suppress_primary: 0,
        })
    }

    /// Borrow the X11 connection fd for calloop registration.
    pub fn connection_fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    /// Read pending events from the X11 connection (non-blocking).
    pub fn dispatch(&mut self) {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => self.handle_event(event),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("X11 poll_for_event error: {e}");
                    break;
                }
            }
        }
    }

    /// Drain queued clipboard events for the compositor.
    pub fn take_events(&mut self) -> Vec<ClipboardEvent> {
        std::mem::take(&mut self.events)
    }

    /// Forward host selection data to an internal client's fd.
    ///
    /// Initiates a ConvertSelection request. The data is forwarded
    /// asynchronously via subsequent dispatch() calls.
    pub fn receive_from_host(&mut self, target: SelectionTarget, mime_type: &str, fd: OwnedFd) {
        tracing::trace!("receive_from_host: {target:?} mime={mime_type}");
        let sel = selection_atom(target, &self.atoms);
        let prop = property_atom(target, &self.atoms);

        let Some(target_atom) = atom_from_mime(mime_type, &self.conn, &self.atoms) else {
            tracing::warn!("receive_from_host: cannot map MIME '{mime_type}' to atom");
            return;
        };

        if let Err(e) =
            self.conn
                .convert_selection(self.window, sel, target_atom, prop, x11rb::CURRENT_TIME)
        {
            tracing::warn!("ConvertSelection failed: {e}");
            return;
        }
        let _ = self.conn.flush();

        self.pending_converts.insert(
            (sel, prop),
            PendingConvert::Data {
                fd,
                incr: false,
                data: Vec::new(),
            },
        );
    }

    /// Advertise an internal client's selection to the host.
    pub fn set_host_selection(&mut self, target: SelectionTarget, mime_types: &[String]) {
        let sel = selection_atom(target, &self.atoms);

        if let Err(e) = self
            .conn
            .set_selection_owner(self.window, sel, x11rb::CURRENT_TIME)
        {
            tracing::warn!("set_selection_owner failed: {e}");
            return;
        }
        let _ = self.conn.flush();

        match target {
            SelectionTarget::Clipboard => {
                self.our_clipboard_mimes = mime_types.to_vec();
                self.suppress_clipboard += 1;
            }
            SelectionTarget::Primary => {
                self.our_primary_mimes = mime_types.to_vec();
                self.suppress_primary += 1;
            }
        }
    }

    /// Clear our selection on the host.
    pub fn clear_host_selection(&mut self, target: SelectionTarget) {
        let sel = selection_atom(target, &self.atoms);

        if let Err(e) = self
            .conn
            .set_selection_owner(0u32, sel, x11rb::CURRENT_TIME)
        {
            tracing::warn!("clear_selection_owner failed: {e}");
            return;
        }
        let _ = self.conn.flush();

        match target {
            SelectionTarget::Clipboard => {
                self.our_clipboard_mimes.clear();
                self.suppress_clipboard += 1;
            }
            SelectionTarget::Primary => {
                self.our_primary_mimes.clear();
                self.suppress_primary += 1;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Event handling
    // -----------------------------------------------------------------------

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::XfixesSelectionNotify(n) => self.on_xfixes_notify(n),
            Event::SelectionNotify(n) => self.on_selection_notify(n),
            Event::SelectionRequest(n) => self.on_selection_request(n),
            Event::PropertyNotify(n) => self.on_property_notify(n),
            _ => {}
        }
    }

    /// Host selection owner changed (XFixes notification).
    fn on_xfixes_notify(&mut self, n: xfixes::SelectionNotifyEvent) {
        tracing::trace!("XFixes: selection owner changed (owner={})", n.owner);
        let Some(target) = selection_target_from_atom(n.selection, &self.atoms) else {
            return;
        };

        // If we are the new owner, this is our own set_selection_owner echo.
        if n.owner == self.window {
            // Store timestamp for later use.
            match target {
                SelectionTarget::Clipboard => self.our_clipboard_ts = n.selection_timestamp,
                SelectionTarget::Primary => self.our_primary_ts = n.selection_timestamp,
            }
            return;
        }

        // Check suppress counter.
        let suppress = match target {
            SelectionTarget::Clipboard => &mut self.suppress_clipboard,
            SelectionTarget::Primary => &mut self.suppress_primary,
        };
        if *suppress > 0 {
            *suppress -= 1;
            return;
        }

        // If owner is NONE, selection was cleared.
        if n.owner == 0 {
            self.events.push(ClipboardEvent::HostSelectionChanged {
                target,
                mime_types: Vec::new(),
            });
            return;
        }

        // Query TARGETS from the new owner.
        let sel = n.selection;
        let prop = property_atom(target, &self.atoms);

        if let Err(e) = self.conn.convert_selection(
            self.window,
            sel,
            self.atoms.TARGETS,
            prop,
            n.selection_timestamp,
        ) {
            tracing::warn!("ConvertSelection(TARGETS) failed: {e}");
            return;
        }
        let _ = self.conn.flush();

        self.pending_converts.insert(
            (sel, prop),
            PendingConvert::Targets {
                selection_target: target,
            },
        );
    }

    /// Response to our ConvertSelection request.
    fn on_selection_notify(&mut self, n: SelectionNotifyEvent) {
        tracing::trace!(
            "SelectionNotify: selection={} property={} target={}",
            n.selection,
            n.property,
            n.target
        );
        let key = (n.selection, n.property);

        // property == NONE means conversion failed.
        if n.property == x11rb::NONE {
            tracing::trace!("SelectionNotify: conversion failed (property=NONE)");
            if let Some(PendingConvert::Targets { selection_target }) =
                self.pending_converts.remove(&key)
            {
                self.events.push(ClipboardEvent::HostSelectionChanged {
                    target: selection_target,
                    mime_types: Vec::new(),
                });
            }
            return;
        }

        let Some(pending) = self.pending_converts.remove(&key) else {
            return;
        };

        match pending {
            PendingConvert::Targets { selection_target } => {
                self.handle_targets_reply(selection_target, n.property);
            }
            PendingConvert::Data { fd, incr, data } => {
                self.handle_data_reply(n.selection, n.property, fd, incr, data);
            }
        }
    }

    /// Parse TARGETS property and emit HostSelectionChanged.
    fn handle_targets_reply(&mut self, target: SelectionTarget, property: Atom) {
        let reply = match self.conn.get_property(
            true, // delete after reading
            self.window,
            property,
            AtomEnum::ANY,
            0,
            1024,
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("get_property(TARGETS) reply failed: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("get_property(TARGETS) failed: {e}");
                return;
            }
        };

        tracing::trace!(
            "TARGETS reply: type={} format={} len={}",
            reply.type_,
            reply.format,
            reply.value_len
        );

        let mime_types: Vec<String> = reply
            .value32()
            .map(|atoms| {
                atoms
                    .filter_map(|a| mime_from_atom(a, &self.conn, &self.atoms))
                    .collect()
            })
            .unwrap_or_default();

        tracing::debug!("Host {target:?} changed ({} types)", mime_types.len());
        self.events
            .push(ClipboardEvent::HostSelectionChanged { target, mime_types });
    }

    /// Read data property and write to the internal client fd.
    fn handle_data_reply(
        &mut self,
        selection: Atom,
        property: Atom,
        fd: OwnedFd,
        _was_incr: bool,
        mut data: Vec<u8>,
    ) {
        let reply = match self.conn.get_property(
            true,
            self.window,
            property,
            AtomEnum::ANY,
            0,
            0x1FFFFFFF, // ~500MB max
        ) {
            Ok(cookie) => match cookie.reply() {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("get_property(data) reply failed: {e}");
                    return;
                }
            },
            Err(e) => {
                tracing::warn!("get_property(data) failed: {e}");
                return;
            }
        };

        // Check for INCR transfer start.
        if reply.type_ == self.atoms.INCR {
            tracing::debug!("INCR transfer started for selection");
            // Re-insert as INCR pending — data will arrive via PropertyNotify.
            self.pending_converts.insert(
                (selection, property),
                PendingConvert::Data {
                    fd,
                    incr: true,
                    data: Vec::new(),
                },
            );
            return;
        }

        // Non-INCR: data is in the reply.
        data.extend_from_slice(&reply.value);
        tracing::trace!("receive_from_host: got {} bytes, writing to fd", data.len());
        Self::write_all_to_fd(&fd, &data);
    }

    /// Handle PropertyNotify for INCR transfers (both incoming and outgoing).
    fn on_property_notify(&mut self, n: PropertyNotifyEvent) {
        tracing::trace!(
            "PropertyNotify: window={} atom={} state={:?}",
            n.window,
            n.atom,
            n.state
        );
        if n.window == self.window && n.state == Property::NEW_VALUE {
            // Incoming INCR chunk: property was set by the selection owner.
            self.handle_incr_chunk(n.atom);
        }

        if n.state == Property::DELETE {
            // Outgoing INCR: requestor deleted the property, send next chunk.
            self.handle_outgoing_property_delete(n.window, n.atom);
        }
    }

    /// Read an INCR chunk from our window's property.
    fn handle_incr_chunk(&mut self, property: Atom) {
        // Only process properties that belong to active INCR data transfers.
        // Skip Targets entries — those are handled by on_selection_notify.
        let key = self
            .pending_converts
            .iter()
            .find(|(&(_, p), v)| {
                p == property && matches!(v, PendingConvert::Data { incr: true, .. })
            })
            .map(|(k, _)| *k);

        let Some(key) = key else {
            return;
        };

        let reply =
            match self
                .conn
                .get_property(true, self.window, property, AtomEnum::ANY, 0, 0x1FFFFFFF)
            {
                Ok(cookie) => match cookie.reply() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("INCR get_property failed: {e}");
                        self.pending_converts.remove(&key);
                        return;
                    }
                },
                Err(e) => {
                    tracing::warn!("INCR get_property send failed: {e}");
                    self.pending_converts.remove(&key);
                    return;
                }
            };

        if reply.value.is_empty() {
            // Zero-length chunk = INCR transfer complete.
            if let Some(PendingConvert::Data { fd, data, .. }) = self.pending_converts.remove(&key)
            {
                tracing::debug!("INCR transfer complete ({} bytes)", data.len());
                Self::write_all_to_fd(&fd, &data);
            }
        } else if let Some(PendingConvert::Data { data, .. }) = self.pending_converts.get_mut(&key)
        {
            data.extend_from_slice(&reply.value);
        }
    }

    /// Handle SelectionRequest from a host application wanting our data.
    fn on_selection_request(&mut self, req: SelectionRequestEvent) {
        tracing::trace!(
            "SelectionRequest from window={} target_atom={}",
            req.requestor,
            req.target
        );
        let Some(target) = selection_target_from_atom(req.selection, &self.atoms) else {
            Self::send_selection_notify(&self.conn, &req, false);
            return;
        };

        let our_mimes = match target {
            SelectionTarget::Clipboard => &self.our_clipboard_mimes,
            SelectionTarget::Primary => &self.our_primary_mimes,
        };

        if our_mimes.is_empty() {
            Self::send_selection_notify(&self.conn, &req, false);
            return;
        }

        // TARGETS request.
        if req.target == self.atoms.TARGETS {
            let mut target_atoms: Vec<Atom> = vec![self.atoms.TARGETS, self.atoms.TIMESTAMP];
            for mime in our_mimes {
                if let Some(a) = atom_from_mime(mime, &self.conn, &self.atoms) {
                    target_atoms.push(a);
                }
            }
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                req.property,
                AtomEnum::ATOM,
                &target_atoms,
            );
            Self::send_selection_notify(&self.conn, &req, true);
            return;
        }

        // TIMESTAMP request.
        if req.target == self.atoms.TIMESTAMP {
            let ts = match target {
                SelectionTarget::Clipboard => self.our_clipboard_ts,
                SelectionTarget::Primary => self.our_primary_ts,
            };
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                req.property,
                AtomEnum::INTEGER,
                &[ts],
            );
            Self::send_selection_notify(&self.conn, &req, true);
            return;
        }

        // Data request: create a pipe and emit HostSendRequest.
        let Some(mime_type) = mime_from_atom(req.target, &self.conn, &self.atoms) else {
            tracing::debug!("SelectionRequest: unknown target atom {}", req.target);
            Self::send_selection_notify(&self.conn, &req, false);
            return;
        };

        let (read_fd, write_fd) = match Self::make_pipe() {
            Some(p) => p,
            None => {
                Self::send_selection_notify(&self.conn, &req, false);
                return;
            }
        };

        let id = self.next_outgoing_id;
        self.next_outgoing_id += 1;
        self.outgoing_requests.insert(id, req);

        self.events.push(ClipboardEvent::HostSendRequest {
            id,
            target,
            mime_type,
            write_fd,
            read_fd: Some(read_fd),
        });
    }

    /// Complete an outgoing transfer after calloop has read all pipe data.
    pub fn complete_outgoing(&mut self, id: u64, data: Vec<u8>) {
        let Some(req) = self.outgoing_requests.remove(&id) else {
            return;
        };

        tracing::debug!("Outgoing transfer complete: {} bytes", data.len());

        if data.len() > INCR_CHUNK_SIZE {
            // Start INCR transfer for large data.
            let size = data.len() as u32;
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                req.requestor,
                req.property,
                self.atoms.INCR,
                &[size],
            );
            Self::send_selection_notify(&self.conn, &req, true);
            let _ = self.conn.flush();

            self.incr_outgoing.push(IncrOutgoing {
                request: req,
                data,
                flush_on_delete: false,
            });
            return;
        }

        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            req.requestor,
            req.property,
            req.target,
            &data,
        );
        Self::send_selection_notify(&self.conn, &req, true);
        let _ = self.conn.flush();
    }

    /// Handle PropertyNotify(DELETE) for outgoing INCR transfers.
    fn handle_outgoing_property_delete(&mut self, window: Window, property: Atom) {
        let Some(idx) = self
            .incr_outgoing
            .iter()
            .position(|t| t.request.requestor == window && t.request.property == property)
        else {
            return;
        };

        let transfer = &mut self.incr_outgoing[idx];

        if transfer.flush_on_delete {
            self.incr_outgoing.swap_remove(idx);
            return;
        }

        if transfer.data.is_empty() {
            let _ = self.conn.change_property8(
                PropMode::REPLACE,
                window,
                property,
                transfer.request.target,
                &[],
            );
            let _ = self.conn.flush();
            transfer.flush_on_delete = true;
            return;
        }

        let chunk_end = transfer.data.len().min(INCR_CHUNK_SIZE);
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            window,
            property,
            transfer.request.target,
            &transfer.data[..chunk_end],
        );
        transfer.data.drain(..chunk_end);
        let _ = self.conn.flush();
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn send_selection_notify(conn: &RustConnection, req: &SelectionRequestEvent, success: bool) {
        let property = if success { req.property } else { x11rb::NONE };
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property,
        };
        let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, event);
        let _ = conn.flush();
    }

    fn write_all_to_fd(fd: &OwnedFd, data: &[u8]) {
        // Best-effort write; if the client closed the fd, just drop data.
        let raw = fd.as_raw_fd();
        let mut offset = 0;
        while offset < data.len() {
            let n =
                unsafe { libc::write(raw, data[offset..].as_ptr().cast(), data.len() - offset) };
            if n <= 0 {
                break;
            }
            offset += n as usize;
        }
    }

    fn make_pipe() -> Option<(OwnedFd, OwnedFd)> {
        let mut fds = [0i32; 2];
        let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if ret < 0 {
            tracing::warn!("pipe2 failed: {}", std::io::Error::last_os_error());
            return None;
        }
        unsafe { Some((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))) }
    }
}

impl crate::clipboard::ClipboardBackend for X11ClipboardProxy {
    fn dispatch(&mut self) {
        self.dispatch();
    }

    fn take_events(&mut self) -> Vec<ClipboardEvent> {
        self.take_events()
    }

    fn connection_fd(&self) -> BorrowedFd<'_> {
        self.connection_fd()
    }

    fn receive_from_host(
        &mut self,
        target: smithay::wayland::selection::SelectionTarget,
        mime_type: &str,
        fd: OwnedFd,
    ) {
        self.receive_from_host(target, mime_type, fd);
    }

    fn set_host_selection(
        &mut self,
        target: smithay::wayland::selection::SelectionTarget,
        mime_types: &[String],
    ) {
        self.set_host_selection(target, mime_types);
    }

    fn clear_host_selection(&mut self, target: smithay::wayland::selection::SelectionTarget) {
        self.clear_host_selection(target);
    }

    fn complete_outgoing(&mut self, id: u64, data: Vec<u8>) {
        self.complete_outgoing(id, data);
    }
}
