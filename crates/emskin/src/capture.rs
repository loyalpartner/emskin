//! Output framebuffer readback for the screencast pipeline.
//!
//! Sits **above** smithay's `ExportMem` trait and **below** any concrete
//! frame sink (ffmpeg stdin, wl_buffer writeback, disk dump). Call
//! [`Capturer::capture`] between `render_workspace` and `backend.submit`
//! to snapshot the composited framebuffer.
//!
//! # Layers
//!
//! - [`CaptureSource`]: descriptor for "what to read this frame" — a
//!   buffer-coordinate region plus a present timestamp. [`OutputCapture`]
//!   is the first implementor; toplevel and arbitrary-rect sources
//!   (milestone B) plug in by implementing the same trait.
//! - [`Capturer`]: stateful engine that owns cross-frame knowledge
//!   (pixel format, readback orientation). Holding it in the render
//!   loop keeps per-frame call sites a single method call.
//! - [`CapturedFrame`]: an owned GPU-side mapping handle. [`Self::map`]
//!   reborrows the renderer to expose a CPU byte slice; sinks that only
//!   need format/size metadata can skip mapping entirely.
//!
//! # Coordinate conventions
//!
//! `glReadPixels` returns pixel rows bottom-up. Rather than silently
//! flipping inside this module, frames carry [`Orientation`] so the sink
//! chooses — ffmpeg prepends `-vf vflip`, a PNG dumper flips in place,
//! a future Blit sink into a wl_buffer can just re-render upright.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            gles::{GlesError, GlesMapping, GlesRenderer},
            ExportMem, RendererSuper,
        },
    },
    output::Output,
    utils::{Buffer, Physical, Rectangle, Size, Transform},
};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum CaptureError {
    NonPositiveSize(Size<i32, Physical>),
    Gles(GlesError),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonPositiveSize(s) => {
                write!(f, "capture size is non-positive: {:?}", s)
            }
            Self::Gles(e) => write!(f, "gles readback failed: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Gles(e) => Some(e),
            _ => None,
        }
    }
}

impl From<GlesError> for CaptureError {
    fn from(e: GlesError) -> Self {
        Self::Gles(e)
    }
}

// ---------------------------------------------------------------------------
// Orientation
// ---------------------------------------------------------------------------

/// Vertical origin of a captured frame's pixel rows.
///
/// `glReadPixels` is [`Self::BottomUp`]; video encoders and PNG expect
/// [`Self::TopDown`]. The sink decides whether to flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    TopDown,
    BottomUp,
}

// ---------------------------------------------------------------------------
// CaptureSource
// ---------------------------------------------------------------------------

/// A thing whose composited contents can be snapshotted this frame.
///
/// Implementors are purely descriptive — they tell the [`Capturer`] *what*
/// region to read and *when*. They do no GPU work. Construction is where
/// validation happens (e.g. [`OutputCapture::new`] rejects outputs with no
/// mode), so the trait methods are infallible.
pub trait CaptureSource {
    /// Buffer-coordinate rectangle to read back. Must be inside the bound
    /// framebuffer's extent — enforced by the source's constructor.
    fn region(&self) -> Rectangle<i32, Buffer>;

    /// Present timestamp carried along with the captured frame. Used by
    /// sinks for fps throttling and A/V sync.
    fn present_time(&self) -> Duration;
}

/// Capture the full physical extent of an [`Output`]'s currently bound
/// framebuffer.
///
/// # Why `physical_size` is a parameter
///
/// `output.current_mode().size` can lag behind the backend's real
/// framebuffer during fractional-scale resize sequences — winit updates
/// its EGL surface immediately but the output's mode is re-synced on the
/// next render tick. Passing the size explicitly (e.g. from
/// `WinitGraphicsBackend::window_size` on the winit backend, or the CRTC
/// mode for udev/kms) eliminates this drift. The source itself doesn't
/// know which backend is active, so it trusts the caller.
#[derive(Debug, Clone)]
pub struct OutputCapture {
    output: Output,
    region: Rectangle<i32, Buffer>,
    present_time: Duration,
}

impl OutputCapture {
    pub fn new(
        output: &Output,
        physical_size: Size<i32, Physical>,
        present_time: Duration,
    ) -> Result<Self, CaptureError> {
        if physical_size.w <= 0 || physical_size.h <= 0 {
            return Err(CaptureError::NonPositiveSize(physical_size));
        }
        let region =
            Rectangle::from_size(physical_size.to_logical(1).to_buffer(1, Transform::Normal));
        Ok(Self {
            output: output.clone(),
            region,
            present_time,
        })
    }

    pub fn output(&self) -> &Output {
        &self.output
    }
}

impl CaptureSource for OutputCapture {
    fn region(&self) -> Rectangle<i32, Buffer> {
        self.region
    }

    fn present_time(&self) -> Duration {
        self.present_time
    }
}

// ---------------------------------------------------------------------------
// Capturer
// ---------------------------------------------------------------------------

/// Stateful readback engine. Holds cross-frame knowledge so per-frame call
/// sites stay a single line. Instantiate once in `EmskinState::new` and
/// reuse every frame.
#[derive(Debug, Clone)]
pub struct Capturer {
    format: Fourcc,
    orientation: Orientation,
}

impl Capturer {
    /// Default capturer: `Abgr8888` (RGBA byte order on little-endian) in
    /// native `BottomUp` orientation.
    ///
    /// PNG and most `ffmpeg -pixel_format rgba` paths consume RGBA directly
    /// with zero channel swapping. Sinks that want BGRA (e.g. a future
    /// `wl_buffer` writeback for the `ext-image-copy-capture-v1` protocol)
    /// can build with [`Self::with_format`] passing `Fourcc::Argb8888`.
    pub fn new() -> Self {
        Self {
            format: Fourcc::Abgr8888,
            orientation: Orientation::BottomUp,
        }
    }

    /// Override the capture pixel format. The chosen format must be
    /// supported by [`GlesRenderer::copy_framebuffer`] on the host driver;
    /// unsupported formats fail at [`Self::capture`] time with
    /// [`CaptureError::Gles`].
    pub fn with_format(mut self, format: Fourcc) -> Self {
        self.format = format;
        self
    }

    pub fn format(&self) -> Fourcc {
        self.format
    }

    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    /// Snapshot `source` from the already-composited `framebuffer`.
    pub fn capture<S: CaptureSource>(
        &self,
        renderer: &mut GlesRenderer,
        framebuffer: &<GlesRenderer as RendererSuper>::Framebuffer<'_>,
        source: &S,
    ) -> Result<CapturedFrame, CaptureError> {
        let region = source.region();
        let mapping = renderer.copy_framebuffer(framebuffer, region, self.format)?;
        Ok(CapturedFrame {
            mapping,
            size: region.size,
            format: self.format,
            orientation: self.orientation,
            present_time: source.present_time(),
        })
    }
}

impl Default for Capturer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// CapturedFrame
// ---------------------------------------------------------------------------

/// Owned GPU-side readback mapping. Metadata (size, format, orientation,
/// timestamp) is available without touching the GPU; call [`Self::map`]
/// to borrow the CPU byte slice.
pub struct CapturedFrame {
    mapping: GlesMapping,
    size: Size<i32, Buffer>,
    format: Fourcc,
    orientation: Orientation,
    present_time: Duration,
}

impl CapturedFrame {
    pub fn size(&self) -> Size<i32, Buffer> {
        self.size
    }

    pub fn width(&self) -> u32 {
        self.size.w as u32
    }

    pub fn height(&self) -> u32 {
        self.size.h as u32
    }

    /// Bytes per row. Correct for any 32-bit-per-pixel format (Argb/Abgr/
    /// Xrgb/Xbgr 8888); smithay uses tightly packed PBOs with no alignment
    /// padding. Narrower formats would need their bytes-per-pixel tracked
    /// alongside `format` — none are used yet.
    pub fn stride(&self) -> u32 {
        self.size.w as u32 * 4
    }

    pub fn format(&self) -> Fourcc {
        self.format
    }

    pub fn orientation(&self) -> Orientation {
        self.orientation
    }

    pub fn present_time(&self) -> Duration {
        self.present_time
    }

    /// Borrow the CPU byte slice. The slice lives until `self` is dropped;
    /// row order matches [`Self::orientation`].
    ///
    /// # Backend note (winit / EGL surface)
    ///
    /// `map_texture` internally `make_current`s the EGL context *without* a
    /// draw surface, detaching it from whichever EGL surface was bound. On
    /// the winit backend this breaks `WinitGraphicsBackend::submit` —
    /// `eglSwapBuffers` requires the winit surface to still be current.
    ///
    /// Call sequence on winit backend must be:
    ///
    /// ```text
    /// let (renderer, fb) = backend.bind()?;
    /// render_workspace(renderer, &mut fb, ...)?;
    /// let frame = capturer.capture(renderer, &fb, &source)?;   // fills PBO
    /// drop(fb);
    /// backend.submit(...)?;                                    // swap first
    /// let bytes = frame.map(backend.renderer_mut())?;          // then map
    /// ```
    ///
    /// On udev / offscreen-FBO backends the ordering is irrelevant, but
    /// "capture first, submit, then map" is always safe.
    pub fn map<'a>(&'a self, renderer: &'a mut GlesRenderer) -> Result<&'a [u8], CaptureError> {
        Ok(renderer.map_texture(&self.mapping)?)
    }
}

impl std::fmt::Debug for CapturedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedFrame")
            .field("size", &self.size)
            .field("format", &self.format)
            .field("orientation", &self.orientation)
            .field("present_time", &self.present_time)
            .finish_non_exhaustive()
    }
}
