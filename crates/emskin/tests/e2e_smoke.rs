//! End-to-end smoke test.
//!
//! Spawns a real `emskin` subprocess and asserts the IPC handshake +
//! basic command survival. Requires a running Wayland or X11 session
//! (winit opens a real window); on headless CI wrap with `xvfb-run -a`.

mod common;
use common::{recv_one, send_one, Compositor};

#[test]
fn compositor_sends_connected_on_ipc_connect() {
    let compositor = Compositor::spawn();
    let mut stream = compositor.connect_ipc();
    let msg = recv_one(&mut stream);
    assert!(
        msg.contains(r#""type":"connected""#),
        "expected Connected handshake, got: {msg}"
    );
}

#[test]
fn set_measure_does_not_crash() {
    let compositor = Compositor::spawn();
    let mut stream = compositor.connect_ipc();
    let _ = recv_one(&mut stream);

    send_one(&mut stream, r#"{"type":"set_measure","enabled":true}"#);
    send_one(&mut stream, r#"{"type":"set_measure","enabled":false}"#);
    send_one(&mut stream, r#"{"type":"set_measure","enabled":true}"#);

    // Liveness probe: IpcServer accepts a new connection only if the
    // event loop is still running, so reconnecting proves the process
    // survived the previous messages.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut stream2 = compositor.connect_ipc();
    let handshake = recv_one(&mut stream2);
    assert!(
        handshake.contains(r#""type":"connected""#),
        "compositor did not survive SetMeasure toggles: {handshake}"
    );
}
