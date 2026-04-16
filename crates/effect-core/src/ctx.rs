//! Frame context passed into every `Effect::pre_paint` / `paint` call.
//!
//! Only low-level primitives live here — no workspace semantics, no IPC, no
//! Emacs state. The host (emskin) delivers higher-level state to plugins
//! directly by calling their typed setters, not through this struct.

use std::time::Duration;

use smithay::utils::{Logical, Point, Size};

/// Per-frame read-only snapshot supplied to effects.
///
/// From `effect-core`'s perspective any window/workspace/connection info is
/// already fixed by the time this struct exists — `EmskinState` has frozen
/// the relevant state before invoking the render pipeline.
pub struct EffectCtx {
    pub cursor_pos: Option<Point<f64, Logical>>,
    pub output_size: Size<i32, Logical>,
    pub scale: f64,
    /// Monotonic time approximating when the frame will display. Borrowed
    /// from KWin's `presentTime` — lets animated effects stay correct even
    /// if a frame is delayed.
    pub present_time: Duration,
}
