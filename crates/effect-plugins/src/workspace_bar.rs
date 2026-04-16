//! Workspace bar — a top-of-screen strip showing workspace buttons + center title.
//! Only visible when there are 2+ workspaces and --bar=builtin.
//!
//! Layout: [pill buttons on left]  [active workspace title centered]
//! Active button = Catppuccin Blue pill + dark text.
//! Inactive button = gray text only.

use cosmic_text::{Attrs, Buffer as CtBuffer, Family, FontSystem, Metrics, Shaping, SwashCache};
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

pub const BAR_HEIGHT: i32 = 28;
const PILL_H: i32 = 20;
const PILL_PAD_H: i32 = 6;
const INACTIVE_PAD_H: i32 = 2;
const PILL_RADIUS: f32 = 6.0;
const BUTTON_GAP: i32 = 6;
const BAR_MARGIN_LEFT: i32 = 6;
const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = 15.0;
const TITLE_FONT_SIZE: f32 = 12.0;
const TITLE_LINE_HEIGHT: f32 = 14.0;
// 1px padding around text labels for glyph anti-aliasing overflow.
const TEXT_LABEL_PAD: i32 = 1;

// Catppuccin Mocha Base #1e1e2e at 95% (RGBA linear for SolidColorRenderElement).
const BAR_BG: [f32; 4] = [0.118, 0.118, 0.180, 0.95];
// Catppuccin Surface0 #313244.
const SEP_COLOR: [f32; 4] = [0.192, 0.196, 0.267, 1.0];

// BGRA colors for MemoryRenderBuffer pixel rendering.
const PILL_BG: [u8; 4] = [250, 180, 137, 217]; // Catppuccin Blue #89b4fa at 85%
const ACTIVE_FG: [u8; 4] = [46, 30, 30, 255]; // Catppuccin Base #1e1e2e
const INACTIVE_FG: [u8; 4] = [134, 112, 108, 255]; // Catppuccin Overlay0 #6c7086
const TITLE_FG: [u8; 4] = [200, 173, 166, 255]; // Catppuccin Subtext0 #a6adc8

struct BarButton {
    workspace_id: u64,
    active: bool,
    label_buf: MemoryRenderBuffer,
    commit: CommitCounter,
    /// Logical hit rect for click detection.
    hit_rect: Rectangle<i32, Logical>,
    buf_size: (i32, i32),
    last_text: String,
    last_active: bool,
}

impl BarButton {
    fn new() -> Self {
        Self {
            workspace_id: 0,
            active: false,
            label_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            commit: CommitCounter::default(),
            hit_rect: Rectangle::default(),
            buf_size: (0, 0),
            last_text: String::new(),
            last_active: false,
        }
    }
}

pub struct WorkspaceBar {
    /// Toggled by `--bar=none` at startup via `set_bar_enabled` IPC. When
    /// `false`, `is_active()` returns `false` and the bar never paints.
    enabled: bool,
    font_system: Option<FontSystem>,
    swash_cache: SwashCache,
    buttons: Vec<BarButton>,
    bg_id: Id,
    bg_commit: CommitCounter,
    sep_id: Id,
    sep_commit: CommitCounter,
    // Center title.
    title_buf: MemoryRenderBuffer,
    title_commit: CommitCounter,
    title_size: (i32, i32),
    last_title: String,
}

impl Default for WorkspaceBar {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceBar {
    pub fn new() -> Self {
        Self {
            enabled: true,
            font_system: None,
            swash_cache: SwashCache::new(),
            buttons: Vec::new(),
            bg_id: Id::new(),
            bg_commit: CommitCounter::default(),
            sep_id: Id::new(),
            sep_commit: CommitCounter::default(),
            title_buf: MemoryRenderBuffer::new(
                Fourcc::Argb8888,
                (1, 1),
                1,
                Transform::Normal,
                None,
            ),
            title_commit: CommitCounter::default(),
            title_size: (0, 0),
            last_title: String::new(),
        }
    }

    pub fn visible(&self) -> bool {
        self.buttons.len() > 1
    }

    pub fn button_count(&self) -> usize {
        self.buttons.len()
    }

    pub fn height(&self) -> i32 {
        if self.visible() {
            BAR_HEIGHT
        } else {
            0
        }
    }

    /// Update the bar's workspace list and title. Re-renders only when changed.
    pub fn update(
        &mut self,
        workspaces: &[(u64, &str)], // (id, name) pairs
        active_id: u64,
    ) {
        let old_count = self.buttons.len();

        // Grow/shrink button pool.
        while self.buttons.len() < workspaces.len() {
            self.buttons.push(BarButton::new());
        }
        self.buttons.truncate(workspaces.len());

        // Bar visibility changed → damage bg/sep.
        if (old_count > 1) != (self.buttons.len() > 1) {
            self.bg_commit.increment();
            self.sep_commit.increment();
        }

        let mut x_cursor = BAR_MARGIN_LEFT;
        let mut active_name = "";

        for (&(ws_id, name), btn) in workspaces.iter().zip(self.buttons.iter_mut()) {
            btn.workspace_id = ws_id;
            let is_active = ws_id == active_id;
            let text = ws_id.to_string();
            let changed = text != btn.last_text || is_active != btn.last_active;

            if is_active {
                active_name = name;
            }

            if changed {
                btn.active = is_active;
                let font_system = self.font_system.get_or_insert_with(FontSystem::new);
                btn.buf_size = render_pill_button(
                    &mut btn.label_buf,
                    &text,
                    is_active,
                    font_system,
                    &mut self.swash_cache,
                );
                btn.last_text = text;
                btn.last_active = is_active;
                btn.commit.increment();
            }

            let y = (BAR_HEIGHT - PILL_H) / 2;
            btn.hit_rect = Rectangle::new((x_cursor, y).into(), (btn.buf_size.0, PILL_H).into());
            x_cursor += btn.buf_size.0 + BUTTON_GAP;
        }

        // Update center title if changed.
        if active_name != self.last_title {
            if active_name.is_empty() {
                self.title_size = (0, 0);
            } else {
                let font_system = self.font_system.get_or_insert_with(FontSystem::new);
                self.title_size = render_text_label(
                    &mut self.title_buf,
                    active_name,
                    &TITLE_FG,
                    TITLE_FONT_SIZE,
                    TITLE_LINE_HEIGHT,
                    font_system,
                    &mut self.swash_cache,
                );
            }
            self.last_title = active_name.to_string();
            self.title_commit.increment();
        }
    }

    /// Click test: returns workspace_id if a button was hit.
    pub fn click_at(&self, pos: Point<f64, Logical>) -> Option<u64> {
        if !self.visible() {
            return None;
        }
        let px = pos.x as i32;
        let py = pos.y as i32;
        self.buttons.iter().find_map(|btn| {
            let r = btn.hit_rect;
            if px >= r.loc.x && px < r.loc.x + r.size.w && py >= r.loc.y && py < r.loc.y + r.size.h
            {
                Some(btn.workspace_id)
            } else {
                None
            }
        })
    }

    /// Build render elements for the bar.
    pub fn build_elements(
        &self,
        renderer: &mut GlesRenderer,
        output_size: Size<i32, Logical>,
        scale: f64,
    ) -> (
        Vec<SolidColorRenderElement>,
        Vec<MemoryRenderBufferRenderElement<GlesRenderer>>,
    ) {
        if !self.visible() {
            return (Vec::new(), Vec::new());
        }

        let s: Scale<f64> = Scale::from(scale);
        let mut solids = Vec::with_capacity(2);
        let mut labels = Vec::with_capacity(self.buttons.len() + 1);

        // Bar background — full width, BAR_HEIGHT tall.
        let bg_phys_size = Size::<i32, Physical>::from((
            (output_size.w as f64 * scale).round() as i32,
            (BAR_HEIGHT as f64 * scale).round() as i32,
        ));
        solids.push(SolidColorRenderElement::new(
            self.bg_id.clone(),
            Rectangle::new(Point::<i32, Physical>::from((0, 0)), bg_phys_size),
            self.bg_commit,
            BAR_BG,
            Kind::Unspecified,
        ));

        // Bottom separator line — 1px logical.
        let sep_y = ((BAR_HEIGHT - 1) as f64 * scale).round() as i32;
        let sep_phys_size =
            Size::<i32, Physical>::from((bg_phys_size.w, (1.0 * scale).round().max(1.0) as i32));
        solids.push(SolidColorRenderElement::new(
            self.sep_id.clone(),
            Rectangle::new(Point::<i32, Physical>::from((0, sep_y)), sep_phys_size),
            self.sep_commit,
            SEP_COLOR,
            Kind::Unspecified,
        ));

        // Button labels (pill or text-only).
        for btn in &self.buttons {
            let r = btn.hit_rect;
            let buf_y = (BAR_HEIGHT as f64 - btn.buf_size.1 as f64) / 2.0;
            let loc = Point::<f64, Logical>::from((r.loc.x as f64, buf_y)).to_physical(s);

            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                loc,
                &btn.label_buf,
                None,
                None,
                None,
                Kind::Unspecified,
            ) {
                labels.push(elem);
            }
        }

        // Center title.
        if self.title_size.0 > 0 {
            let title_x = (output_size.w as f64 - self.title_size.0 as f64) / 2.0;
            let title_y = (BAR_HEIGHT as f64 - self.title_size.1 as f64) / 2.0;
            let loc = Point::<f64, Logical>::from((title_x, title_y)).to_physical(s);

            if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                loc,
                &self.title_buf,
                None,
                None,
                None,
                Kind::Unspecified,
            ) {
                labels.push(elem);
            }
        }

        (solids, labels)
    }
}

/// Shape text with cosmic_text and measure its bounding box.
/// Returns (configured buffer, text_width, text_height) in logical pixels.
fn shape_and_measure(
    font_system: &mut FontSystem,
    text: &str,
    font_size: f32,
    line_height: f32,
) -> (CtBuffer, i32, i32) {
    let metrics = Metrics::new(font_size, line_height);
    let mut ct_buffer = CtBuffer::new(font_system, metrics);
    ct_buffer.set_size(font_system, Some(f32::INFINITY), Some(f32::INFINITY));
    ct_buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(Family::SansSerif),
        Shaping::Advanced,
        None,
    );
    ct_buffer.shape_until_scroll(font_system, false);

    let mut text_w = 0.0f32;
    let mut text_h = 0.0f32;
    for run in ct_buffer.layout_runs() {
        text_w = text_w.max(run.line_w);
        text_h = text_h.max(run.line_top + run.line_height);
    }
    (ct_buffer, text_w.ceil() as i32, text_h.ceil() as i32)
}

/// Render a workspace button into a MemoryRenderBuffer.
/// Active: rounded pill background + dark text.
/// Inactive: transparent background + gray text.
/// Returns (w, h) in logical pixels.
fn render_pill_button(
    buf: &mut MemoryRenderBuffer,
    text: &str,
    active: bool,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) -> (i32, i32) {
    let (mut ct_buffer, text_w, text_h) =
        shape_and_measure(font_system, text, FONT_SIZE, LINE_HEIGHT);

    let buf_h = PILL_H;
    let text_offset_y = (PILL_H - text_h) / 2;
    let (buf_w, text_offset_x) = if active {
        ((text_w + PILL_PAD_H * 2).max(1), PILL_PAD_H)
    } else {
        ((text_w + INACTIVE_PAD_H * 2).max(1), INACTIVE_PAD_H)
    };

    let fg = if active { ACTIVE_FG } else { INACTIVE_FG };

    let buf_size: Size<i32, SBuffer> = (buf_w, buf_h).into();
    effect_core::paint_buffer(buf, buf_size, |data| {
        data.fill(0);
        if active {
            draw_rounded_rect(data, buf_w, buf_h, PILL_RADIUS, &PILL_BG);
        }

        draw_text_onto(
            data,
            buf_w,
            buf_h,
            text_offset_x,
            text_offset_y,
            &fg,
            &mut ct_buffer,
            font_system,
            swash_cache,
        );
    });

    (buf_w, buf_h)
}

/// Render a plain text label (no background) into a MemoryRenderBuffer.
/// Returns (w, h) in logical pixels.
fn render_text_label(
    buf: &mut MemoryRenderBuffer,
    text: &str,
    fg: &[u8; 4],
    font_size: f32,
    line_height: f32,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) -> (i32, i32) {
    let (mut ct_buffer, text_w, text_h) =
        shape_and_measure(font_system, text, font_size, line_height);

    let buf_w = (text_w + TEXT_LABEL_PAD * 2).max(1);
    let buf_h = (text_h + TEXT_LABEL_PAD * 2).max(1);

    let buf_size: Size<i32, SBuffer> = (buf_w, buf_h).into();
    effect_core::paint_buffer(buf, buf_size, |data| {
        data.fill(0);
        draw_text_onto(
            data,
            buf_w,
            buf_h,
            TEXT_LABEL_PAD,
            TEXT_LABEL_PAD,
            fg,
            &mut ct_buffer,
            font_system,
            swash_cache,
        );
    });

    (buf_w, buf_h)
}

use effect_core::draw_text_onto;

/// Draw a filled rounded rectangle with anti-aliased edges into BGRA pixel data.
fn draw_rounded_rect(data: &mut [u8], w: i32, h: i32, radius: f32, color: &[u8; 4]) {
    let stride = w * 4;

    for py in 0..h {
        for px in 0..w {
            let coverage = rounded_rect_coverage(px as f32, py as f32, w as f32, h as f32, radius);
            if coverage <= 0.0 {
                continue;
            }
            let off = (py * stride + px * 4) as usize;
            if coverage >= 1.0 {
                data[off] = color[0];
                data[off + 1] = color[1];
                data[off + 2] = color[2];
                data[off + 3] = color[3];
            } else {
                // Straight alpha: scale only alpha by coverage, keep RGB intact.
                data[off] = color[0];
                data[off + 1] = color[1];
                data[off + 2] = color[2];
                data[off + 3] = (color[3] as f32 * coverage) as u8;
            }
        }
    }
}

/// Compute pixel coverage for a rounded rectangle using SDF. Returns 0.0..1.0.
fn rounded_rect_coverage(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let cx = px + 0.5;
    let cy = py + 0.5;
    let hw = w / 2.0;
    let hh = h / 2.0;
    let dx = (cx - hw).abs() - (hw - r);
    let dy = (cy - hh).abs() - (hh - r);
    let outside_dist = (dx.max(0.0) * dx.max(0.0) + dy.max(0.0) * dy.max(0.0)).sqrt();
    let inside_dist = dx.max(dy).min(0.0);
    let dist = outside_dist + inside_dist - r;
    (0.5 - dist).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Effect impl
// ---------------------------------------------------------------------------

impl WorkspaceBar {
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl effect_core::Effect for WorkspaceBar {
    fn name(&self) -> &'static str {
        "workspace_bar"
    }

    fn is_active(&self) -> bool {
        // Not `enabled && visible()`: `visible()` depends on `buttons` being
        // populated by `update()` — gating paint on visibility would be a
        // deadlock only if update were driven through the trait. Since
        // `update()` is now called by the host (emskin) directly, either
        // form works; keep `enabled` to avoid invisible pre-paint work.
        self.enabled
    }

    fn chain_position(&self) -> u8 {
        90
    }

    fn paint(
        &mut self,
        renderer: &mut GlesRenderer,
        ctx: &effect_core::EffectCtx,
    ) -> Vec<effect_core::CustomElement<GlesRenderer>> {
        use effect_core::CustomElement;

        let (solids, labels) = self.build_elements(renderer, ctx.output_size, ctx.scale);

        // Intra-effect z-order: labels (pill text) → solids (pill bg + bar bg).
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
