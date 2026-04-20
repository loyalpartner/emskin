//! Shared E2E test harness.
//!
//! Every test spawns a **private** host compositor (either `emez`, our
//! smithay-based headless Wayland host in `crates/emez/`, or a
//! standalone `Xvfb`) and then an emskin instance on top of it. Each
//! pair has its own `XDG_RUNTIME_DIR`, wayland socket names, and X
//! DISPLAY, so parallel tests are naturally isolated — no shared X
//! CLIPBOARD to cross-contaminate, no env var escape hatches.
//!
//! Drop order is significant: Compositor kills emskin first, then lets
//! the NestedHost field drop to kill emez/Xvfb.

#![allow(dead_code)] // Some helpers are only used by a subset of test files.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// =============================================================================
// NestedHost — private emez or Xvfb per test
// =============================================================================

/// One private host compositor owned by a single Compositor. Two variants:
///
/// - `Wayland`: emez (our smithay-based headless host) + its embedded
///   XWayland. Provides both a wayland socket and an X DISPLAY.
/// - `X11`: bare Xvfb. Provides only an X DISPLAY.
pub enum NestedHost {
    Wayland(WaylandHost),
    X11(X11Host),
}

impl NestedHost {
    pub fn wayland() -> Self {
        NestedHost::Wayland(WaylandHost::spawn(false))
    }

    /// Spawn a Wayland host that hides `zwlr_data_control_v1` and
    /// `ext_data_control_v1` from every client. Used to exercise
    /// emskin's `wl_data_device` clipboard fallback under a KDE-like
    /// host where data-control simply isn't advertised.
    pub fn wayland_no_data_control() -> Self {
        NestedHost::Wayland(WaylandHost::spawn(true))
    }

    pub fn x11() -> Self {
        NestedHost::X11(X11Host::spawn())
    }

    /// Host-side wayland socket name. `None` for X11 hosts.
    pub fn wayland_socket(&self) -> Option<&str> {
        match self {
            NestedHost::Wayland(w) => Some(&w.wayland_socket_name),
            NestedHost::X11(_) => None,
        }
    }

    /// Host-side X DISPLAY, always present.
    pub fn display(&self) -> &str {
        match self {
            NestedHost::Wayland(w) => &w.display,
            NestedHost::X11(x) => &x.display,
        }
    }

    pub fn xdg_runtime_dir(&self) -> &Path {
        match self {
            NestedHost::Wayland(w) => &w.xdg_runtime_dir,
            NestedHost::X11(x) => &x.xdg_runtime_dir,
        }
    }

    pub fn log_tail(&self) -> String {
        match self {
            NestedHost::Wayland(w) => std::fs::read_to_string(&w.log_file)
                .unwrap_or_else(|e| format!("<could not read {}: {e}>", w.log_file.display())),
            NestedHost::X11(_) => "<Xvfb stderr not captured>".into(),
        }
    }
}

// -----------------------------------------------------------------------------

pub struct WaylandHost {
    child: Child,
    xdg_runtime_dir: PathBuf,
    wayland_socket_name: String,
    log_file: PathBuf,
    display: String,
    // Drop order: child dies first (signal + wait), then this slot
    // releases its DISPLAY number back to the pool so the next test can
    // grab it. Keep this field *after* `child` in declaration order.
    _display_slot: DisplaySlot,
}

impl WaylandHost {
    fn spawn(hide_data_control: bool) -> Self {
        let xdg = make_private_tempdir("emskin-host");
        let wayland_socket_name = format!("emskin-host-{}", unique_suffix());
        let log_file = xdg.join("emez.log");
        let ready_file = xdg.join("xwayland-ready");

        // Pre-allocate an X DISPLAY number for emez's embedded XWayland.
        // This eliminates the /tmp/.X11-unix/X* linear-scan race that
        // smithay does when display=None is passed to XWayland::spawn —
        // the harness assigns a unique number per test so parallel emez
        // instances cannot collide.
        let display_slot =
            DisplaySlot::reserve().expect("exhausted DISPLAY candidates for emez XWayland");
        let display_num = display_slot.num();
        let display = format!(":{display_num}");

        // `emez` is emskin's sister test compositor (crates/emez) — a
        // smithay-based headless wayland host that advertises
        // zwlr_data_control_v1 + ext_data_control_v1 (which emskin's
        // ClipboardProxy needs) and embeds XWayland so outside X
        // clients can participate in Wayland-host clipboard tests.
        //
        // When `hide_data_control` is set we pass `--no-data-control` so
        // emez stops advertising those globals entirely — simulating a
        // KDE/GNOME host and forcing emskin onto its `wl_data_device`
        // fallback.
        let emez_bin = find_emez_binary();
        let mut cmd = Command::new(&emez_bin);
        cmd.arg("--socket")
            .arg(&wayland_socket_name)
            .arg("--log-file")
            .arg(&log_file)
            .arg("--xwayland")
            .arg("--xwayland-display")
            .arg(display_num.to_string())
            .arg("--xwayland-ready-file")
            .arg(&ready_file);
        if hide_data_control {
            cmd.arg("--no-data-control");
        }
        let child = cmd
            .env("XDG_RUNTIME_DIR", &xdg)
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            )
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn emez — build with `cargo build -p emez`");

        // Wait for the wayland socket to bind.
        let sock_path = xdg.join(&wayland_socket_name);
        wait_for_path(&sock_path, Duration::from_secs(10))
            .unwrap_or_else(|| panic!("emez wayland socket never appeared"));

        // Wait for XWayland to report Ready via the ready file. XWayland
        // startup is ~100-300ms; give it a generous budget.
        wait_for_path(&ready_file, Duration::from_secs(10)).unwrap_or_else(|| {
            panic!(
                "emez XWayland never reported Ready on {display} (log: {})",
                log_file.display()
            )
        });

        Self {
            child,
            xdg_runtime_dir: xdg,
            wayland_socket_name,
            log_file,
            display,
            _display_slot: display_slot,
        }
    }
}

impl Drop for WaylandHost {
    fn drop(&mut self) {
        // SIGTERM first so emez can run its Drop chain — that's how
        // smithay's `XWayland` struct gets a chance to kill the XWayland
        // child. std's `Child::kill()` sends SIGKILL, which skips Drop
        // and leaves XWayland as an orphan holding its X display socket.
        graceful_kill(&mut self.child, Duration::from_millis(1500));
        // Keep the xdg_runtime_dir (and its emez.log) when
        // EMSKIN_E2E_KEEP_LOGS is set; useful for diagnosing test
        // failures by inspecting the emez log after the harness exits.
        if std::env::var_os("EMSKIN_E2E_KEEP_LOGS").is_none() {
            let _ = std::fs::remove_dir_all(&self.xdg_runtime_dir);
        } else {
            eprintln!(
                "[harness] keeping host dir for inspection: {}",
                self.xdg_runtime_dir.display()
            );
        }
    }
}

/// Locate the `emez` test-host binary. Cargo sets `CARGO_BIN_EXE_<name>`
/// only for the crate that owns the binary; since emez is a sibling
/// crate, we walk from emskin's manifest dir up to the workspace root
/// and look for `target/{debug,release}/emez` (or `$CARGO_TARGET_DIR`).
fn find_emez_binary() -> PathBuf {
    if let Ok(p) = std::env::var("EMEZ_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return p;
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("emskin crate is nested at crates/emskin in the workspace");
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join("target"));
    for profile in ["debug", "release"] {
        let candidate = target_dir.join(profile).join("emez");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "emez binary not found under {}. Build it first: `cargo build -p emez`.",
        target_dir.display()
    );
}

// -----------------------------------------------------------------------------

pub struct X11Host {
    child: Child,
    xdg_runtime_dir: PathBuf,
    display: String,
    _display_slot: DisplaySlot,
}

impl X11Host {
    fn spawn() -> Self {
        let xdg = make_private_tempdir("emskin-host-x11");

        // Up to 5 attempts: DisplaySlot filters out occupied numbers,
        // but there's still a micro-race between reservation and Xvfb
        // actually binding the socket (another process could squat
        // between our check and Xvfb's bind). A single retry normally
        // suffices.
        for _ in 0..5 {
            let slot = DisplaySlot::reserve().expect("exhausted DISPLAY candidates for X11 host");
            let disp_num = slot.num();
            let sock = PathBuf::from(format!("/tmp/.X11-unix/X{disp_num}"));
            match Command::new("Xvfb")
                .arg(format!(":{disp_num}"))
                .arg("-screen")
                .arg("0")
                .arg("1280x800x24")
                .arg("-nolisten")
                .arg("tcp")
                .env("XDG_RUNTIME_DIR", &xdg)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    if wait_for_path(&sock, Duration::from_secs(3)).is_some() {
                        return Self {
                            child,
                            xdg_runtime_dir: xdg,
                            display: format!(":{disp_num}"),
                            _display_slot: slot,
                        };
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    // `slot` drops here, releasing the reservation so
                    // the next iteration picks a different number.
                }
                Err(e) => panic!("failed to spawn Xvfb — is it installed? {e}"),
            }
        }
        panic!("X11Host: Xvfb failed to bind after 5 reservation attempts");
    }
}

impl Drop for X11Host {
    fn drop(&mut self) {
        // Xvfb itself responds to SIGTERM cleanly; use the same graceful
        // path as WaylandHost for symmetry and to keep the shutdown
        // behavior consistent across test hosts.
        graceful_kill(&mut self.child, Duration::from_millis(1500));
        let _ = std::fs::remove_dir_all(&self.xdg_runtime_dir);
    }
}

// -----------------------------------------------------------------------------

fn make_private_tempdir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", unique_suffix()));
    std::fs::create_dir_all(&path).expect("mkdir tempdir");
    // Wayland runtime dirs are required by protocol to be 0700.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o700);
        let _ = std::fs::set_permissions(&path, perm);
    }
    path
}

/// Send SIGTERM to the child process and wait up to `timeout` for it
/// to exit. If it's still alive after the deadline, escalate to
/// SIGKILL. Needed because `std::process::Child::kill()` sends SIGKILL
/// directly, which skips Drop on the child and leaves its grandchildren
/// (notably XWayland spawned by emez) as orphans.
fn graceful_kill(child: &mut Child, timeout: Duration) {
    let pid = child.id() as libc::pid_t;
    // SAFETY: libc::kill with a valid pid is always safe; an invalid
    // pid just returns -1 which we ignore.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    let _ = child.kill(); // SIGKILL fallback
    let _ = child.wait();
}

/// Process-wide pool of DISPLAY numbers handed out by `DisplaySlot`.
/// Keeps parallel tests (and the emez + emskin pair within a single
/// test) from picking the same number.
fn reserved_displays() -> &'static Mutex<Vec<u32>> {
    static RESERVED: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();
    RESERVED.get_or_init(|| Mutex::new(Vec::new()))
}

/// RAII guard for one reserved X DISPLAY number. Scans
/// `/tmp/.X11-unix/X<N>` + `/tmp/.X<N>-lock` for filesystem-level
/// availability *and* checks the in-process pool to avoid two tests
/// sharing a number. Released automatically on Drop.
pub struct DisplaySlot {
    num: u32,
}

impl DisplaySlot {
    /// Reserve a free DISPLAY number in the [50, 999] range (user
    /// session usually lives at :0/:1). Returns None only if all 200
    /// scanned candidates happened to collide — extremely unlikely.
    pub fn reserve() -> Option<Self> {
        let pid = std::process::id();
        let base = 50 + ((pid as u64 + unique_nanos()) % 400) as u32;
        let mut reserved = reserved_displays().lock().unwrap();
        for offset in 0..200 {
            let n = base + offset;
            if n > 999 {
                break;
            }
            if reserved.contains(&n) {
                continue;
            }
            if PathBuf::from(format!("/tmp/.X11-unix/X{n}")).exists() {
                continue;
            }
            if PathBuf::from(format!("/tmp/.X{n}-lock")).exists() {
                continue;
            }
            reserved.push(n);
            return Some(Self { num: n });
        }
        None
    }

    pub fn num(&self) -> u32 {
        self.num
    }
}

impl Drop for DisplaySlot {
    fn drop(&mut self) {
        reserved_displays()
            .lock()
            .unwrap()
            .retain(|&x| x != self.num);
    }
}

fn wait_for_path(path: &Path, timeout: Duration) -> Option<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Some(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

// =============================================================================
// Compositor — emskin subprocess running on a NestedHost
// =============================================================================

pub struct Compositor {
    // Field drop order:
    //   1. `child` — emskin dies first (custom Drop SIGTERMs + waits)
    //   2. `_emskin_display_slot` — releases the DISPLAY reservation
    //   3. `host` — kills emez/Xvfb and releases its own slot
    // Reversing steps 1 and 3 risks emskin getting EPIPE on its host
    // connection during shutdown.
    child: Child,
    ipc_socket: PathBuf,
    log_file: PathBuf,
    emskin_wayland_socket: String,
    /// Pre-allocated DISPLAY number passed to emskin via
    /// `--xwayland-display`. Always set by `spawn_on`.
    emskin_xwayland_display: u32,
    _emskin_display_slot: DisplaySlot,
    host: NestedHost,
}

impl Compositor {
    /// Shortcut: spawn emskin on a fresh Wayland host. Used by tests that
    /// don't care about the host variant.
    pub fn spawn() -> Self {
        Self::spawn_on(NestedHost::wayland())
    }

    pub fn spawn_on(host: NestedHost) -> Self {
        let emskin_wayland_socket = format!("emskin-{}", unique_suffix());
        let ipc_socket = unique_tempfile("emskin-e2e", "ipc");
        let log_file = unique_tempfile("emskin-e2e", "log");
        let _ = std::fs::remove_file(&ipc_socket);
        let _ = std::fs::remove_file(&log_file);

        // Pre-allocate emskin's XWayland DISPLAY. Without this, smithay
        // scans /tmp/.X11-unix/X0..X32 inside emskin and races with
        // parallel tests spawning their own emskin instances. The
        // reservation pool guarantees this number is disjoint from
        // every other in-flight test (and from the emez/Xvfb host slot).
        let emskin_display_slot =
            DisplaySlot::reserve().expect("exhausted DISPLAY candidates for emskin's XWayland");
        let emskin_xwayland_display = emskin_display_slot.num();

        let binary = env!("CARGO_BIN_EXE_emskin");
        let mut cmd = Command::new(binary);
        cmd.arg("--no-spawn")
            .arg("--bar")
            .arg("none")
            .arg("--ipc-path")
            .arg(&ipc_socket)
            .arg("--log-file")
            .arg(&log_file)
            .arg("--wayland-socket")
            .arg(&emskin_wayland_socket)
            .arg("--xwayland-display")
            .arg(emskin_xwayland_display.to_string())
            .env(
                "RUST_LOG",
                std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            )
            .env("XDG_RUNTIME_DIR", host.xdg_runtime_dir())
            .env("DISPLAY", host.display())
            // Skip the "first toplevel = Emacs" heuristic so that
            // transient test clients (wl-copy, xclip, …) don't get
            // misidentified as the Emacs frame.
            .env("EMSKIN_DISABLE_EMACS_DETECTION", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // Wayland host: hand WAYLAND_DISPLAY to winit → wayland backend.
        // X11 host: leave WAYLAND_DISPLAY unset → winit falls back to X11.
        if let Some(sock) = host.wayland_socket() {
            cmd.env("WAYLAND_DISPLAY", sock);
        } else {
            cmd.env_remove("WAYLAND_DISPLAY");
        }

        let child = cmd.spawn().expect("failed to spawn emskin");

        let compositor = Self {
            child,
            ipc_socket,
            log_file,
            emskin_wayland_socket,
            emskin_xwayland_display,
            _emskin_display_slot: emskin_display_slot,
            host,
        };
        compositor.wait_for_ipc_socket(Duration::from_secs(10));
        compositor
    }

    fn wait_for_ipc_socket(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.ipc_socket.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "IPC socket {} not created within {:?}",
            self.ipc_socket.display(),
            timeout
        );
    }

    pub fn wait_for_emskin_wayland_socket(&self, timeout: Duration) {
        let sock = self
            .host
            .xdg_runtime_dir()
            .join(&self.emskin_wayland_socket);
        if wait_for_path(&sock, timeout).is_none() {
            panic!("emskin wayland socket {} did not appear", sock.display());
        }
    }

    pub fn connect_ipc(&self) -> UnixStream {
        let stream = UnixStream::connect(&self.ipc_socket).expect("connect ipc");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream
    }

    /// Drain incoming IPC messages until `x_wayland_ready` arrives and
    /// return the emskin-internal XWayland DISPLAY number. Acts as a
    /// synchronization barrier: callers can assume the XWayland is
    /// accepting X connections once this returns. The display itself
    /// is already known from `--xwayland-display`, but we still wait on
    /// the IPC message and sanity-check it matches.
    pub fn cache_xwayland_display(&mut self, stream: &mut UnixStream) -> u32 {
        let d = wait_for_xwayland_ready(stream);
        assert_eq!(
            d, self.emskin_xwayland_display,
            "emskin reported XWayland display :{d}, but --xwayland-display pinned :{}",
            self.emskin_xwayland_display
        );
        d
    }

    /// Wayland socket name emskin advertises for its own clients.
    pub fn emskin_wayland(&self) -> &str {
        &self.emskin_wayland_socket
    }

    /// Host-side wayland socket name, if the host is a Wayland host.
    pub fn host_wayland(&self) -> Option<&str> {
        self.host.wayland_socket()
    }

    pub fn host_display(&self) -> &str {
        self.host.display()
    }

    pub fn xdg_runtime_dir(&self) -> &Path {
        self.host.xdg_runtime_dir()
    }

    /// emskin-internal XWayland DISPLAY string (always available because
    /// the harness pre-allocates it before spawning emskin).
    pub fn emskin_display(&self) -> String {
        format!(":{}", self.emskin_xwayland_display)
    }

    pub fn log_tail(&self) -> String {
        std::fs::read_to_string(&self.log_file)
            .unwrap_or_else(|e| format!("<could not read {}: {e}>", self.log_file.display()))
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "\n--- emskin log ({}) ---\n{}\n--- end emskin log ---\n\
                 --- host log ---\n{}\n--- end host log ---",
                self.log_file.display(),
                self.log_tail(),
                self.host.log_tail(),
            );
        }
        graceful_kill(&mut self.child, Duration::from_millis(1500));
        // Belt-and-braces X socket cleanup for emskin's XWayland. When
        // emskin exits via signal the Rust Drop chain can be truncated
        // before smithay's X11Lock::Drop runs, leaving
        // /tmp/.X11-unix/X<N> and /tmp/.X<N>-lock behind. emskin's side
        // doesn't expose an explicit cleanup hook, so the harness owns
        // this best-effort removal using the number it pre-allocated
        // for `--xwayland-display`.
        let n = self.emskin_xwayland_display;
        let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{n}"));
        let _ = std::fs::remove_file(format!("/tmp/.X{n}-lock"));
        let _ = std::fs::remove_file(&self.ipc_socket);
        if std::env::var_os("EMSKIN_E2E_KEEP_LOGS").is_none() {
            let _ = std::fs::remove_file(&self.log_file);
        } else {
            eprintln!(
                "[harness] keeping emskin log for inspection: {}",
                self.log_file.display()
            );
        }
        // self.host drops next, taking down emez/Xvfb.
    }
}

fn unique_tempfile(prefix: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{prefix}-{}.{ext}", unique_suffix()))
}

fn unique_suffix() -> String {
    format!("{}-{}", std::process::id(), unique_nanos())
}

fn unique_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

// =============================================================================
// IPC framing helpers
// =============================================================================

/// Read one length-prefixed JSON message as a UTF-8 string.
pub fn recv_one(stream: &mut UnixStream) -> String {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).expect("read header");
    let len = u32::from_le_bytes(header) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).expect("read payload");
    String::from_utf8(payload).expect("utf-8 payload")
}

/// Send one length-prefixed JSON message.
pub fn send_one(stream: &mut UnixStream, json: &str) {
    let bytes = json.as_bytes();
    let len = (bytes.len() as u32).to_le_bytes();
    stream.write_all(&len).expect("write header");
    stream.write_all(bytes).expect("write payload");
}

/// Drain incoming IPC messages until `x_wayland_ready` arrives; return the
/// emskin-embedded XWayland's display number.
pub fn wait_for_xwayland_ready(stream: &mut UnixStream) -> u32 {
    for _ in 0..30 {
        let msg = recv_one(stream);
        if msg.contains(r#""type":"x_wayland_ready""#) {
            return parse_xwayland_display(&msg)
                .unwrap_or_else(|| panic!("x_wayland_ready missing display field: {msg}"));
        }
    }
    panic!("x_wayland_ready not received after 30 messages");
}

fn parse_xwayland_display(json: &str) -> Option<u32> {
    let key = r#""display":"#;
    let start = json.find(key)? + key.len();
    let tail = &json[start..];
    let end = tail
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

// =============================================================================
// Test-client helpers (wl-copy / wl-paste / xclip)
// =============================================================================
//
// Every helper takes **explicit** XDG_RUNTIME_DIR + socket/display, not
// relying on env inheritance. This is what makes per-test hosts work
// without risk of bleed-through.

/// Set a wayland CLIPBOARD selection. wl-copy forks a daemon that owns
/// the selection until another client takes it over or WAYLAND_DISPLAY
/// disappears. The foreground process returns immediately.
pub fn wl_copy(xdg: &Path, socket: &str, text: &str) {
    wl_copy_inner(xdg, socket, text, &[]);
}

/// Set a wayland PRIMARY selection (middle-click paste).
pub fn wl_copy_primary(xdg: &Path, socket: &str, text: &str) {
    wl_copy_inner(xdg, socket, text, &["--primary"]);
}

fn wl_copy_inner(xdg: &Path, socket: &str, text: &str, extra_args: &[&str]) {
    let mut cmd = Command::new("wl-copy");
    for a in extra_args {
        cmd.arg(a);
    }
    let status = cmd
        .arg("--")
        .arg(text)
        .env("XDG_RUNTIME_DIR", xdg)
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("WAYLAND_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn wl-copy");
    assert!(status.success(), "wl-copy exit={status}");
}

/// Read a wayland CLIPBOARD selection via `timeout 3 wl-paste --no-newline`.
pub fn wl_paste(xdg: &Path, socket: &str) -> String {
    wl_paste_inner(xdg, socket, &[])
}

/// Read a wayland PRIMARY selection.
pub fn wl_paste_primary(xdg: &Path, socket: &str) -> String {
    wl_paste_inner(xdg, socket, &["--primary"])
}

fn wl_paste_inner(xdg: &Path, socket: &str, extra_args: &[&str]) -> String {
    let mut cmd = Command::new("timeout");
    cmd.arg("--signal=KILL").arg("3").arg("wl-paste");
    for a in extra_args {
        cmd.arg(a);
    }
    let output = cmd
        .arg("--no-newline")
        .env("XDG_RUNTIME_DIR", xdg)
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("WAYLAND_SOCKET")
        .stderr(Stdio::piped())
        .output()
        .expect("spawn wl-paste");
    if !output.status.success() {
        panic!(
            "wl-paste failed (exit={}): stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).expect("utf-8 wl-paste stdout")
}

/// Set an X CLIPBOARD selection. xclip's default `-loops=0` means the
/// foreground process **never exits** — it stays resident as selection
/// owner. Caller must keep the returned `Child` alive until the test
/// reads, then drop/kill.
pub fn xclip_copy(display: &str, text: &str) -> Child {
    xclip_copy_inner(display, text, "clipboard")
}

pub fn xclip_copy_primary(display: &str, text: &str) -> Child {
    xclip_copy_inner(display, text, "primary")
}

fn xclip_copy_inner(display: &str, text: &str, selection: &str) -> Child {
    let mut child = Command::new("xclip")
        .arg("-i")
        .arg("-selection")
        .arg(selection)
        .env("DISPLAY", display)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xclip -i");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(text.as_bytes())
        .expect("pipe to xclip");
    drop(child.stdin.take()); // close stdin → xclip commits the selection
    child
}

/// Read the X CLIPBOARD selection via `timeout 3 xclip -o`. xclip with
/// `-o` prints to stdout and exits — no daemon.
pub fn xclip_paste(display: &str) -> String {
    xclip_paste_inner(display, "clipboard")
}

pub fn xclip_paste_primary(display: &str) -> String {
    xclip_paste_inner(display, "primary")
}

fn xclip_paste_inner(display: &str, selection: &str) -> String {
    let output = Command::new("timeout")
        .arg("--signal=KILL")
        .arg("3")
        .arg("xclip")
        .arg("-o")
        .arg("-selection")
        .arg(selection)
        .env("DISPLAY", display)
        .stderr(Stdio::piped())
        .output()
        .expect("spawn xclip -o");
    if !output.status.success() {
        panic!(
            "xclip -o failed (exit={}): stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).expect("utf-8 xclip stdout")
}
