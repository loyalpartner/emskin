//! Clipboard E2E tests with a **Wayland host** (emez — our smithay-based
//! headless compositor, `crates/emez/`).
//!
//! emez advertises both `zwlr_data_control_v1` and `ext_data_control_v1`,
//! so emskin's `ClipboardProxy` takes its primary wayland data_control
//! path (and stays off the X11 fallback in `clipboard_x11.rs`).
//!
//! emez also embeds its own XWayland (see `crates/emez/src/xwayland.rs`),
//! exposing a host-side X DISPLAY. This lets us exercise the
//! wayland-host ↔ outside-X combinations in the same host environment
//! (the `ox_*` / `*_ox` tests below) alongside the pure-wayland ones.
//!
//! Roles:
//! - `iw` — inside emskin's wayland data_device (wl-copy on emskin)
//! - `ix` — inside emskin's XWayland (xclip on emskin's DISPLAY)
//! - `ow` — outside on host emez (wl-copy on emez's socket)
//! - `ox` — outside on host emez's embedded XWayland (xclip on host DISPLAY)

mod common;
use common::{
    recv_one, wl_copy, wl_paste, wl_paste_primary, xclip_copy, xclip_copy_primary, xclip_paste,
    Compositor, NestedHost,
};

use std::time::Duration;

/// Allow the selection/daemon dance to settle. wl-copy forks a daemon
/// and needs to send `wl_data_source.offer` before readers can see
/// anything; X ↔ wayland propagation via data_control adds a few
/// round-trips too. 300ms is empirically ample.
const DAEMON_SETTLE: Duration = Duration::from_millis(300);

struct Setup {
    compositor: Compositor,
    emskin_xwayland_display: String,
    // Keep the IPC stream alive for the lifetime of the test.
    // emskin gates `set_host_selection` on `ipc.is_connected()` to avoid
    // clobbering host clipboard before a real Emacs connects; dropping
    // this stream would cause that gate to close mid-test and silently
    // skip the host-side writes our assertions depend on.
    _ipc: std::os::unix::net::UnixStream,
}

fn setup() -> Setup {
    let mut compositor = Compositor::spawn_on(NestedHost::wayland());
    let mut stream = compositor.connect_ipc();
    let connected = recv_one(&mut stream);
    assert!(
        connected.contains(r#""type":"connected""#),
        "handshake failed: {connected}"
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
// iw_* : emskin's wayland as source
// =============================================================================

#[test]
fn iw_to_iw() {
    let s = setup();
    let text = "iw-to-iw";
    wl_copy(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
        text,
    );
    std::thread::sleep(DAEMON_SETTLE);
    let got = wl_paste(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    assert_eq!(got, text);
}

#[test]
fn iw_to_ix() {
    let s = setup();
    let text = "iw-to-ix";
    wl_copy(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
        text,
    );
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(&s.emskin_xwayland_display);
    assert_eq!(got, text);
}

#[test]
fn iw_to_ow() {
    let s = setup();
    let text = "iw-to-ow";
    wl_copy(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
        text,
    );
    std::thread::sleep(DAEMON_SETTLE);
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    let got = wl_paste(s.compositor.xdg_runtime_dir(), host_wl);
    assert_eq!(got, text);
}

// =============================================================================
// ix_* : emskin's XWayland as source
// =============================================================================

#[test]
fn ix_to_iw() {
    let s = setup();
    let text = "ix-to-iw";
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = wl_paste(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

#[test]
fn ix_to_ix() {
    let s = setup();
    let text = "ix-to-ix";
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(&s.emskin_xwayland_display);
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

#[test]
fn ix_to_ow() {
    let s = setup();
    let text = "ix-to-ow";
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, text);
    std::thread::sleep(DAEMON_SETTLE);
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    let got = wl_paste(s.compositor.xdg_runtime_dir(), host_wl);
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

// =============================================================================
// ow_* : host (emez) wayland as source
// =============================================================================

#[test]
fn ow_to_iw() {
    let s = setup();
    let text = "ow-to-iw";
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    wl_copy(s.compositor.xdg_runtime_dir(), host_wl, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = wl_paste(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    assert_eq!(got, text);
}

#[test]
fn ow_to_ix() {
    let s = setup();
    let text = "ow-to-ix";
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    wl_copy(s.compositor.xdg_runtime_dir(), host_wl, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(&s.emskin_xwayland_display);
    assert_eq!(got, text);
}

// =============================================================================
// ox_* / *_ox : outside X client on host emez's embedded XWayland
// =============================================================================
//
// These exercise the ClipboardProxy wayland data-control path in both
// directions under a Wayland host, complementing the X11-host variants
// in `e2e_clipboard_x11.rs` (which hit the X11 fallback code instead).

#[test]
fn iw_to_ox() {
    let s = setup();
    let text = "iw-to-ox";
    wl_copy(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
        text,
    );
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(s.compositor.host_display());
    assert_eq!(got, text);
}

#[test]
fn ix_to_ox() {
    let s = setup();
    let text = "ix-to-ox";
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(s.compositor.host_display());
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

#[test]
fn ox_to_iw() {
    let s = setup();
    let text = "ox-to-iw";
    let mut xclip = xclip_copy(s.compositor.host_display(), text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = wl_paste(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

#[test]
fn ox_to_ix() {
    let s = setup();
    let text = "ox-to-ix";
    let mut xclip = xclip_copy(s.compositor.host_display(), text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(&s.emskin_xwayland_display);
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

// =============================================================================
// Stale-selection regression
// =============================================================================

/// Regression: inside XWayland owns the selection first, then an outside
/// wayland client takes a new selection on the host. A subsequent paste
/// inside XWayland must return the **new host** content, not the stale
/// inside-XWayland content. The bug this guards against: when the
/// xclip-`-i` daemon is still holding the inside X CLIPBOARD, emskin's
/// `inject_host_selection` must call `X11Wm::new_selection` to take
/// ownership away from that daemon so subsequent X paste requests are
/// routed to the host selection source instead of the stale daemon.
#[test]
fn ix_then_ow_paste_ix_sees_ow() {
    let s = setup();
    let ix_text = "stale-ix";
    let ow_text = "fresh-ow";

    // Step 1: inside XWayland copies first. xclip -i forks a daemon that
    // stays alive holding the X CLIPBOARD — do not kill it before step 3,
    // otherwise the bug scenario disappears.
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, ix_text);
    std::thread::sleep(DAEMON_SETTLE);

    // Sanity: the inside X selection is indeed ix_text before step 2.
    let before = xclip_paste(&s.emskin_xwayland_display);
    assert_eq!(before, ix_text, "setup precondition failed");

    // Step 2: outside Wayland client takes a new selection on the host.
    // emskin's ClipboardProxy should observe this via data-control and
    // replace the inside selection (both wl_data_device and X CLIPBOARD).
    let host_wl = s
        .compositor
        .host_wayland()
        .expect("wayland host has wl socket");
    wl_copy(s.compositor.xdg_runtime_dir(), host_wl, ow_text);
    std::thread::sleep(DAEMON_SETTLE);

    // Step 3: inside XWayland paste. Must see the fresh host content,
    // not the stale ix xclip daemon's content.
    let got = xclip_paste(&s.emskin_xwayland_display);
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(
        got, ow_text,
        "inside XWayland paste returned stale {ix_text:?} instead of fresh {ow_text:?}"
    );
}

// =============================================================================
// PRIMARY (middle-click) sanity
// =============================================================================

/// Exercises the `set_primary_selection` /
/// `request_primary_client_selection` path once. If the dedicated PRIMARY
/// smithay helpers have a bug, this test will catch it while the
/// CLIPBOARD matrix stays green.
#[test]
fn primary_ix_to_iw() {
    let s = setup();
    let text = "primary-ix-to-iw";
    let mut xclip = xclip_copy_primary(&s.emskin_xwayland_display, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = wl_paste_primary(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}
