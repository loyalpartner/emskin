//! Recording indicator overlay — red dot + MM:SS timer in the top-right
//! of the canvas while `emskin-toggle-record' is on.
//!
//! The host (`emskin`) keeps a typed `Rc<RefCell<RecorderOverlay>>` and
//! calls [`RecorderOverlay::set_active`] when the IPC toggle flips. The
//! `Effect` impl only reads the stored state — never initiates
//! anything — so the indicator is strictly the UI view of the recording
//! state machine maintained in `emskin::recording::Recorder`.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                solid::SolidColorRenderElement,
                Id, Kind,
            },
            gles::GlesRenderer,
            utils::CommitCounter,
        },
    },
    utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use effect_core::{paint_buffer, CustomElement, Effect, EffectCtx};

use crate::bitmap_font::{draw_text, label_width, GLYPH_H};

// ---------------------------------------------------------------------------
// Appearance
// ---------------------------------------------------------------------------

/// Red dot color (linear RGBA).
const DOT_COLOR: [f32; 4] = [0.92, 0.22, 0.24, 1.00];
/// Dot diameter in logical pixels.
const DOT_DIAMETER: i32 = 12;
/// Gap between the dot and the timer label.
const DOT_TEXT_GAP: i32 = 8;
/// Margin from the canvas top/right edge.
const MARGIN: i32 = 16;

/// Timer label background (BGRA on little-endian).
const LABEL_BG: [u8; 4] = [30, 25, 25, 200];
/// Timer label foreground (BGRA on little-endian).
const LABEL_FG: [u8; 4] = [240, 240, 240, 255];
/// Internal padding around the timer text inside its label box.
const LABEL_PAD: i32 = 4;

// ---------------------------------------------------------------------------
// Overlay
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum OverlayState {
    Hidden,
    Active { started_at: Duration },
}

pub struct RecorderOverlay {
    state: OverlayState,
    dot_id: Id,
    dot_commit: CommitCounter,
    label_buf: MemoryRenderBuffer,
    label_size_buffer: Size<i32, Buffer>,
    /// Cached "current seconds" value that the label buffer was painted
    /// for; skip re-painting when the displayed seconds haven't advanced.
    last_seconds: Option<u64>,
}

impl RecorderOverlay {
    pub fn new() -> Self {
        Self {
            state: OverlayState::Hidden,
            dot_id: Id::new(),
            dot_commit: CommitCounter::default(),
            label_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            label_size_buffer: (0, 0).into(),
            last_seconds: None,
        }
    }

    /// Flip the indicator on/off.
    ///
    /// `Some(started_at)` — `started_at` is the moment recording began,
    /// expressed as [`EffectCtx::present_time`] would read at that
    /// instant (i.e. `EmskinState::start_time.elapsed()` on the host).
    /// `None` — hide the indicator; resets the cached seconds counter.
    ///
    /// Idempotent: returns without touching state when the requested
    /// value matches what's already stored. The host calls this every
    /// render tick with a value derived from the recorder, so skipping
    /// the no-op write keeps the render loop's Idle path free of any
    /// per-tick overlay bookkeeping.
    pub fn set_active(&mut self, started_at: Option<Duration>) {
        match (&self.state, started_at) {
            (OverlayState::Hidden, None) => return,
            (OverlayState::Active { started_at: cur }, Some(new)) if *cur == new => return,
            _ => {}
        }
        match started_at {
            Some(d) => {
                self.state = OverlayState::Active { started_at: d };
            }
            None => {
                self.state = OverlayState::Hidden;
                self.last_seconds = None;
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self.state, OverlayState::Active { .. })
    }
}

impl Default for RecorderOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Effect for RecorderOverlay {
    fn name(&self) -> &'static str {
        "recorder"
    }

    fn is_active(&self) -> bool {
        self.is_enabled()
    }

    fn chain_position(&self) -> u8 {
        90
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &EffectCtx,
    ) -> Vec<CustomElement<GlesRenderer>> {
        let started_at = match self.state {
            OverlayState::Hidden => return Vec::new(),
            OverlayState::Active { started_at } => started_at,
        };
        let elapsed = ctx.present_time.saturating_sub(started_at);
        let seconds = elapsed.as_secs();

        // Re-render the label when seconds rolls over.
        if self.last_seconds != Some(seconds) {
            self.last_seconds = Some(seconds);
            self.render_timer_label(seconds);
            self.dot_commit.increment();
        }

        let s = Scale::from(ctx.scale);
        // The label buffer is authored at buffer_scale=1, so its buffer
        // dimensions map 1:1 to logical pixels.
        let label_size_log: Size<i32, Logical> =
            (self.label_size_buffer.w, self.label_size_buffer.h).into();

        // Layout: right-align within canvas, dot then label horizontally,
        // vertically centered on the label box.
        let total_w = DOT_DIAMETER + DOT_TEXT_GAP + label_size_log.w;
        let right_x = ctx.canvas.loc.x + ctx.canvas.size.w - MARGIN;
        let top_y = ctx.canvas.loc.y + MARGIN;
        let dot_x = right_x - total_w;
        let label_x = dot_x + DOT_DIAMETER + DOT_TEXT_GAP;
        let row_h = DOT_DIAMETER.max(label_size_log.h);
        let dot_y = top_y + (row_h - DOT_DIAMETER) / 2;
        let label_y = top_y + (row_h - label_size_log.h) / 2;

        let dot_phys_origin: Point<i32, Physical> = Point::<i32, Logical>::from((dot_x, dot_y))
            .to_f64()
            .to_physical(s)
            .to_i32_round();
        let dot_phys_size: Size<i32, Physical> =
            Size::<i32, Logical>::from((DOT_DIAMETER, DOT_DIAMETER))
                .to_f64()
                .to_physical(s)
                .to_i32_round();

        let dot = SolidColorRenderElement::new(
            self.dot_id.clone(),
            Rectangle::new(dot_phys_origin, dot_phys_size),
            self.dot_commit,
            DOT_COLOR,
            Kind::Unspecified,
        );

        let label_phys = Point::<i32, Logical>::from((label_x, label_y))
            .to_f64()
            .to_physical(s);
        let label_elem = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            label_phys,
            &self.label_buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok();

        let mut out = Vec::with_capacity(2);
        out.push(CustomElement::Solid(dot));
        if let Some(lab) = label_elem {
            out.push(CustomElement::Label(lab));
        }
        out
    }

    fn post_paint(&mut self) -> bool {
        // Keep requesting frames while active so the timer advances even
        // when nothing else is damaging the scene.
        self.is_enabled()
    }
}

impl RecorderOverlay {
    fn render_timer_label(&mut self, seconds: u64) {
        let mm = seconds / 60;
        let ss = seconds % 60;
        let text = format!("{:02}:{:02}", mm, ss);
        let tw = label_width(&text);
        let buf_size: Size<i32, Buffer> = (LABEL_PAD * 2 + tw, LABEL_PAD * 2 + GLYPH_H).into();
        self.label_size_buffer = buf_size;

        paint_buffer(&mut self.label_buf, buf_size, |data| {
            data.chunks_exact_mut(4)
                .for_each(|chunk| chunk.copy_from_slice(&LABEL_BG));
            draw_text(
                data,
                buf_size,
                (LABEL_PAD, LABEL_PAD).into(),
                &text,
                &LABEL_FG,
            );
        });
    }
}
