//! Cursor trail — elastic trailing animation behind the pointer.
//!
//! A chain of "nodes" follows the cursor with spring-damper physics. Each
//! node trails the one in front, producing an elastic tail. Fast cursor
//! movement stretches the chain; when the cursor stops the nodes bounce
//! back and settle.
//!
//! Visual: each node is a pre-rendered soft circle (anti-aliased, once at
//! init), placed with decreasing size and opacity toward the tail.

use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                Kind,
            },
            gles::GlesRenderer,
            utils::CommitCounter,
        },
    },
    utils::{Buffer, Logical, Physical, Point, Scale, Size, Transform},
};

use effect_core::paint_buffer;

// ---------------------------------------------------------------------------
// Physics tuning
// ---------------------------------------------------------------------------

const NODE_COUNT: usize = 10;
/// Spring stiffness — higher = snappier return.
const STIFFNESS: f64 = 120.0;
/// Damping — higher = less oscillation. 12 keeps ~1 bounce before settling.
const DAMPING: f64 = 12.0;
/// Maximum delta-time per physics step (prevents explosion on frame hitches).
const MAX_DT: f64 = 1.0 / 30.0;

// ---------------------------------------------------------------------------
// Visual tuning
// ---------------------------------------------------------------------------

/// Radius of the largest (head) circle, in logical pixels.
const HEAD_RADIUS: i32 = 7;
/// Radius of the smallest (tail) circle.
const TAIL_RADIUS: i32 = 2;
/// Alpha of the head node.
const HEAD_ALPHA: f32 = 0.75;
/// Alpha of the tail node.
const TAIL_ALPHA: f32 = 0.12;
/// Circle color: vivid magenta-pink (#FF4DA6 → BGRA).
const CIRCLE_COLOR: [u8; 4] = [0xa6, 0x4d, 0xff, 0xff]; // BGRA

// ---------------------------------------------------------------------------
// Soft-circle texture
// ---------------------------------------------------------------------------

fn render_circle(radius: i32) -> (MemoryRenderBuffer, CommitCounter) {
    let diameter = radius * 2 + 1;
    let buf_size: Size<i32, Buffer> = (diameter, diameter).into();
    let mut buf = MemoryRenderBuffer::new(Fourcc::Argb8888, (1, 1), 1, Transform::Normal, None);
    let mut commit = CommitCounter::default();
    paint_buffer(&mut buf, buf_size, |data| {
        let cx = radius as f64;
        let cy = radius as f64;
        let r = radius as f64;
        for py in 0..diameter {
            for px in 0..diameter {
                let dx = px as f64 - cx;
                let dy = py as f64 - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                let a = if dist <= r - 1.0 {
                    1.0
                } else if dist <= r {
                    r - dist
                } else {
                    0.0
                };
                let off = ((py * diameter + px) * 4) as usize;
                let alpha = (a * CIRCLE_COLOR[3] as f64) as u8;
                data[off] = CIRCLE_COLOR[0];
                data[off + 1] = CIRCLE_COLOR[1];
                data[off + 2] = CIRCLE_COLOR[2];
                data[off + 3] = alpha;
            }
        }
    });
    commit.increment();
    (buf, commit)
}

// ---------------------------------------------------------------------------
// Node — one bead on the elastic chain
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Node {
    pos: Point<f64, Logical>,
    vel: Point<f64, Logical>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            pos: Point::from((0.0, 0.0)),
            vel: Point::from((0.0, 0.0)),
        }
    }
}

// ---------------------------------------------------------------------------
// CursorTrail
// ---------------------------------------------------------------------------

pub struct CursorTrail {
    enabled: bool,
    nodes: Vec<Node>,
    last_time: Option<Duration>,
    circle_bufs: Vec<(MemoryRenderBuffer, CommitCounter)>,
    settled: bool,
}

impl Default for CursorTrail {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorTrail {
    pub fn new() -> Self {
        let mut circle_bufs = Vec::with_capacity(NODE_COUNT);
        for i in 0..NODE_COUNT {
            let t = i as f64 / (NODE_COUNT - 1).max(1) as f64;
            let r = HEAD_RADIUS as f64 * (1.0 - t) + TAIL_RADIUS as f64 * t;
            circle_bufs.push(render_circle(r.round() as i32));
        }

        Self {
            enabled: false,
            nodes: vec![Node::default(); NODE_COUNT],
            last_time: None,
            circle_bufs,
            settled: true,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.last_time = None;
            self.settled = true;
        }
    }

    fn step_physics(&mut self, cursor: Point<f64, Logical>, dt: f64) {
        let dt = dt.min(MAX_DT);

        self.nodes[0].pos = cursor;
        self.nodes[0].vel = Point::from((0.0, 0.0));

        let mut any_moving = false;
        for i in 1..NODE_COUNT {
            let target = self.nodes[i - 1].pos;
            let dx = self.nodes[i].pos.x - target.x;
            let dy = self.nodes[i].pos.y - target.y;

            let ax = -STIFFNESS * dx - DAMPING * self.nodes[i].vel.x;
            let ay = -STIFFNESS * dy - DAMPING * self.nodes[i].vel.y;

            self.nodes[i].vel.x += ax * dt;
            self.nodes[i].vel.y += ay * dt;
            self.nodes[i].pos.x += self.nodes[i].vel.x * dt;
            self.nodes[i].pos.y += self.nodes[i].vel.y * dt;

            let speed = self.nodes[i].vel.x.abs() + self.nodes[i].vel.y.abs();
            let dist = dx.abs() + dy.abs();
            if speed > 0.1 || dist > 0.5 {
                any_moving = true;
            }
        }
        self.settled = !any_moving;
    }
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl effect_core::Effect for CursorTrail {
    fn name(&self) -> &'static str {
        "cursor_trail"
    }

    fn is_active(&self) -> bool {
        self.enabled
    }

    fn chain_position(&self) -> u8 {
        75
    }

    fn pre_paint(&mut self, ctx: &effect_core::EffectCtx) {
        let Some(cursor) = ctx.cursor_pos else {
            self.last_time = None;
            return;
        };

        let now = ctx.present_time;
        let dt = self
            .last_time
            .map(|prev| (now - prev).as_secs_f64())
            .unwrap_or(0.0);
        self.last_time = Some(now);

        if dt <= 0.0 {
            for node in &mut self.nodes {
                node.pos = cursor;
                node.vel = Point::from((0.0, 0.0));
            }
            self.settled = false;
            return;
        }

        self.step_physics(cursor, dt);
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        if ctx.cursor_pos.is_none() || self.settled {
            return Vec::new();
        }

        let s: Scale<f64> = Scale::from(ctx.scale);
        let mut out = Vec::with_capacity(NODE_COUNT);

        // Render tail → head so head elements are on top (earlier in vec = topmost).
        for i in (1..NODE_COUNT).rev() {
            let t = i as f64 / (NODE_COUNT - 1).max(1) as f64;
            let alpha = HEAD_ALPHA * (1.0 - t as f32) + TAIL_ALPHA * t as f32;
            let node = &self.nodes[i];
            let (ref buf, _commit) = self.circle_bufs[i];
            let r = HEAD_RADIUS as f64 * (1.0 - t) + TAIL_RADIUS as f64 * t;
            let loc = Point::<f64, Logical>::from((node.pos.x - r, node.pos.y - r));
            let phys_loc: Point<f64, Physical> = loc.to_physical(s);

            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                phys_loc,
                buf,
                Some(alpha),
                None,
                None,
                Kind::Unspecified,
            ) {
                out.push(effect_core::CustomElement::Label(elem));
            }
        }

        out
    }

    fn post_paint(&mut self) -> bool {
        !self.settled
    }
}
