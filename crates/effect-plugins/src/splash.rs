//! Splash screen — letter-by-letter reveal of "emskin", inspired by
//! Android TV boot animations.
//!
//! Each letter slides up with a bouncy ease-out-back and fades in on a
//! staggered delay, coloured with the Catppuccin Mocha rainbow palette.
//! A sweep line and subtitle appear after all letters land, followed by
//! a sliding progress bar.  The whole thing fades out once Emacs connects.

use std::time::Instant;

use cosmic_text::{
    Attrs, Buffer as CtBuffer, Family, FontSystem, Metrics, Shaping, SwashCache, Weight,
};
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
    utils::{Buffer as SBuffer, Logical, Physical, Point, Rectangle, Size, Transform},
};

// --- Palette (Catppuccin Mocha) -----------------------------------------------

const BG_COLOR: [f32; 4] = [0.118, 0.118, 0.180, 1.0]; // Base #1e1e2e
const ACCENT: [f32; 4] = [0.537, 0.706, 0.980, 0.8]; // Blue #89b4fa

/// Per-letter BGRA (Rosewater → Flamingo → Mauve → Blue → Teal → Green).
const LETTER_COLORS: [[u8; 4]; 6] = [
    [0xdc, 0xe0, 0xf5, 0xff], // e
    [0xcd, 0xcd, 0xf2, 0xff], // m
    [0xf7, 0xa6, 0xcb, 0xff], // s
    [0xfa, 0xb4, 0x89, 0xff], // k
    [0xd5, 0xe2, 0x94, 0xff], // i
    [0xa1, 0xe3, 0xa6, 0xff], // n
];
const SUBTITLE_FG: [u8; 4] = [0xc8, 0xad, 0xa6, 0xff]; // Subtext0 #a6adc8

// --- Timing -------------------------------------------------------------------

const STAGGER: f32 = 0.12;
const LETTER_DUR: f32 = 0.35;
const SLIDE_PX: f32 = 50.0;

const LINE_DELAY: f32 = 0.70;
const LINE_DUR: f32 = 0.40;

const SUB_DELAY: f32 = 0.85;
const SUB_DUR: f32 = 0.30;

const BREATHE_PERIOD: f32 = 2.5;
const BREATHE_MIN: f32 = 0.88;
const FADE_OUT: f32 = 0.4;

const BAR_H: i32 = 3;
const BAR_RATIO: f32 = 0.30;
const BAR_CYCLE: f32 = 1.8;

// --- Layout -------------------------------------------------------------------

const FONT_RATIO: f32 = 0.07;
const FONT_MIN: f32 = 48.0;
const FONT_MAX: f32 = 128.0;
const SUB_SCALE: f32 = 0.22; // subtitle font relative to main
const LINE_H: i32 = 2;
const PAD: i32 = 2; // pixel padding around glyphs
const GAP_WORD_LINE: i32 = 8;
const GAP_LINE_SUB: i32 = 6;
const GAP_SUB_BAR: i32 = 12;

const WORD: &str = "emskin";
const SUBTITLE: &str = "EMACS+SKIN";

/// Time (seconds) after which all letters have finished their entrance.
const ALL_LANDED: f32 = WORD.len() as f32 * STAGGER + LETTER_DUR;

// ---------------------------------------------------------------------------

struct LetterSlot {
    buf: MemoryRenderBuffer,
    commit: CommitCounter,
    w: i32,
    h: i32,
    color: [u8; 4],
    delay: f32,
}

pub struct SplashScreen {
    /// Set on the first `build_elements` call (not in `new()`), so the
    /// animation begins when the window is actually rendering — not during
    /// compositor init where hundreds of ms can elapse invisibly.
    start: Option<Instant>,
    dismiss_time: Option<Instant>,
    done: bool,

    font_system: FontSystem,
    swash_cache: SwashCache,

    letters: Vec<LetterSlot>,
    subtitle_buf: MemoryRenderBuffer,
    subtitle_commit: CommitCounter,
    subtitle_w: i32,
    subtitle_h: i32,

    cached_font_size: i32,
    total_word_w: i32,
    letter_h: i32,

    bg_id: Id,
    bg_commit: CommitCounter,
    line_id: Id,
    line_commit: CommitCounter,
    bar_id: Id,
    bar_commit: CommitCounter,
}

impl Default for SplashScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl SplashScreen {
    pub fn new() -> Self {
        let letters = LETTER_COLORS
            .iter()
            .enumerate()
            .map(|(i, &c)| LetterSlot {
                buf: MemoryRenderBuffer::new(Fourcc::Argb8888, (1, 1), 1, Transform::Normal, None),
                commit: CommitCounter::default(),
                w: 0,
                h: 0,
                color: c,
                delay: i as f32 * STAGGER,
            })
            .collect();

        Self {
            start: None,
            dismiss_time: None,
            done: false,
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            letters,
            subtitle_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            subtitle_commit: CommitCounter::default(),
            subtitle_w: 0,
            subtitle_h: 0,
            cached_font_size: 0,
            total_word_w: 0,
            letter_h: 0,
            bg_id: Id::new(),
            bg_commit: CommitCounter::default(),
            line_id: Id::new(),
            line_commit: CommitCounter::default(),
            bar_id: Id::new(),
            bar_commit: CommitCounter::default(),
        }
    }

    pub fn dismiss(&mut self) {
        if self.dismiss_time.is_none() {
            self.dismiss_time = Some(Instant::now());
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn build_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
        scale: f64,
    ) -> (
        Vec<SolidColorRenderElement>,
        Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
    ) {
        if self.done {
            return (vec![], vec![]);
        }

        let font_size = (output_size.h as f32 * FONT_RATIO)
            .clamp(FONT_MIN, FONT_MAX)
            .round() as i32;
        if font_size != self.cached_font_size {
            self.rebuild(font_size as f32);
        }

        let elapsed = self
            .start
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs_f32();
        let all_landed = ALL_LANDED;

        // --- Global alpha (dismiss fade-out) ----------------------------------
        let g_alpha = if let Some(d) = self.dismiss_time {
            let t = d.elapsed().as_secs_f32() / FADE_OUT;
            if t >= 1.0 {
                self.done = true;
                return (vec![], vec![]);
            }
            1.0 - ease_in_cubic(t)
        } else {
            1.0
        };

        let breathe = if self.dismiss_time.is_none() && elapsed > all_landed {
            let t = (elapsed - all_landed) / BREATHE_PERIOD;
            let w = ((t * std::f32::consts::TAU).sin() + 1.0) / 2.0;
            BREATHE_MIN + w * (1.0 - BREATHE_MIN)
        } else {
            1.0
        };

        // --- Vertical layout (centred) ----------------------------------------
        let total_h = self.letter_h
            + GAP_WORD_LINE
            + LINE_H
            + GAP_LINE_SUB
            + self.subtitle_h
            + GAP_SUB_BAR
            + BAR_H;
        let base_y = (output_size.h - total_h) / 2;
        let word_y = base_y;
        let line_y = word_y + self.letter_h + GAP_WORD_LINE;
        let sub_y = line_y + LINE_H + GAP_LINE_SUB;
        let bar_y = sub_y + self.subtitle_h + GAP_SUB_BAR;
        let word_x = (output_size.w - self.total_word_w) / 2;

        let mut solids = Vec::with_capacity(3);
        let mut labels = Vec::with_capacity(7);

        // 1. Background (only increment commit when alpha is changing).
        let bg_alpha = BG_COLOR[3] * g_alpha;
        if g_alpha < 1.0 {
            self.bg_commit.increment();
        }
        solids.push(solid(
            &self.bg_id,
            self.bg_commit,
            (0, 0),
            output_size,
            scale,
            [BG_COLOR[0], BG_COLOR[1], BG_COLOR[2], bg_alpha],
        ));

        // 2. Letters -----------------------------------------------------------
        let mut cx = word_x;
        for slot in &self.letters {
            let rt = (elapsed - slot.delay) / LETTER_DUR;
            let t = rt.clamp(0.0, 1.0);
            let alpha = if rt < 0.0 {
                0.0
            } else {
                ease_out_cubic(t) * g_alpha * breathe
            };
            let y_off = if rt < 0.0 {
                SLIDE_PX as i32
            } else {
                (SLIDE_PX * (1.0 - ease_out_back(t))).round() as i32
            };
            let vy = (self.letter_h - slot.h) / 2; // vertical centre
            if alpha > 0.001 {
                let loc = Point::<f64, Logical>::from((cx as f64, (word_y + vy + y_off) as f64));
                if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    loc.to_physical(scale),
                    &slot.buf,
                    Some(alpha),
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    labels.push(e);
                }
            }
            cx += slot.w;
        }

        // 3. Sweep line --------------------------------------------------------
        if self.dismiss_time.is_none() {
            let lt = ((elapsed - LINE_DELAY) / LINE_DUR).clamp(0.0, 1.0);
            if lt > 0.0 {
                let lw = (self.total_word_w as f32 * ease_out_cubic(lt)).round() as i32;
                self.line_commit.increment();
                solids.push(solid(
                    &self.line_id,
                    self.line_commit,
                    (word_x, line_y),
                    Size::from((lw.max(1), LINE_H)),
                    scale,
                    [
                        ACCENT[0],
                        ACCENT[1],
                        ACCENT[2],
                        ACCENT[3] * g_alpha * breathe,
                    ],
                ));
            }
        }

        // 4. Subtitle ----------------------------------------------------------
        {
            let st = (elapsed - SUB_DELAY) / SUB_DUR;
            let sa = if self.dismiss_time.is_some() {
                g_alpha * breathe
            } else if st < 0.0 {
                0.0
            } else {
                ease_out_cubic(st.min(1.0)) * g_alpha * breathe
            };
            if sa > 0.001 {
                let sx = (output_size.w - self.subtitle_w) / 2;
                let loc = Point::<f64, Logical>::from((sx as f64, sub_y as f64));
                if let Ok(e) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    loc.to_physical(scale),
                    &self.subtitle_buf,
                    Some(sa),
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    labels.push(e);
                }
            }
        }

        // 5. Progress bar (appears after letters land) -------------------------
        if self.dismiss_time.is_none() && elapsed > all_landed {
            let bw = (self.total_word_w as f32 * BAR_RATIO).round() as i32;
            let travel = (self.total_word_w - bw).max(0);
            let p = ((elapsed % BAR_CYCLE) / BAR_CYCLE * std::f32::consts::PI)
                .sin()
                .powi(2);
            let bx = word_x + (travel as f32 * p) as i32;
            self.bar_commit.increment();
            solids.push(solid(
                &self.bar_id,
                self.bar_commit,
                (bx, bar_y),
                Size::from((bw.max(1), BAR_H)),
                scale,
                [ACCENT[0], ACCENT[1], ACCENT[2], 0.6 * g_alpha * breathe],
            ));
        }

        (solids, labels)
    }

    // --- Rebuild letter / subtitle buffers ------------------------------------

    fn rebuild(&mut self, font_size: f32) {
        let lh = font_size * 1.1;
        let fs = &mut self.font_system;
        let cache = &mut self.swash_cache;

        let mut tw = 0;
        let mut mh = 0;
        for (i, ch) in WORD.chars().enumerate() {
            let s = &mut self.letters[i];
            let (w, h) = render_text_buf(
                &ch.to_string(),
                &s.color,
                font_size,
                lh,
                true,
                fs,
                cache,
                &mut s.buf,
                &mut s.commit,
            );
            s.w = w;
            s.h = h;
            tw += w;
            mh = mh.max(h);
        }
        self.total_word_w = tw;
        self.letter_h = mh;

        let sub_fs = font_size * SUB_SCALE;
        let fs = &mut self.font_system;
        let cache = &mut self.swash_cache;
        let (sw, sh) = render_text_buf(
            SUBTITLE,
            &SUBTITLE_FG,
            sub_fs,
            sub_fs * 1.2,
            false,
            fs,
            cache,
            &mut self.subtitle_buf,
            &mut self.subtitle_commit,
        );
        self.subtitle_w = sw;
        self.subtitle_h = sh;
        self.cached_font_size = font_size.round() as i32;
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Shorthand for building a `SolidColorRenderElement` from logical coords.
fn solid(
    id: &Id,
    commit: CommitCounter,
    loc: (i32, i32),
    size: Size<i32, Logical>,
    scale: f64,
    color: [f32; 4],
) -> SolidColorRenderElement {
    let ploc = Point::<i32, Physical>::from((
        (loc.0 as f64 * scale).round() as i32,
        (loc.1 as f64 * scale).round() as i32,
    ));
    let psz = Size::<i32, Physical>::from((
        (size.w as f64 * scale).round().max(1.0) as i32,
        (size.h as f64 * scale).round().max(1.0) as i32,
    ));
    SolidColorRenderElement::new(
        id.clone(),
        Rectangle::new(ploc, psz),
        commit,
        color,
        Kind::Unspecified,
    )
}

/// Shape, measure, and render a string into a `MemoryRenderBuffer`.
#[allow(clippy::too_many_arguments)]
fn render_text_buf(
    text: &str,
    fg: &[u8; 4], // BGRA
    font_size: f32,
    line_height: f32,
    bold: bool,
    fs: &mut FontSystem,
    cache: &mut SwashCache,
    buf: &mut MemoryRenderBuffer,
    commit: &mut CommitCounter,
) -> (i32, i32) {
    let metrics = Metrics::new(font_size, line_height);
    let mut ct = CtBuffer::new(fs, metrics);
    ct.set_size(fs, Some(f32::INFINITY), Some(f32::INFINITY));

    let attrs = if bold {
        Attrs::new().family(Family::SansSerif).weight(Weight::BOLD)
    } else {
        Attrs::new().family(Family::SansSerif)
    };
    ct.set_text(fs, text, &attrs, Shaping::Advanced, None);
    ct.shape_until_scroll(fs, false);

    let (mut tw, mut th) = (0.0f32, 0.0f32);
    for run in ct.layout_runs() {
        tw = tw.max(run.line_w);
        th = th.max(run.line_top + run.line_height);
    }
    let tw = tw.ceil() as i32;
    let th = th.ceil() as i32;
    let bw = (tw + PAD * 2).max(1);
    let bh = (th + PAD * 2).max(1);

    let buf_size: Size<i32, SBuffer> = (bw, bh).into();
    effect_core::paint_buffer(buf, buf_size, |data| {
        data.fill(0);
        effect_core::draw_text_onto(data, bw, bh, PAD, PAD, fg, &mut ct, fs, cache);
    });
    commit.increment();

    (bw, bh)
}

// ---------------------------------------------------------------------------
// Easing
// ---------------------------------------------------------------------------

fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_in_cubic(t: f32) -> f32 {
    t.powi(3)
}

/// Ease-out with slight overshoot (bounce past target then settle).
fn ease_out_back(t: f32) -> f32 {
    let c1: f32 = 1.70158;
    let c3 = c1 + 1.0;
    1.0 + c3 * (t - 1.0).powi(3) + c1 * (t - 1.0).powi(2)
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl effect_core::Effect for SplashScreen {
    fn name(&self) -> &'static str {
        "splash"
    }

    fn is_active(&self) -> bool {
        !self.done
    }

    fn chain_position(&self) -> u8 {
        95
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        use effect_core::CustomElement;

        let (solids, labels) = self.build_elements(renderer, ctx.output_size, ctx.scale);

        // Intra-effect z-order: labels (topmost, letter bitmaps) → solids (bar/line).
        let mut out = Vec::with_capacity(solids.len() + labels.len());
        for label in labels {
            out.push(CustomElement::Label(label));
        }
        for solid in solids {
            out.push(CustomElement::Solid(solid));
        }
        out
    }

    fn post_paint(&mut self) -> bool {
        // Request another frame while animating — matches the `needs_redraw = true`
        // previously set unconditionally in winit.rs:469 for splash.
        !self.done
    }

    fn should_remove(&self) -> bool {
        self.done
    }
}
