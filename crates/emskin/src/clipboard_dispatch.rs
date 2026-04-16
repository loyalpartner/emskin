//! Clipboard event handling — processes events from the host clipboard backend
//! and bridges them to internal Wayland/X11 clients.

use crate::clipboard::ClipboardEvent;
use crate::state::{EmskinState, SelectionOrigin};

pub fn handle_clipboard_event(state: &mut EmskinState, event: ClipboardEvent) {
    match event {
        ClipboardEvent::HostSelectionChanged { target, mime_types } => {
            inject_host_selection(state, target, mime_types);
        }
        ClipboardEvent::HostSendRequest {
            id,
            target,
            mime_type,
            write_fd,
            read_fd,
        } => {
            forward_client_selection(state, target, mime_type, write_fd);
            // Flush immediately so the write_fd reaches the Wayland client
            // before our OwnedFd copy is dropped (closing the write end).
            let _ = state.display_handle.flush_clients();
            if let Some(read_fd) = read_fd {
                if !register_outgoing_pipe(state, id, read_fd) {
                    // Calloop registration failed — clean up and notify X11 requestor.
                    if let Some(ref mut cb) = state.selection.clipboard {
                        cb.complete_outgoing(id, Vec::new());
                    }
                }
            }
        }
        ClipboardEvent::SourceCancelled { target } => {
            tracing::debug!("Host source cancelled ({target:?})");
            match target {
                smithay::wayland::selection::SelectionTarget::Clipboard => {
                    state.selection.clipboard_origin = SelectionOrigin::default();
                }
                smithay::wayland::selection::SelectionTarget::Primary => {
                    state.selection.primary_origin = SelectionOrigin::default();
                }
            }
        }
    }
}

fn inject_host_selection(
    state: &mut EmskinState,
    target: smithay::wayland::selection::SelectionTarget,
    mime_types: Vec<String>,
) {
    use smithay::wayland::selection::data_device::{
        clear_data_device_selection, set_data_device_selection,
    };
    use smithay::wayland::selection::primary_selection::{
        clear_primary_selection, set_primary_selection,
    };
    use smithay::wayland::selection::SelectionTarget;

    // Cache host mime types for replay when XWM becomes ready.
    match target {
        SelectionTarget::Clipboard => state.selection.host_clipboard_mimes = mime_types.clone(),
        SelectionTarget::Primary => state.selection.host_primary_mimes = mime_types.clone(),
    }

    if mime_types.is_empty() {
        tracing::debug!("Host {target:?} cleared");
        match target {
            SelectionTarget::Clipboard => {
                clear_data_device_selection(&state.display_handle, &state.seat)
            }
            SelectionTarget::Primary => clear_primary_selection(&state.display_handle, &state.seat),
        }
        if let Some(ref mut xwm) = state.xwm {
            if let Err(e) = xwm.new_selection(target, None) {
                tracing::warn!("X11 clear {target:?} selection failed: {e}");
            }
        }
    } else {
        tracing::debug!("Host {target:?} changed ({} types)", mime_types.len());
        if let Some(ref mut xwm) = state.xwm {
            if let Err(e) = xwm.new_selection(target, Some(mime_types.clone())) {
                tracing::warn!("X11 set {target:?} selection failed: {e}");
            }
        }
        match target {
            SelectionTarget::Clipboard => {
                set_data_device_selection(&state.display_handle, &state.seat, mime_types, ())
            }
            SelectionTarget::Primary => {
                set_primary_selection(&state.display_handle, &state.seat, mime_types, ())
            }
        }
    }
}

fn forward_client_selection(
    state: &mut EmskinState,
    target: smithay::wayland::selection::SelectionTarget,
    mime_type: String,
    fd: std::os::fd::OwnedFd,
) {
    use smithay::wayland::selection::data_device::request_data_device_client_selection;
    use smithay::wayland::selection::primary_selection::request_primary_client_selection;
    use smithay::wayland::selection::SelectionTarget;

    let origin = match target {
        SelectionTarget::Clipboard => state.selection.clipboard_origin,
        SelectionTarget::Primary => state.selection.primary_origin,
    };

    match origin {
        SelectionOrigin::Wayland => {
            let result = match target {
                SelectionTarget::Clipboard => {
                    request_data_device_client_selection(&state.seat, mime_type, fd)
                        .map_err(|e| format!("{e:?}"))
                }
                SelectionTarget::Primary => {
                    request_primary_client_selection(&state.seat, mime_type, fd)
                        .map_err(|e| format!("{e:?}"))
                }
            };
            if let Err(e) = result {
                tracing::warn!("Failed to forward {target:?} selection to host: {e}");
            }
        }
        SelectionOrigin::X11 => {
            if let Some(ref mut xwm) = state.xwm {
                if let Err(e) = xwm.send_selection(target, mime_type, fd) {
                    tracing::warn!("Failed to forward X11 {target:?} selection to host: {e}");
                }
            } else {
                tracing::warn!("X11 {target:?} selection requested but XWM unavailable");
            }
        }
    }
}

/// Register a pipe read_fd with calloop for event-driven reading.
/// Returns `false` if registration fails (caller should clean up).
fn register_outgoing_pipe(state: &mut EmskinState, id: u64, read_fd: std::os::fd::OwnedFd) -> bool {
    use smithay::reexports::calloop::{generic::Generic, Interest, Mode, PostAction};
    use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};

    // SAFETY: into_raw_fd() relinquishes ownership; File takes it over.
    let file = unsafe { std::fs::File::from_raw_fd(read_fd.into_raw_fd()) };
    let mut buf_data: Vec<u8> = Vec::new();

    if let Err(e) = state.loop_handle.insert_source(
        Generic::new(file, Interest::READ, Mode::Level),
        move |_, file, state| {
            let mut buf = [0u8; 65536];
            loop {
                // SAFETY: buf is valid for buf.len() bytes; fd is open and non-blocking.
                let n = unsafe { libc::read(file.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    buf_data.extend_from_slice(&buf[..n as usize]);
                } else if n == 0 {
                    let data = std::mem::take(&mut buf_data);
                    if let Some(ref mut clipboard) = state.selection.clipboard {
                        clipboard.complete_outgoing(id, data);
                    }
                    return Ok(PostAction::Remove);
                } else {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(PostAction::Continue);
                    }
                    tracing::warn!("outgoing pipe read error: {err}");
                    return Ok(PostAction::Remove);
                }
            }
        },
    ) {
        tracing::warn!("Failed to register outgoing pipe: {e}");
        return false;
    }
    true
}
