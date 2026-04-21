//! Clipboard E2E tests with a **Wayland host** (emez — our smithay-based
//! headless compositor, `crates/emez/`).
//!
//! emez advertises both `zwlr_data_control_v1` and `ext_data_control_v1`,
//! so emskin's `ClipboardProxy` takes its primary wayland data_control
//! path (and stays off the X11 fallback in `clipboard_x11.rs`).
//!
//! emez also embeds its own XWayland (see `crates/emez/src/xwayland.rs`),
//! exposing a host-side X DISPLAY. This lets us exercise the
//! wayland-host ↔ outside-X combinations alongside the pure-wayland ones.
//!
//! Roles:
//! - `iw` — inside emskin's wayland data_device (wl-copy on emskin)
//! - `ow` — outside on host emez (wl-copy on emez's socket)
//! - `ox` — outside on host emez's embedded XWayland (xclip on host DISPLAY)
//!
//! There is no `ix` role: under `xwayland-satellite` every X client is
//! translated into a Wayland client before it reaches emskin, so from
//! the compositor's point of view there is simply no such thing as an
//! "internal X" peer. The X-side translation logic is satellite's
//! concern and has its own test suite upstream.
//!
//! Tests here therefore do not cover the X → Wayland propagation
//! performed by satellite — they only verify the wayland data paths on
//! emskin.

mod common;
use common::{recv_one, wl_copy, wl_paste, xclip_copy, xclip_paste, Compositor, NestedHost};

use std::time::Duration;

/// Allow the selection/daemon dance to settle. wl-copy forks a daemon
/// and needs to send `wl_data_source.offer` before readers can see
/// anything; host ↔ emskin propagation via data_control adds a few
/// round-trips too. 300ms is empirically ample.
const DAEMON_SETTLE: Duration = Duration::from_millis(300);

struct Setup {
    compositor: Compositor,
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
    // Wait for satellite's X socket to be pre-bound + XWaylandReady IPC.
    // Even though no test here spawns an X client, this is the cleanest
    // signal that emskin has finished the full startup path.
    let _ = compositor.cache_xwayland_display(&mut stream);
    compositor.wait_for_emskin_wayland_socket(Duration::from_secs(5));
    Setup {
        compositor,
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

// =============================================================================
// ox_* : outside X client on host emez's embedded XWayland
// =============================================================================

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
