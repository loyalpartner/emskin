//! Skeleton overlay — debug inspector drawing wireframe rectangles for each
//! Emacs frame region (frame chrome, windows, header/mode-line, echo area).
//!
//! Rects are pushed by the Emacs client via `SetSkeleton` IPC. Every rect
//! gets a colored border drawn on top of Emacs. The kind+coordinate labels
//! are stacked in a single panel at the top-left of the compositor window —
//! clicking a label flashes the corresponding rect's border for ~800 ms so
//! it's easy to identify which area a label refers to.

use std::time::{Duration, Instant};

use cosmic_text::{
    Attrs, Buffer as CtBuffer, Color as CtColor, Family, FontSystem, Metrics, Shaping, SwashCache,
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
    utils::{Buffer as SBuffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use serde::{Deserialize, Serialize};
use smithay::utils::{Point as SPoint, Size as SSize};

/// Wire format: flat `{kind, label, x, y, w, h, selected}` JSON shape that
/// matches what Emacs's `emskin--report-skeleton` sends.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkeletonRectWire {
    kind: String,
    #[serde(default)]
    label: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    #[serde(default)]
    selected: bool,
}

/// A single rect pushed by Emacs via the `set_skeleton` IPC. Kind is one of:
/// frame / chrome / menu-bar / tool-bar / tab-bar / window / header-line /
/// tab-line / mode-line / echo-area / mini-buffer / (unknown).
///
/// Rust-side representation uses smithay's typed `Rectangle<i32, Logical>`;
/// serde serialization round-trips through `SkeletonRectWire` to preserve the
/// existing flat JSON wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "SkeletonRectWire", into = "SkeletonRectWire")]
pub struct SkeletonRect {
    pub kind: String,
    pub label: String,
    pub rect: Rectangle<i32, Logical>,
    pub selected: bool,
}

impl From<SkeletonRectWire> for SkeletonRect {
    fn from(w: SkeletonRectWire) -> Self {
        Self {
            kind: w.kind,
            label: w.label,
            rect: Rectangle::new(SPoint::from((w.x, w.y)), SSize::from((w.w, w.h))),
            selected: w.selected,
        }
    }
}

impl From<SkeletonRect> for SkeletonRectWire {
    fn from(r: SkeletonRect) -> Self {
        Self {
            kind: r.kind,
            label: r.label,
            x: r.rect.loc.x,
            y: r.rect.loc.y,
            w: r.rect.size.w,
            h: r.rect.size.h,
            selected: r.selected,
        }
    }
}

// ---------------------------------------------------------------------------
// Color palette (RGBA linear, matches SolidColorRenderElement's format).
// ---------------------------------------------------------------------------

const COLOR_FRAME: [f32; 4] = [1.0, 0.15, 0.15, 1.0];
const COLOR_CHROME: [f32; 4] = [0.65, 0.25, 1.0, 1.0];
const COLOR_MENU_BAR: [f32; 4] = [1.0, 0.65, 0.2, 0.85];
const COLOR_TOOL_BAR: [f32; 4] = [0.95, 0.45, 0.15, 0.85];
const COLOR_TAB_BAR: [f32; 4] = [0.2, 0.85, 0.85, 0.85];
const COLOR_WINDOW: [f32; 4] = [1.0, 0.4, 0.8, 0.75];
const COLOR_SELECTED: [f32; 4] = [1.0, 0.85, 0.2, 0.95];
const COLOR_HEADER: [f32; 4] = [0.4, 0.9, 0.4, 0.75];
const COLOR_MODELINE: [f32; 4] = [0.3, 0.8, 1.0, 0.75];
const COLOR_ECHO: [f32; 4] = [1.0, 0.4, 1.0, 0.75];
const COLOR_DEFAULT: [f32; 4] = [0.8, 0.8, 0.8, 0.7];

/// Label background (BGRA — matches Fourcc::Argb8888 on little-endian).
const LABEL_BG: [u8; 4] = [20, 20, 30, 210];

const FONT_SIZE: f32 = 12.0;
const LINE_HEIGHT: f32 = 14.0;
const LABEL_PAD: i32 = 3;
/// Pixels to shrink each rect inward per nesting level. Keeps nested
/// borders from coinciding so every rect stays visually distinct.
const INSET_STEP: i32 = 2;
/// Upper bound so a deeply nested rect doesn't collapse to zero.
const MAX_INSET: i32 = 8;

/// Margin from the compositor window edge to the label panel.
const PANEL_MARGIN: i32 = 12;
/// Vertical gap between stacked labels in the panel.
const PANEL_GAP: i32 = 1;

/// Duration of the click flash highlight.
const FLASH_DURATION_MS: u64 = 1500;
/// Alpha applied to the kind color when filling the flashing rect.
const FLASH_FILL_ALPHA: f32 = 0.7;

fn color_for(kind: &str, selected: bool) -> [f32; 4] {
    if selected {
        return COLOR_SELECTED;
    }
    match kind {
        "frame" => COLOR_FRAME,
        "chrome" => COLOR_CHROME,
        "menu-bar" => COLOR_MENU_BAR,
        "tool-bar" => COLOR_TOOL_BAR,
        "tab-bar" => COLOR_TAB_BAR,
        "window" => COLOR_WINDOW,
        "header-line" | "tab-line" => COLOR_HEADER,
        "mode-line" => COLOR_MODELINE,
        "echo-area" | "mini-buffer" => COLOR_ECHO,
        _ => COLOR_DEFAULT,
    }
}

/// Convert an RGBA float color to BGRA u8 for label foreground.
fn color_fg_bgra(color: [f32; 4]) -> [u8; 4] {
    let r = (color[0].clamp(0.0, 1.0) * 255.0) as u8;
    let g = (color[1].clamp(0.0, 1.0) * 255.0) as u8;
    let b = (color[2].clamp(0.0, 1.0) * 255.0) as u8;
    [b, g, r, 255]
}

// ---------------------------------------------------------------------------
// Per-rect stable state
// ---------------------------------------------------------------------------

struct Entry {
    rect: SkeletonRect,
    /// Visually drawn rect (`rect` inset inward by `depth * INSET_STEP`
    /// logical pixels, where depth = number of other rects that fully
    /// contain this one). Frame stays at depth 0 so its border is drawn
    /// exactly; every nested level shrinks a bit so neighboring borders
    /// don't coincide.
    draw_rect: (i32, i32, i32, i32),
    top_id: Id,
    bottom_id: Id,
    left_id: Id,
    right_id: Id,
    fill_id: Id,
    label_buf: MemoryRenderBuffer,
    commit: CommitCounter,
    last_text: String,
    last_fg: [u8; 4],
    label_size_log: (i32, i32),
    /// Absolute logical position where this entry's label is drawn in the
    /// label panel (top-left corner of the rasterized label buffer).
    panel_pos: (i32, i32),
    has_label: bool,
    /// If `Some(t)`, the border is flashing; ends when `Instant::now() >= t`.
    flash_until: Option<Instant>,
}

impl Entry {
    fn new() -> Self {
        Self {
            rect: SkeletonRect {
                kind: String::new(),
                label: String::new(),
                rect: Rectangle::default(),
                selected: false,
            },
            draw_rect: (0, 0, 0, 0),
            top_id: Id::new(),
            bottom_id: Id::new(),
            left_id: Id::new(),
            right_id: Id::new(),
            fill_id: Id::new(),
            label_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            commit: CommitCounter::default(),
            last_text: String::new(),
            last_fg: [0, 0, 0, 0],
            label_size_log: (0, 0),
            panel_pos: (0, 0),
            has_label: false,
            flash_until: None,
        }
    }
}

// ---------------------------------------------------------------------------
// SkeletonOverlay
// ---------------------------------------------------------------------------

pub struct SkeletonOverlay {
    pub enabled: bool,
    /// `true` between a Press that hit a label and its paired Release — swallows
    /// the Release so downstream pointer focus doesn't see a dangling click.
    /// Migrated here from `EmskinState::skeleton_click_absorbed` so skeleton owns
    /// its own input-flow state.
    click_absorbed: bool,
    font_system: Option<FontSystem>,
    swash_cache: SwashCache,
    entries: Vec<Entry>,
}

impl SkeletonOverlay {
    pub fn new() -> Self {
        Self {
            enabled: false,
            click_absorbed: false,
            font_system: None,
            swash_cache: SwashCache::new(),
            entries: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.click_absorbed = false;
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Hit-test a logical cursor position against the panel label boxes.
    /// On hit: starts the target rect's flash effect and returns a clone
    /// of the corresponding SkeletonRect.
    pub fn click_at(&mut self, pos: Point<f64, Logical>) -> Option<SkeletonRect> {
        if !self.enabled {
            return None;
        }
        let px = pos.x as i32;
        let py = pos.y as i32;
        let hit_idx = self.entries.iter().enumerate().find_map(|(idx, entry)| {
            if !entry.has_label {
                return None;
            }
            let (lx, ly) = entry.panel_pos;
            let (lw, lh) = entry.label_size_log;
            if px >= lx && px < lx + lw && py >= ly && py < ly + lh {
                Some(idx)
            } else {
                None
            }
        })?;
        let entry = &mut self.entries[hit_idx];
        entry.flash_until = Some(Instant::now() + Duration::from_millis(FLASH_DURATION_MS));
        entry.commit.increment();
        Some(entry.rect.clone())
    }

    /// Replace the current rect list. Entries are reused by index to keep
    /// stable render IDs (important for smithay's damage tracker).
    pub fn set_rects(&mut self, rects: Vec<SkeletonRect>) {
        while self.entries.len() < rects.len() {
            self.entries.push(Entry::new());
        }
        self.entries.truncate(rects.len());

        for (entry, rect) in self.entries.iter_mut().zip(rects.into_iter()) {
            // Text format: "<kind> <label> (x,y) WxH" (label skipped if empty).
            let text = if rect.label.is_empty() {
                format!(
                    "{} ({},{}) {}x{}",
                    rect.kind, rect.rect.loc.x, rect.rect.loc.y, rect.rect.size.w, rect.rect.size.h
                )
            } else {
                format!(
                    "{} {} ({},{}) {}x{}",
                    rect.kind,
                    rect.label,
                    rect.rect.loc.x,
                    rect.rect.loc.y,
                    rect.rect.size.w,
                    rect.rect.size.h
                )
            };
            let fg = color_fg_bgra(color_for(&rect.kind, rect.selected));

            if text != entry.last_text || fg != entry.last_fg {
                // Disjoint field borrow: initialize FontSystem lazily, then
                // pass `&mut self.font_system` and `&mut self.swash_cache`
                // to render_label without whole-self borrow.
                if self.font_system.is_none() {
                    tracing::info!("skeleton: initializing cosmic-text FontSystem");
                    self.font_system = Some(FontSystem::new());
                }
                let font_system = self.font_system.as_mut().unwrap();
                let size = render_label(
                    &mut entry.label_buf,
                    &text,
                    fg,
                    font_system,
                    &mut self.swash_cache,
                );
                entry.last_text = text;
                entry.last_fg = fg;
                entry.label_size_log = size;
            }

            entry.rect = rect;
            entry.commit.increment();
        }

        // Compute per-entry inset from nesting depth: depth = number of
        // earlier rects that fully contain this one. The frame is drawn
        // exactly on its outer bounds (depth 0); chrome/window are inset
        // by one step; menu-bar/tool-bar/header-line/mode-line inside
        // those are inset by two. Order matters — elisp sends outer
        // containers first. O(n²) but n ≲ 20 for a debug overlay.
        for i in 0..self.entries.len() {
            let r_i = self.entries[i].rect.rect;
            let mut depth = 0i32;
            for j in 0..i {
                let r_j = self.entries[j].rect.rect;
                if r_j.loc.x <= r_i.loc.x
                    && r_j.loc.y <= r_i.loc.y
                    && r_j.loc.x + r_j.size.w >= r_i.loc.x + r_i.size.w
                    && r_j.loc.y + r_j.size.h >= r_i.loc.y + r_i.size.h
                {
                    depth += 1;
                }
            }
            let raw_inset = (depth * INSET_STEP).min(MAX_INSET);
            // Clamp so a tiny rect never collapses to zero or negative.
            let max_w_inset = (r_i.size.w / 3).max(0);
            let max_h_inset = (r_i.size.h / 3).max(0);
            let inset = raw_inset.min(max_w_inset).min(max_h_inset);
            self.entries[i].draw_rect = (
                r_i.loc.x + inset,
                r_i.loc.y + inset,
                (r_i.size.w - inset * 2).max(1),
                (r_i.size.h - inset * 2).max(1),
            );
        }

        // Mark which entries have renderable labels; actual panel layout
        // (position in bottom-right corner) is deferred to build_elements
        // because it needs the current output size.
        for entry in self.entries.iter_mut() {
            entry.has_label = entry.label_size_log.0 > 0 && entry.label_size_log.1 > 0;
        }
    }

    pub fn build_elements(
        &mut self,
        renderer: &mut GlesRenderer,
        output_size_log: Size<i32, Logical>,
        scale: f64,
    ) -> (
        Vec<SolidColorRenderElement>,
        Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
    ) {
        if !self.enabled || self.entries.is_empty() {
            return (Vec::new(), Vec::new());
        }

        // Flash pre-pass: clear expired flashes. The fill element is a
        // stable solid during its lifetime, so we only need to bump the
        // commit counter when it ends (removing the element from the
        // list) so the damage tracker repaints the cleared area.
        let now = Instant::now();
        for entry in self.entries.iter_mut() {
            if let Some(until) = entry.flash_until {
                if now >= until {
                    entry.flash_until = None;
                    entry.commit.increment();
                }
            }
        }

        // Panel layout: right-aligned vertical stack anchored to the
        // bottom-right of the compositor window.
        let mut max_w = 0i32;
        let mut total_h = 0i32;
        let mut visible_count = 0i32;
        for entry in &self.entries {
            if entry.has_label {
                max_w = max_w.max(entry.label_size_log.0);
                total_h += entry.label_size_log.1;
                visible_count += 1;
            }
        }
        if visible_count > 1 {
            total_h += (visible_count - 1) * PANEL_GAP;
        }
        let panel_right = output_size_log.w - PANEL_MARGIN;
        let panel_bottom = output_size_log.h - PANEL_MARGIN;
        let mut panel_y = (panel_bottom - total_h).max(PANEL_MARGIN);
        for entry in self.entries.iter_mut() {
            if !entry.has_label {
                continue;
            }
            let lw = entry.label_size_log.0;
            let lh = entry.label_size_log.1;
            let ex = (panel_right - lw).max(PANEL_MARGIN);
            entry.panel_pos = (ex, panel_y);
            panel_y += lh + PANEL_GAP;
        }

        let s: Scale<f64> = Scale::from(scale);
        let mut borders = Vec::with_capacity(self.entries.len() * 4);
        let mut fills: Vec<SolidColorRenderElement> = Vec::new();
        let mut labels = Vec::with_capacity(self.entries.len());

        for entry in &self.entries {
            let color = color_for(&entry.rect.kind, entry.rect.selected);
            let thickness: i32 = if entry.rect.selected { 2 } else { 1 };
            // When flashing, fill the entire rect interior with the kind
            // color (same color as the label text) so the user can
            // immediately spot which region was clicked.
            let flash_fill: Option<[f32; 4]> = entry
                .flash_until
                .map(|_| [color[0], color[1], color[2], FLASH_FILL_ALPHA]);

            // Borders are drawn on the inset rect (see draw_rect docs);
            // label text still reports the original rect's coordinates.
            let (dx, dy, dw, dh) = entry.draw_rect;
            let tl_log = Point::<f64, Logical>::from((dx as f64, dy as f64));
            let br_log = Point::<f64, Logical>::from(((dx + dw) as f64, (dy + dh) as f64));
            let tl: Point<i32, Physical> = tl_log.to_physical(s).to_i32_round();
            let br: Point<i32, Physical> = br_log.to_physical(s).to_i32_round();
            let w_phys = (br.x - tl.x).max(1);
            let h_phys = (br.y - tl.y).max(1);

            let mut push_edge = |id: Id, x: i32, y: i32, w: i32, h: i32| {
                borders.push(SolidColorRenderElement::new(
                    id,
                    Rectangle::new(
                        Point::<i32, Physical>::from((x, y)),
                        Size::<i32, Physical>::from((w, h)),
                    ),
                    entry.commit,
                    color,
                    Kind::Unspecified,
                ));
            };
            push_edge(entry.top_id.clone(), tl.x, tl.y, w_phys, thickness);
            push_edge(
                entry.bottom_id.clone(),
                tl.x,
                br.y - thickness,
                w_phys,
                thickness,
            );
            push_edge(entry.left_id.clone(), tl.x, tl.y, thickness, h_phys);
            push_edge(
                entry.right_id.clone(),
                br.x - thickness,
                tl.y,
                thickness,
                h_phys,
            );

            // Flash fill — whole interior painted in kind color.
            if let Some(fill_color) = flash_fill {
                fills.push(SolidColorRenderElement::new(
                    entry.fill_id.clone(),
                    Rectangle::new(tl, Size::<i32, Physical>::from((w_phys, h_phys))),
                    entry.commit,
                    fill_color,
                    Kind::Unspecified,
                ));
            }

            if entry.has_label {
                let label_loc = Point::<f64, Logical>::from((
                    entry.panel_pos.0 as f64,
                    entry.panel_pos.1 as f64,
                ))
                .to_physical(s);
                if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    label_loc,
                    &entry.label_buf,
                    None,
                    None,
                    None,
                    Kind::Unspecified,
                ) {
                    labels.push(elem);
                }
            }
        }

        // Final order: borders first (topmost within skeleton solids) so
        // they visually sit above the fills; fills underneath.
        let mut solids = borders;
        solids.extend(fills);
        (solids, labels)
    }
}

impl Default for SkeletonOverlay {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// cosmic-text rasterization
// ---------------------------------------------------------------------------

/// Rasterize `text` into `buf`, resizing it to fit. Returns the (w, h) in
/// logical pixels. `fg` is BGRA.
fn render_label(
    buf: &mut MemoryRenderBuffer,
    text: &str,
    fg: [u8; 4],
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) -> (i32, i32) {
    let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
    let mut ct_buffer = CtBuffer::new(font_system, metrics);
    ct_buffer.set_size(font_system, Some(f32::INFINITY), Some(f32::INFINITY));
    ct_buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
        None,
    );
    ct_buffer.shape_until_scroll(font_system, false);

    // Measure: find the maximum line width and total height.
    let mut max_w = 0.0f32;
    let mut max_bottom = 0.0f32;
    for run in ct_buffer.layout_runs() {
        max_w = max_w.max(run.line_w);
        max_bottom = max_bottom.max(run.line_top + run.line_height);
    }

    let inner_w = max_w.ceil() as i32;
    let inner_h = max_bottom.ceil() as i32;
    let buf_w = (inner_w + LABEL_PAD * 2).max(1);
    let buf_h = (inner_h + LABEL_PAD * 2).max(1);

    let buf_size: Size<i32, SBuffer> = (buf_w, buf_h).into();
    effect_core::paint_buffer(buf, buf_size, |data| {
        let stride = buf_w * 4;
        data.chunks_exact_mut(4)
            .for_each(|c| c.copy_from_slice(&LABEL_BG));

        let ct_color = CtColor::rgba(fg[2], fg[1], fg[0], 255);
        ct_buffer.draw(
            font_system,
            swash_cache,
            ct_color,
            |gx, gy, gw, gh, color| {
                let alpha = color.a() as u32;
                if alpha == 0 {
                    return;
                }
                let px_r = color.r() as u32;
                let px_g = color.g() as u32;
                let px_b = color.b() as u32;

                for dy in 0..gh as i32 {
                    for dx in 0..gw as i32 {
                        let x = gx + dx + LABEL_PAD;
                        let y = gy + dy + LABEL_PAD;
                        if x < 0 || x >= buf_w || y < 0 || y >= buf_h {
                            continue;
                        }
                        let off = (y * stride + x * 4) as usize;
                        let inv = 255 - alpha;
                        // Source arrives as linear RGBA; destination is BGRA.
                        let db = data[off] as u32;
                        let dg = data[off + 1] as u32;
                        let dr = data[off + 2] as u32;
                        data[off] = ((db * inv + px_b * alpha) / 255) as u8;
                        data[off + 1] = ((dg * inv + px_g * alpha) / 255) as u8;
                        data[off + 2] = ((dr * inv + px_r * alpha) / 255) as u8;
                        data[off + 3] = 255;
                    }
                }
            },
        );
    });

    (buf_w, buf_h)
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl effect_core::Effect for SkeletonOverlay {
    fn name(&self) -> &'static str {
        "skeleton"
    }

    fn is_active(&self) -> bool {
        self.enabled
    }

    fn chain_position(&self) -> u8 {
        85
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        use effect_core::CustomElement;

        let (solids, labels) = self.build_elements(renderer, ctx.output_size, ctx.scale);

        // Intra-effect z-order: labels (panel text on right) → solids (borders).
        let mut out = Vec::with_capacity(solids.len() + labels.len());
        for label in labels {
            out.push(CustomElement::Label(label));
        }
        for solid in solids {
            out.push(CustomElement::Solid(solid));
        }
        out
    }
}
