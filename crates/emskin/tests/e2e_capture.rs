//! Screenshot / recording E2E tests.
//!
//! Covers:
//! - `TakeScreenshot` IPC → PNG file produced on disk with a valid header.
//!   Exercises the GPU readback path in `capture.rs` (copy_framebuffer +
//!   map_texture split across winit's submit boundary — the fiddliest
//!   place in emskin outside the clipboard bridge).
//! - `SetRecording start` → sleep → `SetRecording stop` → `RecordingStopped`
//!   IPC arrives → mp4 file exists and has non-trivial size. Covers the
//!   state machine in `recording.rs` and the ffmpeg spawn integration.

mod common;
use common::{recv_one, send_one, Compositor};

use std::path::PathBuf;
use std::time::{Duration, Instant};

fn unique_output(suffix: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("emskin-e2e-capture-{pid}-{nanos}.{suffix}"))
}

fn wait_for_file_nonempty(path: &PathBuf, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(bytes) = std::fs::read(path) {
            if bytes.len() > 32 {
                return bytes;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "file {} did not reach >32 bytes within {:?}",
        path.display(),
        timeout
    );
}

/// Drain IPC messages until one matches `pred` (by string contains).
/// Typical use: `wait_for_message(&mut stream, r#""type":"recording_stopped""#)`.
fn wait_for_message(stream: &mut std::os::unix::net::UnixStream, needle: &str) -> String {
    for _ in 0..50 {
        let msg = recv_one(stream);
        if msg.contains(needle) {
            return msg;
        }
    }
    panic!("did not receive message matching {needle} after 50 reads");
}

#[test]
fn take_screenshot_produces_png() {
    let compositor = Compositor::spawn();
    let mut stream = compositor.connect_ipc();
    let _connected = recv_one(&mut stream);

    let out = unique_output("png");
    let _ = std::fs::remove_file(&out);

    let escaped = out.to_string_lossy().replace('\\', "\\\\");
    let msg = format!(r#"{{"type":"take_screenshot","path":"{escaped}"}}"#);
    send_one(&mut stream, &msg);

    let bytes = wait_for_file_nonempty(&out, Duration::from_secs(5));

    // PNG magic: 89 50 4E 47 0D 0A 1A 0A
    assert_eq!(
        &bytes[..8],
        &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        "output at {} is not a PNG (first 8 bytes: {:?})",
        out.display(),
        &bytes[..8.min(bytes.len())]
    );

    let _ = std::fs::remove_file(&out);
}

#[test]
fn recording_produces_mp4_and_emits_stopped() {
    if which("ffmpeg").is_none() {
        eprintln!("skipping recording test: ffmpeg not on PATH");
        return;
    }
    let compositor = Compositor::spawn();
    let mut stream = compositor.connect_ipc();
    let _connected = recv_one(&mut stream);

    let out = unique_output("mp4");
    let _ = std::fs::remove_file(&out);

    let escaped = out.to_string_lossy().replace('\\', "\\\\");
    send_one(
        &mut stream,
        &format!(r#"{{"type":"set_recording","enabled":true,"path":"{escaped}","fps":30}}"#),
    );

    // Let a few frames land. 800ms ≈ 24 frames at 30fps; plenty for a
    // valid mp4 and short enough to keep the test fast.
    std::thread::sleep(Duration::from_millis(800));

    send_one(&mut stream, r#"{"type":"set_recording","enabled":false}"#);

    let stopped = wait_for_message(&mut stream, r#""type":"recording_stopped""#);
    assert!(
        stopped.contains(r#""reason":"user""#),
        "expected user-stopped recording, got: {stopped}"
    );

    // ffmpeg finishes flushing asynchronously after the stop event —
    // give it a short grace period then assert the file exists.
    let bytes = wait_for_file_nonempty(&out, Duration::from_secs(5));
    assert!(
        bytes.len() > 1024,
        "mp4 suspiciously small: {} bytes",
        bytes.len()
    );

    let _ = std::fs::remove_file(&out);
}

fn which(cmd: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path).find_map(|dir| {
            let full = dir.join(cmd);
            if full.is_file() {
                Some(full)
            } else {
                None
            }
        })
    })
}
