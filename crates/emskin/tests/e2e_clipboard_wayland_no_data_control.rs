//! Clipboard E2E tests under a Wayland host that hides data-control.
//!
//! The emez instance is started with `--no-data-control`, so both
//! `ext_data_control_v1` and `zwlr_data_control_v1` are hidden from
//! every client — the KDE/GNOME situation. emskin falls back to the
//! `wl_data_device` backend in `src/clipboard_wl.rs`.
//!
//! Focus handoff inside emez (`handlers.rs::SelectionHandler::new_selection`)
//! returns focus to the primary toplevel (emskin's winit window) as
//! soon as a transient client (e.g. wl-copy) finishes `set_selection`.
//! That mirrors what a real compositor does via click-to-focus and is
//! what lets the end-to-end propagation path work here.
//!
//! Roles (no `ix` under xwayland-satellite — see sibling file's
//! header):
//! - `iw` — inside emskin's wayland data_device (wl-copy on emskin)
//! - `ow` — outside on host emez (wl-copy on emez's socket)

mod common;
use common::{recv_one, wl_copy, wl_paste, Compositor, NestedHost};

use std::time::Duration;

/// Selection propagation over `wl_data_device` needs a few extra
/// roundtrips vs. data-control (focus handoff, source rebind, offer
/// advertisement). 500ms is empirically enough without slowing the
/// green path measurably.
const SETTLE: Duration = Duration::from_millis(500);

struct Setup {
    compositor: Compositor,
    // Keep the IPC stream alive — emskin gates `set_host_selection` on
    // `ipc.is_connected()`, and dropping this would silently disable
    // host-side writes mid-test. Same rationale as the sibling suite.
    _ipc: std::os::unix::net::UnixStream,
}

fn setup() -> Setup {
    let mut compositor = Compositor::spawn_on(NestedHost::wayland_no_data_control());
    let mut stream = compositor.connect_ipc();
    let connected = recv_one(&mut stream);
    assert!(
        connected.contains(r#""type":"connected""#),
        "IPC handshake failed: {connected}"
    );
    let _ = compositor.cache_xwayland_display(&mut stream);
    compositor.wait_for_emskin_wayland_socket(Duration::from_secs(5));
    Setup {
        compositor,
        _ipc: stream,
    }
}

// =============================================================================
// Boot sanity — verifies backend selection and no busy-loop regression.
// =============================================================================

#[test]
fn boots_with_wl_data_device_backend() {
    let s = setup();
    let log = s.compositor.log_tail();
    assert!(
        log.contains("Clipboard sync initialized (wl_data_device_manager"),
        "expected wl_data_device backend to be selected, log tail:\n{log}"
    );
    assert!(
        !log.contains("Clipboard sync initialized (ext_data_control_v1)")
            && !log.contains("Clipboard sync initialized (zwlr_data_control_v1)"),
        "data-control backend should NOT activate under --no-data-control, log tail:\n{log}"
    );
}

// =============================================================================
// Simple propagation: external wayland copy → emskin reads it internally.
// =============================================================================

#[test]
fn ow_to_iw_via_wl_data_device() {
    let s = setup();
    let text = "ow-to-iw-no-dc";
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    wl_copy(s.compositor.xdg_runtime_dir(), host_wl, text);
    std::thread::sleep(SETTLE);
    let got = wl_paste(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    assert_eq!(got, text);
}
