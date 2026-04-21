//! Clipboard E2E tests with an **X11 host** (bare Xvfb, no host wayland).
//!
//! Covers the cross-boundary combinations where the **host** is X11
//! (emskin's `ClipboardProxy` fallback connects to the host Xvfb
//! directly) and the emskin-internal client is a Wayland client.
//!
//! Under xwayland-satellite emskin has no "internal X" role: every X
//! client is translated into a Wayland client before it reaches us.
//! So the matrix here is intentionally narrow — it exercises only the
//! paths where emskin sees Wayland on one side and the host Xvfb on
//! the other.
//!
//! Roles:
//! - `iw` — inside emskin's wayland data_device
//! - `ox` — outside on host Xvfb

mod common;
use common::{
    recv_one, wl_copy, wl_paste, wl_paste_primary, xclip_copy, xclip_copy_primary, xclip_paste,
    Compositor, NestedHost,
};

use std::time::Duration;

const DAEMON_SETTLE: Duration = Duration::from_millis(300);

struct Setup {
    compositor: Compositor,
    _ipc: std::os::unix::net::UnixStream,
}

fn setup() -> Setup {
    let mut compositor = Compositor::spawn_on(NestedHost::x11());
    let mut stream = compositor.connect_ipc();
    let connected = recv_one(&mut stream);
    assert!(
        connected.contains(r#""type":"connected""#),
        "handshake failed: {connected}"
    );
    let _ = compositor.cache_xwayland_display(&mut stream);
    compositor.wait_for_emskin_wayland_socket(Duration::from_secs(5));
    Setup {
        compositor,
        _ipc: stream,
    }
}

// =============================================================================
// emskin-internal wayland → host X sink
// =============================================================================

#[test]
fn iw_to_ox() {
    let s = setup();
    let text = "x11host-iw-to-ox";
    wl_copy(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
        text,
    );
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(s.compositor.host_display());
    assert_eq!(got, text);
}

// =============================================================================
// host X source → emskin-internal wayland sink
// =============================================================================

#[test]
fn ox_to_iw() {
    let s = setup();
    let text = "x11host-ox-to-iw";
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

// =============================================================================
// PRIMARY (middle-click) sanity on X11 host
// =============================================================================

#[test]
fn primary_ox_to_iw() {
    let s = setup();
    let text = "x11host-primary-ox-to-iw";
    let mut xclip = xclip_copy_primary(s.compositor.host_display(), text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = wl_paste_primary(
        s.compositor.xdg_runtime_dir(),
        s.compositor.emskin_wayland(),
    );
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}
