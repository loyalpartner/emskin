//! Clipboard E2E tests with an **X11 host** (bare Xvfb, no host wayland).
//!
//! Covers the cross-boundary combinations involving a host-side X11
//! client (emskin's `ClipboardProxy` fallback connects to the host
//! Xvfb directly). Intentionally skips `里 W ↔ 里 X` because those go
//! through emskin's anvil-pattern internal bridge regardless of host,
//! and `e2e_clipboard_wayland.rs` already covers them.
//!
//! Roles:
//! - `iw` — inside emskin's wayland data_device
//! - `ix` — inside emskin's XWayland
//! - `ox` — outside on host Xvfb (no `ow` — host has no wayland)

mod common;
use common::{
    recv_one, wl_copy, wl_paste, wl_paste_primary, xclip_copy, xclip_copy_primary, xclip_paste,
    Compositor, NestedHost,
};

use std::time::Duration;

const DAEMON_SETTLE: Duration = Duration::from_millis(300);

struct Setup {
    compositor: Compositor,
    emskin_xwayland_display: String,
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
    let d = compositor.cache_xwayland_display(&mut stream);
    compositor.wait_for_emskin_wayland_socket(Duration::from_secs(5));
    Setup {
        compositor,
        emskin_xwayland_display: format!(":{d}"),
        _ipc: stream,
    }
}

// =============================================================================
// emskin sources → host X sink
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

#[test]
fn ix_to_ox() {
    let s = setup();
    let text = "x11host-ix-to-ox";
    let mut xclip = xclip_copy(&s.emskin_xwayland_display, text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(s.compositor.host_display());
    let _ = xclip.kill();
    let _ = xclip.wait();
    assert_eq!(got, text);
}

// =============================================================================
// host X source → emskin sinks
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

#[test]
fn ox_to_ix() {
    let s = setup();
    let text = "x11host-ox-to-ix";
    let mut xclip = xclip_copy(s.compositor.host_display(), text);
    std::thread::sleep(DAEMON_SETTLE);
    let got = xclip_paste(&s.emskin_xwayland_display);
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
