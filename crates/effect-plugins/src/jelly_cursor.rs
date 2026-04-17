//! Jelly cursor — elastic text-cursor animation.
//!
//! Ported from the `jelly` style of manateelazycat/holo-layer's
//! `plugin/cursor_animation.py`. When Emacs's text caret moves, a filled
//! quadrilateral stretches from the previous rect to the new one over
//! `DURATION`, then collapses into the new rect.
//!
//! Caret rects arrive via IPC (`SetCursorRect`), computed by the elisp
//! client in `post-command-hook`. The compositor owns only the animation
//! timing and rendering.

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
    utils::{Buffer as SBuffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use effect_core::paint_buffer;

// ---------------------------------------------------------------------------
// Tuning
// ---------------------------------------------------------------------------

const DURATION: Duration = Duration::from_millis(200);
/// Default cursor color (BGRA) — Catppuccin Mocha sky #89dceb.
const DEFAULT_COLOR_SOLID: [u8; 4] = [0xeb, 0xdc, 0x89, 0xc8];
/// Alpha applied to all jelly colors (0..=255).
const COLOR_ALPHA: u8 = 0xc8;
/// Safety margin around the polygon bbox, in logical pixels. Prevents
/// rounding from clipping the edge at fractional scale.
const BBOX_PAD: i32 = 2;
const EPS: f64 = 1e-9;

// ---------------------------------------------------------------------------
// Animation state
// ---------------------------------------------------------------------------

type RectF = Rectangle<f64, Logical>;

enum AnimState {
    /// No caret known (either never reported or host told us to cancel).
    Idle,
    /// Last known caret rect; next `update` with a different rect starts
    /// an animation.
    Primed(RectF),
    /// Interpolating `from → to`.
    Animating {
        from: RectF,
        to: RectF,
        start: Duration,
    },
}

impl AnimState {
    fn last_rect(&self) -> Option<RectF> {
        match self {
            AnimState::Idle => None,
            AnimState::Primed(r) => Some(*r),
            AnimState::Animating { to, .. } => Some(*to),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

pub struct JellyCursor {
    enabled: bool,
    state: AnimState,
    buf: MemoryRenderBuffer,
    commit: CommitCounter,
    color_solid: [u8; 4],
    color_light: [u8; 4],
}

impl Default for JellyCursor {
    fn default() -> Self {
        Self::new()
    }
}

impl JellyCursor {
    pub fn new() -> Self {
        Self {
            enabled: true,
            state: AnimState::Idle,
            buf: MemoryRenderBuffer::new(Fourcc::Argb8888, (1, 1), 1, Transform::Normal, None),
            commit: CommitCounter::default(),
            color_solid: DEFAULT_COLOR_SOLID,
            color_light: lighter_bgra(DEFAULT_COLOR_SOLID),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.state = AnimState::Idle;
        }
    }

    /// Parse a CSS-style hex color (`#RRGGBB` or `#RRGGBBAA`) and use it as
    /// the jelly color. Silently ignores malformed input.
    pub fn set_color_hex(&mut self, hex: &str) {
        if let Some(bgra) = parse_hex_color(hex) {
            self.color_solid = bgra;
            self.color_light = lighter_bgra(bgra);
        }
    }

    /// Push the current caret rect (in canvas coordinates) into the
    /// animation state machine. `None` resets to Idle.
    pub fn update(&mut self, rect: Option<Rectangle<i32, Logical>>, now: Duration) {
        if !self.enabled {
            return;
        }
        let Some(r) = rect else {
            self.state = AnimState::Idle;
            return;
        };
        let target = rect_i32_to_f64(r);
        self.state = match self.state.last_rect() {
            None => AnimState::Primed(target),
            Some(last) if rects_equal(last, target) => return,
            Some(last) => AnimState::Animating {
                from: last,
                to: target,
                start: now,
            },
        };
    }

    /// Animating endpoints and [0, 1] progress, or None when not animating.
    fn progress(&self, now: Duration) -> Option<(f64, RectF, RectF)> {
        let AnimState::Animating { from, to, start } = &self.state else {
            return None;
        };
        let elapsed = now.saturating_sub(*start);
        if elapsed >= DURATION {
            return None;
        }
        Some((
            elapsed.as_secs_f64() / DURATION.as_secs_f64(),
            *from,
            *to,
        ))
    }
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl effect_core::Effect for JellyCursor {
    fn name(&self) -> &'static str {
        "jelly_cursor"
    }

    fn is_active(&self) -> bool {
        self.enabled
    }

    fn chain_position(&self) -> u8 {
        77
    }

    fn pre_paint(&mut self, ctx: &effect_core::EffectCtx) {
        // Transition Animating → Primed when elapsed exceeds DURATION.
        // Bumping commit makes the damage tracker repaint the cleared area.
        if let AnimState::Animating { to, start, .. } = &self.state {
            if ctx.present_time.saturating_sub(*start) >= DURATION {
                let to = *to;
                self.state = AnimState::Primed(to);
                self.commit.increment();
            }
        }
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        let Some((p, from, to)) = self.progress(ctx.present_time) else {
            return Vec::new();
        };

        let pts = jelly_polygon(from, to, p);
        let (min_x, min_y, max_x, max_y) = bounds(&pts);
        let bbox_x = min_x.floor() as i32 - BBOX_PAD;
        let bbox_y = min_y.floor() as i32 - BBOX_PAD;
        let bbox_w = (max_x.ceil() as i32 - bbox_x + BBOX_PAD).max(1);
        let bbox_h = (max_y.ceil() as i32 - bbox_y + BBOX_PAD).max(1);

        let origin = Point::<f64, Logical>::from((bbox_x as f64, bbox_y as f64));
        let pts_local: [Point<f64, Logical>; 4] =
            std::array::from_fn(|i| pts[i] - origin);
        let gradient = Gradient {
            from: rect_center(from) - origin,
            to: rect_center(to) - origin,
            c_start: self.color_light,
            c_end: self.color_solid,
        };

        let buf_size: Size<i32, SBuffer> = (bbox_w, bbox_h).into();
        paint_buffer(&mut self.buf, buf_size, |data| {
            fill_polygon_bgra(
                PixelBuffer {
                    data,
                    w: bbox_w,
                    h: bbox_h,
                },
                &pts_local,
                &gradient,
            );
        });
        self.commit.increment();

        let s: Scale<f64> = Scale::from(ctx.scale);
        let loc_phys: Point<f64, Physical> = origin.to_physical(s);
        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            loc_phys,
            &self.buf,
            None,
            None,
            None,
            Kind::Unspecified,
        )
        .ok()
        .map(|e| vec![effect_core::CustomElement::Label(e)])
        .unwrap_or_default()
    }

    fn post_paint(&mut self) -> bool {
        matches!(self.state, AnimState::Animating { .. })
    }
}

// ---------------------------------------------------------------------------
// Polygon geometry — direct port of cursor_animation.py's `jelly_polygon`
// ---------------------------------------------------------------------------

fn jelly_polygon(start: RectF, end: RectF, p: f64) -> [Point<f64, Logical>; 4] {
    let cs = start.loc;
    let ce = end.loc;
    let (ws, hs) = (start.size.w, start.size.h);
    let (we, he) = (end.size.w, end.size.h);
    let dx = cs.x - ce.x;
    let dy = cs.y - ce.y;

    // Four base corners per motion quadrant — keeps the polygon convex and
    // non-self-intersecting regardless of direction.
    let mut pts = if dx * dy > 0.0 {
        [cs, cs + Point::from((ws, hs)), ce + Point::from((we, he)), ce]
    } else if dx * dy < 0.0 {
        [
            cs + Point::from((0.0, hs)),
            cs + Point::from((ws, 0.0)),
            ce + Point::from((we, 0.0)),
            ce + Point::from((0.0, he)),
        ]
    } else if dx.abs() < EPS {
        if dy >= 0.0 {
            [
                cs + Point::from((0.0, hs)),
                cs + Point::from((ws, hs)),
                ce + Point::from((we, 0.0)),
                ce,
            ]
        } else {
            [
                cs,
                cs + Point::from((ws, 0.0)),
                ce + Point::from((we, he)),
                ce + Point::from((0.0, he)),
            ]
        }
    } else if dx >= 0.0 {
        [
            cs + Point::from((ws, 0.0)),
            cs + Point::from((ws, hs)),
            ce + Point::from((0.0, he)),
            ce,
        ]
    } else {
        [
            cs,
            cs + Point::from((0.0, hs)),
            ce + Point::from((we, he)),
            ce + Point::from((we, 0.0)),
        ]
    };

    // Two-phase deformation around p = 0.5: leading edge slides out, then
    // trailing edge collapses onto it.
    if p < 0.5 {
        let k = p * 2.0;
        pts[2] = lerp(pts[1], pts[2], k);
        pts[3] = lerp(pts[0], pts[3], k);
    } else {
        let k = (p - 0.5) * 2.0;
        pts[0] = lerp(pts[0], pts[3], k);
        pts[1] = lerp(pts[1], pts[2], k);
    }
    pts
}

fn lerp(a: Point<f64, Logical>, b: Point<f64, Logical>, t: f64) -> Point<f64, Logical> {
    Point::from((a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t))
}

fn rect_center(r: RectF) -> Point<f64, Logical> {
    Point::from((r.loc.x + r.size.w * 0.5, r.loc.y + r.size.h * 0.5))
}

fn rect_i32_to_f64(r: Rectangle<i32, Logical>) -> RectF {
    Rectangle::new(
        Point::from((r.loc.x as f64, r.loc.y as f64)),
        Size::from((r.size.w as f64, r.size.h as f64)),
    )
}

fn rects_equal(a: RectF, b: RectF) -> bool {
    const E: f64 = 0.5;
    (a.loc.x - b.loc.x).abs() < E
        && (a.loc.y - b.loc.y).abs() < E
        && (a.size.w - b.size.w).abs() < E
        && (a.size.h - b.size.h).abs() < E
}

/// Single-pass x/y min/max over 4 points.
fn bounds(pts: &[Point<f64, Logical>; 4]) -> (f64, f64, f64, f64) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in pts {
        if p.x < min_x {
            min_x = p.x;
        }
        if p.x > max_x {
            max_x = p.x;
        }
        if p.y < min_y {
            min_y = p.y;
        }
        if p.y > max_y {
            max_y = p.y;
        }
    }
    (min_x, min_y, max_x, max_y)
}

// ---------------------------------------------------------------------------
// Polygon rasterization
// ---------------------------------------------------------------------------

struct PixelBuffer<'a> {
    data: &'a mut [u8],
    w: i32,
    h: i32,
}

struct Gradient {
    from: Point<f64, Logical>,
    to: Point<f64, Logical>,
    c_start: [u8; 4],
    c_end: [u8; 4],
}

/// Scanline-fill a convex quad into a BGRA buffer with a linear gradient.
///
/// Pixels outside the polygon are cleared to transparent in the same pass
/// (merging clear + fill saves a full-buffer zeroing sweep).
fn fill_polygon_bgra(
    buf: PixelBuffer<'_>,
    pts: &[Point<f64, Logical>; 4],
    grad: &Gradient,
) {
    let PixelBuffer { data, w: buf_w, h: buf_h } = buf;
    let stride = buf_w * 4;

    let (_, min_yf, _, max_yf) = bounds(pts);
    let poly_min_y = (min_yf.floor() as i32).max(0);
    let poly_max_y = (max_yf.ceil() as i32).min(buf_h - 1);

    let gx = grad.to.x - grad.from.x;
    let gy = grad.to.y - grad.from.y;
    let g_len2 = gx * gx + gy * gy;
    let has_grad = g_len2 > EPS;
    let inv_len2 = if has_grad { 1.0 / g_len2 } else { 0.0 };
    let dt = gx * inv_len2;

    for py in 0..buf_h {
        let row_off = (py * stride) as usize;
        // Rows outside polygon bbox are fully transparent.
        if py < poly_min_y || py > poly_max_y {
            data[row_off..row_off + stride as usize].fill(0);
            continue;
        }

        // Scan at pixel centre so a horizontal edge (two verts at exact
        // integer y) doesn't emit spurious intersections.
        let y = py as f64 + 0.5;
        let mut x_lo = f64::NAN;
        let mut x_hi = f64::NAN;
        for i in 0..4 {
            let a = pts[i];
            let b = pts[(i + 1) % 4];
            let (lo, hi) = if a.y <= b.y { (a, b) } else { (b, a) };
            // Half-open [lo.y, hi.y) avoids double-counting at shared verts.
            if y < lo.y || y >= hi.y {
                continue;
            }
            let dy = hi.y - lo.y;
            if dy.abs() < EPS {
                continue;
            }
            let x = lo.x + (y - lo.y) / dy * (hi.x - lo.x);
            if x_lo.is_nan() {
                x_lo = x;
            } else {
                x_hi = x;
            }
        }
        if x_lo.is_nan() || x_hi.is_nan() {
            data[row_off..row_off + stride as usize].fill(0);
            continue;
        }
        if x_lo > x_hi {
            std::mem::swap(&mut x_lo, &mut x_hi);
        }
        let x0 = (x_lo.ceil() as i32).clamp(0, buf_w);
        let x1 = ((x_hi - EPS).floor() as i32 + 1).clamp(0, buf_w);
        if x1 <= x0 {
            data[row_off..row_off + stride as usize].fill(0);
            continue;
        }

        // Left gap → transparent.
        if x0 > 0 {
            data[row_off..row_off + (x0 * 4) as usize].fill(0);
        }

        // Interior → gradient; recurrence `t += dt` replaces per-pixel
        // dot product.
        let mut t = if has_grad {
            ((x0 as f64 + 0.5 - grad.from.x) * gx + (y - grad.from.y) * gy) * inv_len2
        } else {
            1.0
        };
        let mut off = row_off + (x0 * 4) as usize;
        for _ in x0..x1 {
            let tc = t.clamp(0.0, 1.0) as f32;
            data[off] = mix(grad.c_start[0], grad.c_end[0], tc);
            data[off + 1] = mix(grad.c_start[1], grad.c_end[1], tc);
            data[off + 2] = mix(grad.c_start[2], grad.c_end[2], tc);
            data[off + 3] = mix(grad.c_start[3], grad.c_end[3], tc);
            off += 4;
            t += dt;
        }

        // Right gap → transparent.
        if x1 < buf_w {
            data[row_off + (x1 * 4) as usize..row_off + stride as usize].fill(0);
        }
    }
}

fn mix(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 * (1.0 - t) + b as f32 * t).round() as u8
}

// ---------------------------------------------------------------------------
// Color helpers
// ---------------------------------------------------------------------------

/// Parse `#RRGGBB` or `#RRGGBBAA` (case-insensitive, `#` optional) as BGRA.
fn parse_hex_color(s: &str) -> Option<[u8; 4]> {
    let s = s.trim().trim_start_matches('#');
    let (r, g, b, a) = match s.len() {
        6 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            COLOR_ALPHA,
        ),
        8 => (
            u8::from_str_radix(&s[0..2], 16).ok()?,
            u8::from_str_radix(&s[2..4], 16).ok()?,
            u8::from_str_radix(&s[4..6], 16).ok()?,
            u8::from_str_radix(&s[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some([b, g, r, a])
}

/// Qt's `QColor::lighter(150)` approximation — half-way to opaque white.
fn lighter_bgra(c: [u8; 4]) -> [u8; 4] {
    let mix_white = |v: u8| ((v as u16 + 255) / 2) as u8;
    [mix_white(c[0]), mix_white(c[1]), mix_white(c[2]), c[3]]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> RectF {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn polygon_at_p0_matches_start_rect_corners() {
        let start = rect(0.0, 0.0, 8.0, 16.0);
        let end = rect(20.0, 0.0, 8.0, 16.0);
        let pts = jelly_polygon(start, end, 0.0);
        assert!((pts[0].x - 0.0).abs() < 1e-6);
        assert!((pts[1].x - 0.0).abs() < 1e-6);
    }

    #[test]
    fn polygon_at_p1_collapses_to_end_rect() {
        let start = rect(0.0, 0.0, 8.0, 16.0);
        let end = rect(20.0, 0.0, 8.0, 16.0);
        let pts = jelly_polygon(start, end, 1.0);
        let on_end = |p: Point<f64, Logical>| {
            p.x >= end.loc.x - 0.5
                && p.x <= end.loc.x + end.size.w + 0.5
                && p.y >= end.loc.y - 0.5
                && p.y <= end.loc.y + end.size.h + 0.5
        };
        assert!(pts.iter().all(|&p| on_end(p)), "pts = {:?}", pts);
    }

    #[test]
    fn fill_polygon_writes_interior_pixels() {
        let mut data = vec![0u8; 16 * 16 * 4];
        let pts: [Point<f64, Logical>; 4] = [
            Point::from((2.0, 2.0)),
            Point::from((14.0, 2.0)),
            Point::from((14.0, 14.0)),
            Point::from((2.0, 14.0)),
        ];
        let grad = Gradient {
            from: Point::from((2.0, 8.0)),
            to: Point::from((14.0, 8.0)),
            c_start: [0xff, 0, 0, 0xff],
            c_end: [0, 0, 0xff, 0xff],
        };
        fill_polygon_bgra(
            PixelBuffer { data: &mut data, w: 16, h: 16 },
            &pts,
            &grad,
        );
        let center = (8 * 16 + 8) * 4;
        assert!(data[center + 3] > 0, "interior pixel not painted");
        // Outside bbox pixels cleared by the merged pass.
        assert_eq!(data[0..4], [0, 0, 0, 0]);
        let bottom_left = (15 * 16) * 4;
        assert_eq!(data[bottom_left..bottom_left + 4], [0, 0, 0, 0]);
    }

    #[test]
    fn parses_hex_colors() {
        assert_eq!(parse_hex_color("#cba6f7"), Some([0xf7, 0xa6, 0xcb, COLOR_ALPHA]));
        assert_eq!(parse_hex_color("cba6f7"), Some([0xf7, 0xa6, 0xcb, COLOR_ALPHA]));
        assert_eq!(parse_hex_color("#cba6f780"), Some([0xf7, 0xa6, 0xcb, 0x80]));
        assert_eq!(parse_hex_color("not-a-color"), None);
        assert_eq!(parse_hex_color("#abc"), None);
    }

    #[test]
    fn lighter_moves_halfway_to_white() {
        let lit = lighter_bgra([0, 0, 0, 0xff]);
        assert_eq!(lit[0], 127);
        assert_eq!(lit[3], 0xff);
    }

    #[test]
    fn set_color_hex_updates_solid_and_light() {
        let mut jc = JellyCursor::new();
        jc.set_color_hex("#000000");
        assert_eq!(jc.color_solid, [0, 0, 0, COLOR_ALPHA]);
        assert_eq!(jc.color_light, lighter_bgra([0, 0, 0, COLOR_ALPHA]));
    }

    #[test]
    fn update_with_none_sets_idle() {
        let mut jc = JellyCursor::new();
        jc.update(Some(Rectangle::new((10, 10).into(), (8, 16).into())), Duration::ZERO);
        assert!(matches!(jc.state, AnimState::Primed(_)));
        jc.update(None, Duration::from_millis(50));
        assert!(matches!(jc.state, AnimState::Idle));
        // Re-entry after Idle: no animation.
        jc.update(
            Some(Rectangle::new((30, 30).into(), (8, 16).into())),
            Duration::from_millis(100),
        );
        assert!(matches!(jc.state, AnimState::Primed(_)));
    }

    #[test]
    fn update_with_changed_rect_seeds_animation() {
        let mut jc = JellyCursor::new();
        jc.update(
            Some(Rectangle::new((0, 0).into(), (8, 16).into())),
            Duration::ZERO,
        );
        jc.update(
            Some(Rectangle::new((20, 0).into(), (8, 16).into())),
            Duration::from_millis(10),
        );
        assert!(matches!(jc.state, AnimState::Animating { .. }));
    }

    #[test]
    fn update_mid_flight_retargets_without_stalling() {
        let mut jc = JellyCursor::new();
        jc.update(Some(Rectangle::new((0, 0).into(), (8, 16).into())), Duration::ZERO);
        jc.update(
            Some(Rectangle::new((20, 0).into(), (8, 16).into())),
            Duration::from_millis(10),
        );
        jc.update(
            Some(Rectangle::new((40, 0).into(), (8, 16).into())),
            Duration::from_millis(15),
        );
        match jc.state {
            AnimState::Animating { from, to, .. } => {
                assert!((from.loc.x - 20.0).abs() < 1e-6, "from = {:?}", from);
                assert!((to.loc.x - 40.0).abs() < 1e-6, "to = {:?}", to);
            }
            _ => panic!("expected Animating"),
        }
    }
}
