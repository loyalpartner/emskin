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
//! Roles mirror `e2e_clipboard_wayland.rs`:
//! - `iw` — inside emskin's wayland data_device (wl-copy on emskin)
//! - `ix` — inside emskin's XWayland (xclip on emskin's DISPLAY)
//! - `ow` — outside on host emez (wl-copy on emez's socket)

mod common;
use common::{recv_one, wl_copy, wl_paste, xclip_copy, xclip_paste, Compositor, NestedHost};

use std::time::Duration;

/// Selection propagation over `wl_data_device` needs a few extra
/// roundtrips vs. data-control (focus handoff, source rebind, offer
/// advertisement). 500ms is empirically enough without slowing the
/// green path measurably.
const SETTLE: Duration = Duration::from_millis(500);

struct Setup {
    compositor: Compositor,
    emskin_xwayland_display: String,
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
    let d = compositor.cache_xwayland_display(&mut stream);
    compositor.wait_for_emskin_wayland_socket(Duration::from_secs(5));
    Setup {
        compositor,
        emskin_xwayland_display: format!(":{d}"),
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

#[test]
fn ow_to_ix_via_wl_data_device() {
    let s = setup();
    let text = "ow-to-ix-no-dc";
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    wl_copy(s.compositor.xdg_runtime_dir(), host_wl, text);
    std::thread::sleep(SETTLE);
    let got = xclip_paste(&s.emskin_xwayland_display);
    assert_eq!(got, text);
}

// =============================================================================
// Regression: the exact scenario the user reported.
// =============================================================================

/// Inside XWayland copies first and keeps the `xclip -i` daemon alive
/// holding the X CLIPBOARD. Then an outside-wayland client takes a new
/// host selection. The inside paste must see the **fresh host** content,
/// not the stale inside-XWayland content.
///
/// Under the `wl_data_device` backend this exercises:
/// 1. Host → `wl_data_device.selection(new_offer)` arrives on emskin
///    after emez's focus handoff.
/// 2. emskin's `clipboard_dispatch` calls `inject_host_selection`,
///    which routes through `X11Wm::new_selection` to take over the
///    inside X CLIPBOARD from the stale xclip daemon.
/// 3. `xclip -o` asks the new owner (emskin's XWM) which proxies the
///    request back through emskin → host → the wl-copy daemon.
#[test]
fn ix_then_ow_paste_ix_sees_ow_no_dc() {
    let s = setup();
    let ix_text = "stale-ix-no-dc";
    let ow_text = "fresh-ow-no-dc";

    // Step 1: inside XWayland copies first. xclip -i daemon stays alive.
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, ix_text);
    std::thread::sleep(SETTLE);
    let before = xclip_paste(&s.emskin_xwayland_display);
    assert_eq!(before, ix_text, "setup precondition failed");

    // Step 2: outside wayland takes a new selection on the host.
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    wl_copy(s.compositor.xdg_runtime_dir(), host_wl, ow_text);
    std::thread::sleep(SETTLE);

    // Step 3: inside XWayland paste. Must see fresh host content.
    let got = xclip_paste(&s.emskin_xwayland_display);
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(
        got, ow_text,
        "inside XWayland paste returned stale {ix_text:?} instead of fresh {ow_text:?} \
         (wl_data_device fallback did not propagate host selection)"
    );
}
