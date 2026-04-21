//! Event-driven integration tests for `XwlsIntegration`.
//!
//! Drives a real calloop `EventLoop<TestState>`, a mock satellite binary
//! (a shell script that either sleeps or exits immediately), and observes
//! state transitions + spawner-thread completion.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use smithay::reexports::calloop::{channel, EventLoop};

use emskin::xwayland_satellite::{setup_connection, HasXwls, SpawnConfig, ToMain, XwlsIntegration};

// ---------- helpers ----------

fn test_display_start() -> u32 {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    100 + ((ns as u32 ^ std::process::id()) % 1000)
}

fn tmpdir(tag: &str) -> PathBuf {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!(
        "emskin-xwls-watch-{}-{}-{}",
        tag,
        std::process::id(),
        ns
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    let mut f = fs::File::create(&p).unwrap();
    writeln!(f, "#!/bin/sh").unwrap();
    writeln!(f, "{body}").unwrap();
    drop(f);
    let mut perm = fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&p, perm).unwrap();
    p
}

fn spawn_config(binary: PathBuf) -> SpawnConfig {
    SpawnConfig {
        binary,
        wayland_socket: PathBuf::from("/run/user/1000/wayland-test"),
        xdg_runtime_dir: PathBuf::from("/run/user/1000"),
    }
}

/// Minimal calloop state that hosts an `XwlsIntegration` and exposes it
/// via the `HasXwls` trait.
struct TestState {
    xwls: Option<XwlsIntegration>,
}

impl HasXwls for TestState {
    fn xwls_mut(&mut self) -> Option<&mut XwlsIntegration> {
        self.xwls.as_mut()
    }
}

/// Wire the `ToMain` channel into `loop_handle` so that `Rearm` messages
/// dispatch into `on_rearm`. Mirrors what `main.rs` will do in
/// production.
fn install_rearm_handler(
    loop_handle: &smithay::reexports::calloop::LoopHandle<'static, TestState>,
    rx: channel::Channel<ToMain>,
) {
    let handle_for_rearm = loop_handle.clone();
    loop_handle
        .insert_source(rx, move |event, _, state| {
            if let channel::Event::Msg(ToMain::Rearm) = event {
                if let Some(x) = state.xwls_mut() {
                    let _ = x.on_rearm(&handle_for_rearm);
                }
            }
        })
        .unwrap();
}

/// Dispatch the loop until `pred(state)` is true or `deadline` elapses.
fn dispatch_until(
    event_loop: &mut EventLoop<'static, TestState>,
    state: &mut TestState,
    deadline: Instant,
    mut pred: impl FnMut(&TestState) -> bool,
) -> bool {
    while Instant::now() < deadline {
        if pred(state) {
            return true;
        }
        event_loop
            .dispatch(Some(Duration::from_millis(50)), state)
            .unwrap();
    }
    pred(state)
}

// ---------- tests ----------

#[test]
fn new_returns_disarmed_integration() {
    let dir = tmpdir("new");
    let bin = write_script(&dir, "sat", "exit 0");
    let sockets = setup_connection(test_display_start()).unwrap();
    let (tx, _rx) = channel::channel::<ToMain>();

    let x = XwlsIntegration::new(sockets, spawn_config(bin), tx);
    assert!(x.is_disarmed());
    assert!(!x.is_watching());
    assert!(!x.is_running());
    assert!(x.display_name().starts_with(':'));
}

#[test]
fn arm_transitions_to_watching_and_disarm_reverts() {
    let dir = tmpdir("arm");
    let bin = write_script(&dir, "sat", "sleep 30");
    let sockets = setup_connection(test_display_start()).unwrap();

    let event_loop: EventLoop<'static, TestState> = EventLoop::try_new().unwrap();
    let (tx, rx) = channel::channel::<ToMain>();
    install_rearm_handler(&event_loop.handle(), rx);

    let mut state = TestState {
        xwls: Some(XwlsIntegration::new(sockets, spawn_config(bin), tx)),
    };

    state
        .xwls_mut()
        .unwrap()
        .arm(&event_loop.handle())
        .expect("arm should install sources");
    assert!(state.xwls_mut().unwrap().is_watching());

    state.xwls_mut().unwrap().disarm(&event_loop.handle());
    assert!(state.xwls_mut().unwrap().is_disarmed());
}

#[test]
fn socket_connection_triggers_spawn_transitioning_to_running() {
    let dir = tmpdir("spawn");
    // Long-running mock so state stays Running while we assert.
    let bin = write_script(&dir, "sat", "sleep 30");
    let sockets = setup_connection(test_display_start()).unwrap();
    let unix_path = sockets.unix_socket_path();

    let mut event_loop: EventLoop<'static, TestState> = EventLoop::try_new().unwrap();
    let (tx, rx) = channel::channel::<ToMain>();
    install_rearm_handler(&event_loop.handle(), rx);

    let mut state = TestState {
        xwls: Some(XwlsIntegration::new(sockets, spawn_config(bin), tx)),
    };
    state.xwls_mut().unwrap().arm(&event_loop.handle()).unwrap();

    // A connect on the unix socket must make the Generic source readable.
    let _client = UnixStream::connect(&unix_path).expect("client connect");

    let deadline = Instant::now() + Duration::from_secs(3);
    let became_running = dispatch_until(&mut event_loop, &mut state, deadline, |s| {
        s.xwls.as_ref().is_some_and(|x| x.is_running())
    });
    assert!(became_running, "expected Running after socket connect");
}

#[test]
fn spawner_exit_triggers_rearm_via_channel() {
    let dir = tmpdir("rearm");
    // Mock satellite that exits immediately — the spawner thread observes
    // the exit and sends ToMain::Rearm.
    let bin = write_script(&dir, "sat", "exit 0");
    let sockets = setup_connection(test_display_start()).unwrap();
    let unix_path = sockets.unix_socket_path();

    let mut event_loop: EventLoop<'static, TestState> = EventLoop::try_new().unwrap();
    let (tx, rx) = channel::channel::<ToMain>();
    install_rearm_handler(&event_loop.handle(), rx);

    let mut state = TestState {
        xwls: Some(XwlsIntegration::new(sockets, spawn_config(bin), tx)),
    };
    // Race-free completion hook: spawner pings this after child exits but
    // before (or around) sending Rearm.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    state.xwls_mut().unwrap().set_spawner_done_hook(done_tx);

    state.xwls_mut().unwrap().arm(&event_loop.handle()).unwrap();

    let _client = UnixStream::connect(&unix_path).unwrap();

    // Drive dispatches until we observe Running (briefly) OR spawner done.
    let deadline = Instant::now() + Duration::from_secs(3);
    let _ = dispatch_until(&mut event_loop, &mut state, deadline, |_| {
        done_rx.try_recv().is_ok()
    });

    // Spawner thread is done → now we need the Rearm message dispatched.
    // Give the loop more turns; on_rearm should move state → Watching.
    let deadline = Instant::now() + Duration::from_secs(3);
    let back_to_watching = dispatch_until(&mut event_loop, &mut state, deadline, |s| {
        s.xwls.as_ref().is_some_and(|x| x.is_watching())
    });
    assert!(back_to_watching, "expected Watching after Rearm handled");
}

#[test]
fn full_cycle_spawns_twice() {
    let dir = tmpdir("cycle");
    let bin = write_script(&dir, "sat", "exit 0");
    let sockets = setup_connection(test_display_start()).unwrap();
    let unix_path = sockets.unix_socket_path();

    let mut event_loop: EventLoop<'static, TestState> = EventLoop::try_new().unwrap();
    let (tx, rx) = channel::channel::<ToMain>();
    install_rearm_handler(&event_loop.handle(), rx);

    let (done_tx, done_rx) = mpsc::channel::<()>();

    let mut state = TestState {
        xwls: Some(XwlsIntegration::new(sockets, spawn_config(bin), tx)),
    };
    state.xwls_mut().unwrap().set_spawner_done_hook(done_tx);
    state.xwls_mut().unwrap().arm(&event_loop.handle()).unwrap();

    // Cycle 1
    drop(UnixStream::connect(&unix_path).unwrap());
    let d1 = Instant::now() + Duration::from_secs(3);
    assert!(
        dispatch_until(&mut event_loop, &mut state, d1, |_| done_rx
            .try_recv()
            .is_ok()),
        "first cycle: spawner never completed"
    );
    let d2 = Instant::now() + Duration::from_secs(3);
    assert!(
        dispatch_until(&mut event_loop, &mut state, d2, |s| s
            .xwls
            .as_ref()
            .is_some_and(|x| x.is_watching())),
        "first cycle: never rearmed to Watching"
    );

    // Cycle 2 — a fresh connect should retrigger the spawn path.
    drop(UnixStream::connect(&unix_path).unwrap());
    let d3 = Instant::now() + Duration::from_secs(3);
    assert!(
        dispatch_until(&mut event_loop, &mut state, d3, |_| done_rx
            .try_recv()
            .is_ok()),
        "second cycle: spawner never completed"
    );
    let d4 = Instant::now() + Duration::from_secs(3);
    assert!(
        dispatch_until(&mut event_loop, &mut state, d4, |s| s
            .xwls
            .as_ref()
            .is_some_and(|x| x.is_watching())),
        "second cycle: never rearmed to Watching"
    );
}
