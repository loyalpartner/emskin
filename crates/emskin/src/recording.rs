//! Screenshot / screen-record state machine.
//!
//! Two consumers share the same capture pipeline:
//!
//! - **One-shot screenshot** — `request_screenshot(path)` → next frame is
//!   captured, finalized as a PNG, state returns to `Idle`.
//! - **Continuous recording** — `request_recording(path, fps)` → next
//!   frame spawns an ffmpeg subprocess, every subsequent frame that
//!   meets the fps budget is pushed to ffmpeg's stdin; `stop_recording()`
//!   flushes the last pending frame and closes the encoder.
//!
//! Each render tick drives the state machine through two phases:
//!
//! 1. Inside `backend.bind()`, after `render_workspace`:
//!    [`Recorder::capture_frame`] fills a PBO (the EGL surface is still
//!    the current draw target, safe for `copy_framebuffer`).
//! 2. After `backend.submit`:
//!    [`Recorder::finalize`] maps the PBO (`map_texture` detaches the
//!    EGL surface, which is fine post-swap) and hands the bytes to the
//!    active sink — PNG writer for screenshots, ffmpeg stdin for video.

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smithay::{
    backend::renderer::{gles::GlesRenderer, RendererSuper},
    output::Output,
    utils::{Physical, Size},
};

use crate::capture::{CaptureError, CapturedFrame, Capturer, Orientation, OutputCapture};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum RecordingError {
    Capture(CaptureError),
    Io(io::Error),
    Png(png::EncodingError),
}

impl std::fmt::Display for RecordingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capture(e) => write!(f, "capture failed: {e}"),
            Self::Io(e) => write!(f, "i/o failed: {e}"),
            Self::Png(e) => write!(f, "png encoding failed: {e}"),
        }
    }
}

impl std::error::Error for RecordingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capture(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Png(e) => Some(e),
        }
    }
}

impl From<CaptureError> for RecordingError {
    fn from(e: CaptureError) -> Self {
        Self::Capture(e)
    }
}

impl From<io::Error> for RecordingError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<png::EncodingError> for RecordingError {
    fn from(e: png::EncodingError) -> Self {
        Self::Png(e)
    }
}

// ---------------------------------------------------------------------------
// VideoEncoder — thin ffmpeg subprocess wrapper
// ---------------------------------------------------------------------------

/// Owns an ffmpeg child process receiving `Fourcc::Abgr8888` raw frames
/// (bottom-up) on its stdin. The encoder's own `-vf vflip` turns them
/// into the top-down orientation mp4 containers expect, so we don't
/// touch the CPU image buffer at all.
///
/// # Threading model
///
/// `push_frame` is called from the render loop and **must not block** —
/// a synchronous `write_all` into ffmpeg's stdin would stall every
/// compositor tick on libx264's encode latency, which is precisely what
/// makes an otherwise-idle system feel stuck during recording. Instead,
/// frames are handed to a dedicated writer thread via a bounded
/// channel; a full channel means ffmpeg is falling behind and we drop
/// the newest frame (counted in [`Self::dropped_frames`]) rather than
/// grow memory unboundedly. Backpressure ends up being "rendered
/// video has slightly lower fps than the render loop" — acceptable,
/// whereas "compositor freezes" is not.
///
/// Currently hard-codes `libx264 -preset ultrafast -pix_fmt yuv420p`.
/// Encoder options become fields on this struct when configurability
/// actually matters.
struct VideoEncoder {
    /// `Some(..)` until `finish()` consumes it; `Drop` uses it to kill +
    /// wait any still-running ffmpeg, so an emskin crash / SIGTERM while
    /// recording doesn't leak ffmpeg as an orphan.
    child: Option<Child>,
    /// `Some(tx)` while active; `None` after `finish()` or `Drop` closes it.
    sender: Option<mpsc::SyncSender<Vec<u8>>>,
    /// Joined on shutdown so we wait for ffmpeg to flush queued frames.
    writer_thread: Option<JoinHandle<()>>,
    size: Size<i32, Physical>,
    fps: u32,
    bytes_per_frame: usize,
    dropped_frames: Arc<AtomicU64>,
}

/// How many frames can be queued for the writer thread before `push_frame`
/// starts dropping. Small so backpressure is felt as fps degradation, not
/// latency: a queue depth of 3 at 1920×1080 BGRA ≈ 24 MB, tolerable.
const WRITER_QUEUE_DEPTH: usize = 3;

impl VideoEncoder {
    fn spawn(path: &Path, size: Size<i32, Physical>, fps: u32) -> io::Result<Self> {
        if size.w <= 0 || size.h <= 0 {
            return Err(io::Error::other(format!(
                "video encoder rejects non-positive size {:?}",
                size
            )));
        }
        if fps == 0 {
            return Err(io::Error::other("video encoder rejects fps=0"));
        }
        ensure_parent_dir(path)?;

        let video_size = format!("{}x{}", size.w, size.h);
        let fps_str = fps.to_string();
        // `pad=ceil(iw/2)*2:ceil(ih/2)*2` forces even width/height, which
        // libx264 + yuv420p require (chroma is 2:2 subsampled). When
        // `size` is already even this is a no-op; when odd, it appends a
        // 1px black strip on the right/bottom rather than crop or scale,
        // so no content is lost and no resampling is introduced.
        let mut child = Command::new("ffmpeg")
            .args([
                "-y",
                "-loglevel",
                "warning",
                "-f",
                "rawvideo",
                "-pixel_format",
                "rgba",
                "-video_size",
                &video_size,
                "-framerate",
                &fps_str,
                "-i",
                "pipe:0",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-vf",
                "vflip,pad=ceil(iw/2)*2:ceil(ih/2)*2",
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg spawned without stdin"))?;

        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(WRITER_QUEUE_DEPTH);
        let dropped_frames = Arc::new(AtomicU64::new(0));

        let writer_thread = thread::Builder::new()
            .name("emskin-video-encoder".into())
            .spawn(move || {
                // Keep pulling until the sender is dropped (finish()).
                // On write failure, log once and drain so send calls
                // complete quickly rather than block render loop retries.
                let mut failed = false;
                while let Ok(bytes) = rx.recv() {
                    if failed {
                        continue;
                    }
                    if let Err(e) = stdin.write_all(&bytes) {
                        tracing::warn!("recorder: ffmpeg stdin write failed: {e}");
                        failed = true;
                    }
                }
                // `stdin` dropped here → ffmpeg sees EOF and starts flushing.
            })?;

        Ok(Self {
            child: Some(child),
            sender: Some(tx),
            writer_thread: Some(writer_thread),
            size,
            fps,
            bytes_per_frame: (size.w as usize) * (size.h as usize) * 4,
            dropped_frames,
        })
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Hand a frame to the writer thread. Returns `Ok` on both successful
    /// queueing and backpressure drops; only fails if the writer thread
    /// disconnected (ffmpeg died outright) or the byte count is wrong.
    /// That non-blocking behaviour is load-bearing — see the struct docs.
    fn push_frame(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() != self.bytes_per_frame {
            return Err(io::Error::other(format!(
                "frame byte count mismatch: got {}, expected {} ({}x{}x4)",
                bytes.len(),
                self.bytes_per_frame,
                self.size.w,
                self.size.h
            )));
        }
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| io::Error::other("encoder already closed"))?;
        match sender.try_send(bytes.to_vec()) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                let n = self.dropped_frames.fetch_add(1, Ordering::Relaxed) + 1;
                // Log only occasionally — every power of two — to avoid
                // log-spam swamping the very render loop we're trying to
                // keep responsive.
                if n.is_power_of_two() {
                    tracing::warn!("recorder: writer queue full, dropped {n} frame(s) total");
                }
                Ok(())
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(io::Error::other("encoder writer thread died"))
            }
        }
    }

    /// Graceful shutdown: close the channel (signals EOF to the writer
    /// thread → ffmpeg stdin EOF), then wait for both.
    ///
    /// Consumes `self`, so `Drop` afterwards only sees `None`-valued
    /// `child` / `sender` / `writer_thread` fields and skips its
    /// best-effort cleanup.
    fn finish(mut self) -> io::Result<ExitStatus> {
        drop(self.sender.take());
        if let Some(t) = self.writer_thread.take() {
            if t.join().is_err() {
                tracing::warn!("recorder: writer thread panicked");
            }
        }
        self.child
            .take()
            .ok_or_else(|| io::Error::other("encoder already finished"))?
            .wait()
    }
}

impl Drop for VideoEncoder {
    /// Best-effort cleanup when the encoder is dropped without an explicit
    /// `finish()` — for instance if `Recorder` is dropped mid-recording
    /// because the compositor is shutting down or panicking.
    ///
    /// Policy: close the stdin channel so the writer thread exits and
    /// ffmpeg sees EOF, then `kill` ffmpeg (rather than wait indefinitely
    /// for it to finalize a partial mp4 — we may be on a panic path). If
    /// `finish()` already ran, every `take()` here returns `None` and the
    /// impl is a no-op.
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(t) = self.writer_thread.take() {
            if t.join().is_err() {
                tracing::warn!("recorder: writer thread panicked (during drop)");
            }
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder")
            .field("size", &self.size)
            .field("fps", &self.fps)
            .field("bytes_per_frame", &self.bytes_per_frame)
            .field(
                "dropped_frames",
                &self.dropped_frames.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Recorder state machine
// ---------------------------------------------------------------------------

/// Stateful recorder. Holds at most one pending frame per render cycle.
#[derive(Debug)]
pub struct Recorder {
    state: RecorderState,
    capturer: Capturer,
}

#[derive(Debug)]
enum RecorderState {
    Idle,

    // --- screenshot path ---
    /// One-shot screenshot requested; awaiting next render tick.
    ScreenshotRequested {
        path: PathBuf,
    },
    /// PBO filled during `capture_frame`; `finalize` writes it as PNG.
    ScreenshotPending {
        frame: CapturedFrame,
        path: PathBuf,
    },

    // --- video path ---
    /// Video recording requested; next `capture_frame` spawns ffmpeg and
    /// captures the first frame.
    RecordingRequested {
        path: PathBuf,
        fps: u32,
    },
    /// Active recording, running an async-readback pipeline:
    ///
    /// - `capture_frame` (tick N) calls `copy_framebuffer` into a fresh
    ///   PBO and stores it in `just_captured`. No GPU sync is involved —
    ///   `copy_framebuffer` only queues the ReadPixels command.
    /// - `finalize` (tick N) then maps + pushes whatever is in `to_push`,
    ///   which was moved there at the *end* of tick N-1's finalize. By
    ///   now the GPU has had a full render tick to complete the ReadPixels
    ///   that populated that PBO, so `glMapBufferRange` doesn't stall.
    /// - At the end of `finalize` (tick N), `just_captured` rotates into
    ///   `to_push` and `just_captured` becomes `None`, arming the
    ///   pipeline for tick N+1.
    ///
    /// The first recorded frame shows up in the output with a one-tick
    /// latency (≈ 33 ms at 30 fps render rate), which is imperceptible.
    Recording {
        encoder: VideoEncoder,
        meta: RecordingMeta,
        /// Copied at least one tick ago; safe to map now.
        to_push: Option<CapturedFrame>,
        /// Copied this tick inside the bind block; rotates into `to_push`
        /// at end of `finalize`.
        just_captured: Option<CapturedFrame>,
    },
    /// Recording is winding down. `finalize` flushes `to_push` (if any),
    /// closes the encoder, emits a [`FinishEvent`], and returns to
    /// `Idle`. Mapping `to_push` here may incur one GPU sync because
    /// we're closing right away instead of waiting another tick — but
    /// this path only runs once per recording, so the stall is
    /// acceptable. `just_captured` is always `None` in this state
    /// because Stopping is only entered outside `capture_frame`.
    Stopping {
        encoder: VideoEncoder,
        meta: RecordingMeta,
        to_push: Option<CapturedFrame>,
        reason: FinishReason,
    },
}

/// Why a recording ended. Emitted alongside [`FinishEvent`] so hosts can
/// give the user an accurate explanation and decide whether to keep any
/// UI indicators lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinishReason {
    /// User called `stop_recording()` via IPC.
    User,
    /// Physical framebuffer size changed mid-recording. libx264 / the
    /// rawvideo pipe can't accept dynamic dimensions, so the recording
    /// was closed and the user will see a clean mp4 of what fit.
    Resize,
    /// `push_frame` failed (ffmpeg died / broken pipe) — salvage-and-close.
    EncoderError,
    /// A new recording request arrived while one was already active.
    Replaced,
}

impl FinishReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Resize => "resize",
            Self::EncoderError => "encoder_error",
            Self::Replaced => "replaced",
        }
    }
}

/// Summary of a finished recording, returned by [`Recorder::finalize`]
/// when the state machine completes a `Stopping` cycle. The host relays
/// this to the Emacs client so the elisp `emskin-record` variable can
/// flip back to `nil` even if the stop was auto-triggered.
#[derive(Debug, Clone)]
pub struct FinishEvent {
    pub path: PathBuf,
    pub frames_written: u64,
    pub duration: Duration,
    pub reason: FinishReason,
}

#[derive(Debug)]
struct RecordingMeta {
    path: PathBuf,
    /// Host-relative `Instant` of the first captured frame. Used for
    /// final-duration reporting (wall-clock, not video-time).
    started_at: Instant,
    /// `EffectCtx::present_time` at first capture — same clock the
    /// overlay renders in. Lets the host sync the red dot's timer
    /// without needing to reach into `Instant`-space.
    started_at_present: Duration,
    frame_interval: Duration,
    last_frame_at: Option<Instant>,
    frames_written: u64,
    size: Size<i32, Physical>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            state: RecorderState::Idle,
            capturer: Capturer::new(),
        }
    }

    // ------ external requests ------

    /// Queue a one-shot PNG screenshot for the next rendered frame.
    /// Supersedes any previously queued screenshot request.
    pub fn request_screenshot(&mut self, path: PathBuf) {
        tracing::info!("recorder: screenshot queued → {}", path.display());
        self.state = RecorderState::ScreenshotRequested { path };
    }

    /// Begin a video recording. If one is already running, it's moved to
    /// `Stopping { reason: Replaced }` so its encoder closes cleanly; the
    /// finish event surfaces through a later `finalize` call. Any PBO
    /// queued in the old recording is discarded — the replaced stream
    /// ends wherever the pipeline happened to be.
    pub fn request_recording(&mut self, path: PathBuf, fps: u32) {
        let state = std::mem::replace(&mut self.state, RecorderState::Idle);
        self.state = match state {
            RecorderState::Recording {
                encoder,
                meta,
                to_push: _,
                just_captured: _,
            } => {
                tracing::warn!("recorder: replacing active recording");
                RecorderState::Stopping {
                    encoder,
                    meta,
                    to_push: None,
                    reason: FinishReason::Replaced,
                }
            }
            RecorderState::Stopping { .. } => {
                // Already winding down a prior recording; queueing behind
                // it would need a second slot. In practice this path is
                // only hit if the user mashes the toggle during the ~1
                // render tick it takes Stopping to drain, so flag it and
                // drop the new request.
                tracing::warn!(
                    "recorder: ignoring recording request while previous \
                     recording is still closing (path={})",
                    path.display()
                );
                state
            }
            RecorderState::ScreenshotRequested { path: old_path } => {
                tracing::warn!(
                    "recorder: discarding pending screenshot ({}) for new recording",
                    old_path.display()
                );
                RecorderState::RecordingRequested { path, fps }
            }
            RecorderState::ScreenshotPending { path: old_path, .. } => {
                tracing::warn!(
                    "recorder: discarding captured-but-unwritten screenshot ({}) for new recording",
                    old_path.display()
                );
                RecorderState::RecordingRequested { path, fps }
            }
            RecorderState::RecordingRequested { path: old_path, .. } => {
                tracing::warn!(
                    "recorder: overwriting queued recording request ({}) with new request",
                    old_path.display()
                );
                RecorderState::RecordingRequested { path, fps }
            }
            RecorderState::Idle => {
                tracing::info!(
                    "recorder: recording queued → {} @ {}fps",
                    path.display(),
                    fps
                );
                RecorderState::RecordingRequested { path, fps }
            }
        };
    }

    /// Signal end-of-recording. Moves `Recording` → `Stopping { User }`;
    /// the next `finalize` call flushes the one-tick-old queued frame,
    /// closes ffmpeg, and returns a `FinishEvent`.
    pub fn stop_recording(&mut self) {
        let state = std::mem::replace(&mut self.state, RecorderState::Idle);
        self.state = match state {
            RecorderState::Recording {
                encoder,
                meta,
                to_push,
                just_captured: _,
            } => {
                tracing::info!(
                    "recorder: stop requested; {} frames so far",
                    meta.frames_written
                );
                // Discard just_captured: it was only copied this tick and
                // its ReadPixels likely hasn't finished on the GPU yet,
                // so mapping it would introduce the very stall we're
                // trying to avoid. One frame less at the tail of the
                // video is a cheap trade-off for responsive shutdown.
                RecorderState::Stopping {
                    encoder,
                    meta,
                    to_push,
                    reason: FinishReason::User,
                }
            }
            RecorderState::RecordingRequested { path, .. } => {
                tracing::info!(
                    "recorder: aborting pending recording request ({})",
                    path.display()
                );
                RecorderState::Idle
            }
            other => {
                if !matches!(other, RecorderState::Idle) {
                    tracing::debug!("recorder: stop_recording called in unexpected state");
                }
                other
            }
        };
    }

    // ------ per-frame driver ------

    /// Inside the bind block, after `render_workspace`. Fills a PBO if
    /// the state wants one this tick.
    pub fn capture_frame(
        &mut self,
        renderer: &mut GlesRenderer,
        framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
        output: &Output,
        physical_size: Size<i32, Physical>,
        present_time: Duration,
    ) {
        // Fast path: Idle is the common case whenever the user isn't
        // capturing. Skip the `Instant::now()` syscall + `mem::replace`
        // so the render loop pays nothing when recorder is dormant.
        if matches!(self.state, RecorderState::Idle) {
            return;
        }
        let now = Instant::now();
        let state = std::mem::replace(&mut self.state, RecorderState::Idle);

        self.state = match state {
            // Idle is ruled out above; these arms only run when we're
            // mid-pipeline and need to carry state forward.
            RecorderState::Idle
            | RecorderState::ScreenshotPending { .. }
            | RecorderState::Stopping { .. } => state,

            RecorderState::ScreenshotRequested { path } => {
                match self.try_capture(renderer, framebuffer, output, physical_size, present_time) {
                    Some(frame) => RecorderState::ScreenshotPending { frame, path },
                    None => RecorderState::Idle,
                }
            }

            RecorderState::RecordingRequested { path, fps } => {
                let encoder = match VideoEncoder::spawn(&path, physical_size, fps) {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!("recorder: ffmpeg spawn failed: {e}");
                        self.state = RecorderState::Idle;
                        return;
                    }
                };
                let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
                let meta = RecordingMeta {
                    path,
                    started_at: now,
                    started_at_present: present_time,
                    frame_interval,
                    last_frame_at: None,
                    frames_written: 0,
                    size: physical_size,
                };
                // Seed the pipeline: the first capture goes straight into
                // `just_captured`; `to_push` stays `None` because we have
                // no previous-tick PBO yet. The first visible frame of the
                // video lands one render tick after this.
                let just_captured =
                    self.try_capture(renderer, framebuffer, output, physical_size, present_time);
                RecorderState::Recording {
                    encoder,
                    meta,
                    to_push: None,
                    just_captured,
                }
            }

            RecorderState::Recording {
                encoder,
                meta,
                to_push,
                just_captured: _,
            } => {
                // `just_captured` should have been drained into `to_push`
                // by last tick's finalize; if it's still here the host
                // skipped finalize — drop it rather than leak.
                if physical_size != meta.size {
                    tracing::warn!(
                        "recorder: frame size changed {:?} → {:?}; stopping",
                        meta.size,
                        physical_size
                    );
                    // Preserve `to_push` so Stopping can flush the last
                    // correctly-sized frame.
                    self.state = RecorderState::Stopping {
                        encoder,
                        meta,
                        to_push,
                        reason: FinishReason::Resize,
                    };
                    return;
                }
                let due = match meta.last_frame_at {
                    None => true,
                    Some(last) => now.saturating_duration_since(last) >= meta.frame_interval,
                };
                let just_captured = if due {
                    self.try_capture(renderer, framebuffer, output, physical_size, present_time)
                } else {
                    None
                };
                RecorderState::Recording {
                    encoder,
                    meta,
                    to_push,
                    just_captured,
                }
            }
        };
    }

    /// After `backend.submit()`. Consumes any PBO captured this tick.
    ///
    /// Returns `Some(FinishEvent)` when a recording completed on this call
    /// (i.e. a `Stopping` state was drained). Callers use this to sync
    /// external UI/state (overlay, elisp `emskin-record` variable).
    #[must_use]
    pub fn finalize(&mut self, renderer: &mut GlesRenderer) -> Option<FinishEvent> {
        // Mirror `capture_frame`'s Idle fast-path — finalize runs every
        // render tick too.
        if matches!(self.state, RecorderState::Idle) {
            return None;
        }
        let now = Instant::now();
        let state = std::mem::replace(&mut self.state, RecorderState::Idle);

        let (next, finished) = match state {
            RecorderState::Idle
            | RecorderState::ScreenshotRequested { .. }
            | RecorderState::RecordingRequested { .. } => (state, None),

            RecorderState::ScreenshotPending { frame, path } => {
                match write_screenshot(&frame, &path, renderer) {
                    Ok(()) => tracing::info!("recorder: screenshot → {}", path.display()),
                    Err(e) => tracing::warn!("recorder: screenshot finalize failed: {e}"),
                }
                (RecorderState::Idle, None)
            }

            RecorderState::Recording {
                mut encoder,
                mut meta,
                to_push,
                just_captured,
            } => {
                // Push the PBO captured last tick: its ReadPixels has
                // had a full render tick to complete on the GPU, so
                // `map_texture` here is a memcpy rather than a stall.
                let push_result = if let Some(frame) = to_push {
                    match push_frame(&mut encoder, &frame, renderer) {
                        Ok(()) => {
                            meta.frames_written += 1;
                            meta.last_frame_at = Some(now);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    // First tick after recording start — nothing queued yet.
                    Ok(())
                };
                match push_result {
                    Ok(()) => (
                        // Rotate the pipeline: this tick's fresh capture
                        // becomes next tick's `to_push`.
                        RecorderState::Recording {
                            encoder,
                            meta,
                            to_push: just_captured,
                            just_captured: None,
                        },
                        None,
                    ),
                    Err(e) => {
                        tracing::warn!("recorder: push_frame failed: {e}; entering Stopping");
                        (
                            RecorderState::Stopping {
                                encoder,
                                meta,
                                to_push: None,
                                reason: FinishReason::EncoderError,
                            },
                            None,
                        )
                    }
                }
            }

            RecorderState::Stopping {
                mut encoder,
                mut meta,
                to_push,
                reason,
            } => {
                if let Some(frame) = to_push {
                    if let Err(e) = push_frame(&mut encoder, &frame, renderer) {
                        tracing::warn!("recorder: final push_frame failed: {e}");
                    } else {
                        meta.frames_written += 1;
                    }
                }
                let duration = meta.started_at.elapsed();
                let dropped = encoder.dropped_frames();
                match encoder.finish() {
                    Ok(status) => tracing::info!(
                        "recorder: video → {} ({} frames, {} dropped, {:.2}s, \
                         reason={}, exit {:?})",
                        meta.path.display(),
                        meta.frames_written,
                        dropped,
                        duration.as_secs_f64(),
                        reason.as_str(),
                        status.code()
                    ),
                    Err(e) => tracing::warn!(
                        "recorder: ffmpeg finish failed: {e} ({} frames, \
                         {} dropped, reason={})",
                        meta.frames_written,
                        dropped,
                        reason.as_str()
                    ),
                }
                let event = FinishEvent {
                    path: meta.path,
                    frames_written: meta.frames_written,
                    duration,
                    reason,
                };
                (RecorderState::Idle, Some(event))
            }
        };

        self.state = next;
        finished
    }

    /// The render loop should keep forcing frames while this returns
    /// true. Covers every non-Idle state — including single-shot
    /// screenshots (they still need one render tick to fire), not just
    /// video recording. The name is descriptive about the *use case*
    /// rather than the narrower "is there an active encoder" sense.
    pub fn wants_continuous_frames(&self) -> bool {
        !matches!(self.state, RecorderState::Idle)
    }

    /// Host-facing "show the recording indicator this tick?" query.
    ///
    /// Returns the overlay's clock origin (same domain as
    /// `EffectCtx::present_time` — i.e. `EmskinState::start_time.elapsed()`)
    /// when the recording machinery wants a red dot on screen, or `None`
    /// when the indicator should be hidden. The host calls this every
    /// render tick and pipes the result straight into
    /// `RecorderOverlay::set_active`.
    ///
    /// - `RecordingRequested`: overlay appears immediately using `now`
    ///   as the start time, even though ffmpeg won't actually spawn
    ///   until the next `capture_frame`. Prevents the visual "red dot
    ///   briefly missing" glitch between IPC dispatch and the first
    ///   render tick.
    /// - `Recording`: use the meta's cached start present-time.
    /// - Any other state (Idle, Screenshot*, Stopping): hide. Hiding in
    ///   Stopping reflects the user's intent the moment they hit stop,
    ///   even while ffmpeg is still flushing bytes in the background.
    pub fn overlay_started_at(&self, now: Duration) -> Option<Duration> {
        match &self.state {
            RecorderState::RecordingRequested { .. } => Some(now),
            RecorderState::Recording { meta, .. } => Some(meta.started_at_present),
            _ => None,
        }
    }

    // ------ internal helpers ------

    fn try_capture(
        &self,
        renderer: &mut GlesRenderer,
        framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
        output: &Output,
        physical_size: Size<i32, Physical>,
        present_time: Duration,
    ) -> Option<CapturedFrame> {
        let source = match OutputCapture::new(output, physical_size, present_time) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("recorder: source build failed: {e}");
                return None;
            }
        };
        match self.capturer.capture(renderer, framebuffer, &source) {
            Ok(frame) => Some(frame),
            Err(e) => {
                tracing::warn!("recorder: capture failed: {e}");
                None
            }
        }
    }
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

fn push_frame(
    encoder: &mut VideoEncoder,
    frame: &CapturedFrame,
    renderer: &mut GlesRenderer,
) -> Result<(), RecordingError> {
    let bytes = frame.map(renderer)?;
    encoder.push_frame(bytes)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// PNG sink
// ---------------------------------------------------------------------------

fn write_screenshot(
    frame: &CapturedFrame,
    path: &Path,
    renderer: &mut GlesRenderer,
) -> Result<(), RecordingError> {
    let width = frame.width();
    let height = frame.height();
    let orientation = frame.orientation();
    let bytes = frame.map(renderer)?;
    ensure_parent_dir(path)?;
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder.write_header()?;
    match orientation {
        Orientation::TopDown => {
            png_writer.write_image_data(bytes)?;
        }
        Orientation::BottomUp => {
            let stride = width as usize * 4;
            let mut flipped = vec![0u8; bytes.len()];
            for y in 0..(height as usize) {
                let src = &bytes[y * stride..(y + 1) * stride];
                let dst_y = height as usize - 1 - y;
                flipped[dst_y * stride..(dst_y + 1) * stride].copy_from_slice(src);
            }
            png_writer.write_image_data(&flipped)?;
        }
    }
    png_writer.finish()?;
    Ok(())
}

/// Create the parent directory of `path` if it isn't already there. No-op
/// when `path` has no parent (e.g. bare filename). Shared between the
/// PNG and ffmpeg sinks so both behave consistently under
/// `~/Videos/emskin/foo.mp4`-style paths where the dir might not exist.
fn ensure_parent_dir(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    Ok(())
}
