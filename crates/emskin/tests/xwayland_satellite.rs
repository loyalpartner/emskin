//! Integration tests for the niri-pattern xwayland-satellite helpers.
//!
//! Covers the pure pieces (socket pre-binding + spawn-command construction).
//! Event-loop integration is out of scope for this file.

use std::fs;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use emskin::xwayland_satellite::sockets::{clear_out_pending_connections, Unlink};
use emskin::xwayland_satellite::{
    build_spawn_command, setup_connection, test_ondemand, SpawnConfig, X11Sockets,
};

// -----------------------------------------------------------------
// helpers

/// Pick a high, process-unique starting display to avoid clashing with
/// whatever the dev machine already has on `:0..:9`. Uses nanotime bits
/// so parallel test runs collide only with astronomical probability.
fn test_display_start() -> u32 {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Display >= 100 is well clear of any real X server on a dev machine.
    100 + ((ns as u32 ^ std::process::id()) % 1000)
}

fn tmpdir(tag: &str) -> PathBuf {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("emskin-xwls-{}-{}-{}", tag, std::process::id(), ns));
    fs::create_dir_all(&d).unwrap();
    d
}

/// Write an executable shell script into `dir/name` that runs `body` and
/// returns it as a path suitable for `test_ondemand` / `build_spawn_command`.
fn write_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
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

// -----------------------------------------------------------------
// X11Sockets

#[test]
fn setup_connection_binds_free_display_and_exposes_paths() {
    let start = test_display_start();
    let sockets: X11Sockets = setup_connection(start).expect("setup_connection should succeed");

    assert!(
        sockets.display >= start,
        "display {} should be >= start {start}",
        sockets.display
    );
    assert_eq!(sockets.display_name, format!(":{}", sockets.display));
    assert!(
        sockets.lock_path().exists(),
        "lock file {:?} should exist",
        sockets.lock_path()
    );
    assert!(
        sockets.unix_socket_path().exists(),
        "unix socket {:?} should exist",
        sockets.unix_socket_path()
    );
    assert!(
        sockets.unix_fd.as_raw_fd() >= 0,
        "unix_fd should be a valid fd"
    );
}

#[test]
fn setup_connection_skips_already_locked_display() {
    let start = test_display_start();
    let blocked_lock = PathBuf::from(format!("/tmp/.X{start}-lock"));

    // Guard regardless of outcome.
    let _cleanup = Unlink::new(blocked_lock.clone());

    // Pre-create the lock so `start` is unusable. `O_EXCL|O_CREAT` in the
    // impl must cause `pick_x11_display` to advance.
    fs::File::create(&blocked_lock).unwrap();

    let sockets = setup_connection(start).expect("should advance past locked display");
    assert!(
        sockets.display > start,
        "should skip locked :{start}, got :{}",
        sockets.display
    );
}

#[test]
fn dropping_x11sockets_unlinks_lock_and_socket() {
    let start = test_display_start();
    let sockets = setup_connection(start).unwrap();
    let lock = sockets.lock_path();
    let sock = sockets.unix_socket_path();
    assert!(lock.exists() && sock.exists());

    drop(sockets);

    assert!(!lock.exists(), "lock should be unlinked on drop");
    assert!(!sock.exists(), "unix socket should be unlinked on drop");
}

#[test]
fn unix_fd_accepts_a_client_connection() {
    let start = test_display_start();
    let sockets = setup_connection(start).unwrap();

    // Spawn a tiny client thread — accept() is blocking.
    let path = sockets.unix_socket_path();
    let t = std::thread::spawn(move || UnixStream::connect(&path).unwrap());

    let listener = UnixListener::from(sockets.unix_fd.try_clone().unwrap());
    let (_server, _addr) = listener
        .accept()
        .expect("accept should succeed on our pre-bound socket");
    let _client = t.join().unwrap();
}

#[test]
fn clear_out_pending_connections_drains_but_keeps_listener_usable() {
    // Standalone listener to keep this test independent of display allocation.
    let dir = tmpdir("drain");
    let sock = dir.join("s");
    let listener = UnixListener::bind(&sock).unwrap();

    // Two queued clients.
    let _c1 = UnixStream::connect(&sock).unwrap();
    let _c2 = UnixStream::connect(&sock).unwrap();

    let fd = clear_out_pending_connections(listener.into());
    // A third client should still be able to connect + be accepted through the same fd.
    let _c3 = UnixStream::connect(&sock).unwrap();
    let listener = UnixListener::from(fd);
    let (_accepted, _) = listener.accept().expect("listener should still accept");
}

// -----------------------------------------------------------------
// test_ondemand

#[test]
fn test_ondemand_true_when_binary_exits_zero() {
    let dir = tmpdir("ondemand-ok");
    let bin = write_script(&dir, "sat", "exit 0");
    assert!(test_ondemand(&bin));
}

#[test]
fn test_ondemand_false_when_binary_exits_nonzero() {
    let dir = tmpdir("ondemand-fail");
    let bin = write_script(&dir, "sat", "exit 1");
    assert!(!test_ondemand(&bin));
}

#[test]
fn test_ondemand_false_when_binary_missing() {
    assert!(!test_ondemand(std::path::Path::new(
        "/nonexistent/definitely-not-a-real-binary"
    )));
}

// -----------------------------------------------------------------
// build_spawn_command

fn spawn_config(binary: PathBuf) -> SpawnConfig {
    SpawnConfig {
        binary,
        wayland_socket: PathBuf::from("/run/user/1000/wayland-emskin"),
        xdg_runtime_dir: PathBuf::from("/run/user/1000"),
    }
}

#[test]
fn spawn_command_argv_prefix_is_display_then_listenfds() {
    let dir = tmpdir("argv");
    let bin = write_script(&dir, "sat", "exit 0");
    let start = test_display_start();
    let sockets = setup_connection(start).unwrap();
    let cfg = spawn_config(bin);

    let cmd = build_spawn_command(&cfg, &sockets);
    let args: Vec<&std::ffi::OsStr> = cmd.get_args().collect();
    let args_str: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert_eq!(args_str[0], sockets.display_name, "first arg must be :N");

    // -listenfd appears at least once and is always followed by a parseable fd.
    let mut saw_unix = false;
    let mut i = 1;
    while i < args_str.len() {
        if args_str[i] == "-listenfd" {
            let fd: i32 = args_str
                .get(i + 1)
                .expect("-listenfd must be followed by an fd")
                .parse()
                .expect("fd arg must be numeric");
            assert!(fd >= 0);
            saw_unix = true;
            i += 2;
        } else {
            i += 1;
        }
    }
    assert!(saw_unix, "expected at least one -listenfd argument");
}

#[test]
fn spawn_command_env_sets_wayland_display_and_runtime_dir() {
    let dir = tmpdir("env");
    let bin = write_script(&dir, "sat", "exit 0");
    let start = test_display_start();
    let sockets = setup_connection(start).unwrap();
    let cfg = spawn_config(bin);

    let cmd = build_spawn_command(&cfg, &sockets);
    let envs: Vec<(String, Option<String>)> = cmd
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|v| v.to_string_lossy().into_owned()),
            )
        })
        .collect();

    let wayland = envs
        .iter()
        .find(|(k, _)| k == "WAYLAND_DISPLAY")
        .expect("WAYLAND_DISPLAY must be set");
    assert_eq!(
        wayland.1.as_deref(),
        Some("/run/user/1000/wayland-emskin"),
        "WAYLAND_DISPLAY should match spawn_config",
    );

    let runtime = envs
        .iter()
        .find(|(k, _)| k == "XDG_RUNTIME_DIR")
        .expect("XDG_RUNTIME_DIR must be set");
    assert_eq!(runtime.1.as_deref(), Some("/run/user/1000"));
}

#[test]
fn spawn_command_env_removes_display() {
    let dir = tmpdir("env-rm");
    let bin = write_script(&dir, "sat", "exit 0");
    let start = test_display_start();
    let sockets = setup_connection(start).unwrap();
    let cfg = spawn_config(bin);

    let cmd = build_spawn_command(&cfg, &sockets);
    let removed: bool = cmd
        .get_envs()
        .any(|(k, v)| k.to_string_lossy() == "DISPLAY" && v.is_none());
    assert!(
        removed,
        "DISPLAY should be marked for removal so the child doesn't inherit host :0"
    );
}

#[test]
fn spawn_command_uses_configured_binary() {
    let dir = tmpdir("bin");
    let bin = write_script(&dir, "sat", "exit 0");
    let start = test_display_start();
    let sockets = setup_connection(start).unwrap();
    let cfg = spawn_config(bin.clone());

    let cmd = build_spawn_command(&cfg, &sockets);
    assert_eq!(
        std::path::Path::new(cmd.get_program()),
        bin,
        "program should be the configured binary path"
    );
    // Sanity: let the future GREEN impl drop before we exit the test — both
    // guard files must be unlinked so we don't pollute /tmp on assertion
    // failures.
    drop(cmd);
    drop(sockets);
    let _ = Duration::from_millis(0); // keep `time` import used in this test block
}
