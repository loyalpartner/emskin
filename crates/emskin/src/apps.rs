use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    desktop::{PopupManager, Window},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Point, Rectangle},
    wayland::seat::WaylandFocus,
};

/// A mirror view of an embedded app, possibly in a different workspace than
/// its source. The source texture is accessed via WlSurface directly (not
/// through Space), so cross-workspace mirrors work naturally.
pub struct MirrorView {
    pub geometry: Rectangle<i32, Logical>,
    /// Which workspace this mirror is displayed in.
    pub workspace_id: u64,
}

/// An embedded application window.
pub struct AppWindow {
    pub window_id: u64,
    pub window: Window,
    /// Which workspace this app's source surface belongs to.
    pub workspace_id: u64,
    /// Committed geometry (logical px) — currently used for rendering.
    pub geometry: Option<Rectangle<i32, Logical>>,
    /// Pending geometry awaiting the client's next buffer commit.
    pub pending_geometry: Option<Rectangle<i32, Logical>>,
    /// When `pending_geometry` was set (for timeout-based force-commit).
    pub pending_since: Option<Instant>,
    pub visible: bool,
    /// Mirror views: view_id → MirrorView. Each entry is a scaled copy of the
    /// source surface, positioned at the given rectangle. Mirrors can be in a
    /// different workspace than the source.
    pub mirrors: HashMap<u64, MirrorView>,
}

/// A renderable surface layer — toplevel or popup.
pub struct SurfaceLayer {
    pub surface: WlSurface,
    /// Where to render this layer's buffer origin, relative to the toplevel.
    /// Matches smithay's `Space::render_location()`: popup position minus the
    /// `xdg_surface.set_window_geometry` offset. The subtraction cancels GTK
    /// CSD shadow padding baked into the buffer so the visible window lands
    /// at the intended top-left.
    pub render_offset: Point<i32, Logical>,
}

impl AppWindow {
    /// Get the primary WlSurface (Wayland toplevel or X11).
    pub fn wl_surface(&self) -> Option<WlSurface> {
        self.window
            .toplevel()
            .map(|t| t.wl_surface().clone())
            .or_else(|| self.window.x11_surface().and_then(|x| x.wl_surface()))
    }

    /// Collect the full surface stack: toplevel (offset=0,0) + all popups (recursive).
    /// For Wayland windows this includes xdg popups; for X11 windows just the surface.
    pub fn surface_layers(&self) -> Vec<SurfaceLayer> {
        if let Some(toplevel) = self.window.toplevel() {
            let wl = toplevel.wl_surface();
            let wg = self.window.geometry().loc;
            let mut layers = vec![SurfaceLayer {
                surface: wl.clone(),
                render_offset: (-wg.x, -wg.y).into(),
            }];
            for (popup, offset) in PopupManager::popups_for_surface(wl) {
                let pg = popup.geometry().loc;
                layers.push(SurfaceLayer {
                    surface: popup.wl_surface().clone(),
                    render_offset: (offset.x - pg.x, offset.y - pg.y).into(),
                });
            }
            layers
        } else if let Some(x11) = self.window.x11_surface() {
            x11.wl_surface()
                .map(|s| {
                    vec![SurfaceLayer {
                        surface: s,
                        render_offset: (0, 0).into(),
                    }]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

/// Tracks all live embedded application windows.
#[derive(Default)]
pub struct AppManager {
    windows: HashMap<u64, AppWindow>,
    next_id: u64,
}

impl AppManager {
    pub fn alloc_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn insert(&mut self, app: AppWindow) {
        self.windows.insert(app.window_id, app);
    }

    pub fn remove(&mut self, window_id: u64) -> Option<AppWindow> {
        self.windows.remove(&window_id)
    }

    pub fn get(&self, window_id: u64) -> Option<&AppWindow> {
        self.windows.get(&window_id)
    }

    pub fn get_mut(&mut self, window_id: u64) -> Option<&mut AppWindow> {
        self.windows.get_mut(&window_id)
    }

    pub fn windows(&self) -> impl Iterator<Item = &AppWindow> {
        self.windows.values()
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut AppWindow> {
        self.windows.values_mut()
    }

    /// Find the window_id for a given Wayland surface (works for both Wayland and X11 windows).
    pub fn id_for_surface(&self, wl: &WlSurface) -> Option<u64> {
        self.windows
            .values()
            .find(|w| w.window.wl_surface().map(|s| &*s == wl).unwrap_or(false))
            .map(|w| w.window_id)
    }

    /// Find a mutable reference to the AppWindow for a given Wayland surface.
    pub fn get_mut_by_surface(&mut self, wl: &WlSurface) -> Option<&mut AppWindow> {
        self.windows
            .values_mut()
            .find(|w| w.window.wl_surface().map(|s| &*s == wl).unwrap_or(false))
    }

    /// Collect embedded app windows whose pending geometry has timed out.
    /// Returns (window_id, window, geo) for each; caller must `map_element`.
    pub fn collect_timed_out(
        &mut self,
        timeout: Duration,
    ) -> Vec<(u64, Window, Rectangle<i32, Logical>)> {
        let mut result = Vec::new();
        for app in self.windows.values_mut() {
            if let (Some(since), Some(pending)) = (app.pending_since, app.pending_geometry) {
                if since.elapsed() > timeout {
                    app.geometry = Some(pending);
                    app.pending_geometry = None;
                    app.pending_since = None;
                    result.push((app.window_id, app.window.clone(), pending));
                }
            }
        }
        result
    }

    /// Test if `pos` hits a mirror region and map to source surface coordinates.
    /// Returns mapped point if hit, None otherwise.
    pub fn mirror_hit_test(
        pos: smithay::utils::Point<f64, Logical>,
        source_geo: Rectangle<i32, Logical>,
        mirror_geo: Rectangle<i32, Logical>,
    ) -> Option<smithay::utils::Point<f64, Logical>> {
        let src_size = source_geo.size.to_f64();
        let m = mirror_geo.to_f64();
        let ratio = Self::aspect_fit_ratio(src_size, m.size)?;
        let fit: smithay::utils::Size<f64, Logical> =
            (src_size.w * ratio, src_size.h * ratio).into();
        let rel = pos - m.loc;
        if rel.x < 0.0 || rel.y < 0.0 || rel.x >= fit.w || rel.y >= fit.h {
            return None;
        }
        Some(source_geo.loc.to_f64() + rel.downscale(ratio))
    }

    /// Compute the aspect-fit scale ratio for rendering `src_size` inside `dst_size`.
    /// Returns `None` if either dimension is zero.
    pub fn aspect_fit_ratio(
        src: smithay::utils::Size<f64, Logical>,
        dst: smithay::utils::Size<f64, Logical>,
    ) -> Option<f64> {
        if src.w <= 0.0 || src.h <= 0.0 || dst.w <= 0.0 || dst.h <= 0.0 {
            return None;
        }
        Some((dst.w / src.w).min(dst.h / src.h))
    }

    /// Check if `pos` falls inside any mirror in the given workspace.
    /// Returns (window_id, view_id, mapped surface coordinate) with proportional mapping.
    /// Only checks mirrors whose `workspace_id` matches `active_workspace_id`.
    pub fn mirror_under(
        &self,
        pos: smithay::utils::Point<f64, Logical>,
        active_workspace_id: u64,
    ) -> Option<(u64, u64, smithay::utils::Point<f64, Logical>)> {
        for app in self.windows.values() {
            let Some(source_geo) = app.geometry else {
                continue;
            };
            for (&view_id, mirror) in &app.mirrors {
                if mirror.workspace_id != active_workspace_id {
                    continue;
                }
                if let Some(mapped) = Self::mirror_hit_test(pos, source_geo, mirror.geometry) {
                    return Some((app.window_id, view_id, mapped));
                }
            }
        }
        None
    }

    /// Remove and return all windows whose Wayland surface has been destroyed.
    pub fn drain_dead(&mut self) -> Vec<AppWindow> {
        let dead_ids: Vec<u64> = self
            .windows
            .iter()
            .filter(|(_, w)| !w.window.alive())
            .map(|(id, _)| *id)
            .collect();
        dead_ids
            .into_iter()
            .filter_map(|id| self.windows.remove(&id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::Size;

    #[test]
    fn aspect_fit_ratio_returns_none_for_zero_src_width() {
        let src: Size<f64, Logical> = (0.0, 100.0).into();
        let dst: Size<f64, Logical> = (200.0, 200.0).into();
        assert!(AppManager::aspect_fit_ratio(src, dst).is_none());
    }

    #[test]
    fn aspect_fit_ratio_returns_none_for_zero_src_height() {
        let src: Size<f64, Logical> = (100.0, 0.0).into();
        let dst: Size<f64, Logical> = (200.0, 200.0).into();
        assert!(AppManager::aspect_fit_ratio(src, dst).is_none());
    }

    #[test]
    fn aspect_fit_ratio_returns_none_for_zero_dst_width() {
        let src: Size<f64, Logical> = (100.0, 100.0).into();
        let dst: Size<f64, Logical> = (0.0, 200.0).into();
        assert!(AppManager::aspect_fit_ratio(src, dst).is_none());
    }

    #[test]
    fn aspect_fit_ratio_returns_none_for_zero_dst_height() {
        let src: Size<f64, Logical> = (100.0, 100.0).into();
        let dst: Size<f64, Logical> = (200.0, 0.0).into();
        assert!(AppManager::aspect_fit_ratio(src, dst).is_none());
    }

    #[test]
    fn aspect_fit_ratio_equal_sizes_returns_one() {
        let size: Size<f64, Logical> = (100.0, 100.0).into();
        let ratio = AppManager::aspect_fit_ratio(size, size).unwrap();
        assert!((ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn aspect_fit_ratio_landscape_src_in_square_dst() {
        // 200x100 into 100x100 → scale by 0.5 (width-limited)
        let src: Size<f64, Logical> = (200.0, 100.0).into();
        let dst: Size<f64, Logical> = (100.0, 100.0).into();
        let ratio = AppManager::aspect_fit_ratio(src, dst).unwrap();
        assert!((ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn aspect_fit_ratio_portrait_src_in_square_dst() {
        // 100x200 into 100x100 → scale by 0.5 (height-limited)
        let src: Size<f64, Logical> = (100.0, 200.0).into();
        let dst: Size<f64, Logical> = (100.0, 100.0).into();
        let ratio = AppManager::aspect_fit_ratio(src, dst).unwrap();
        assert!((ratio - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn aspect_fit_ratio_dst_larger_than_src() {
        // 100x100 into 400x200 → scale by 2.0 (height-limited)
        let src: Size<f64, Logical> = (100.0, 100.0).into();
        let dst: Size<f64, Logical> = (400.0, 200.0).into();
        let ratio = AppManager::aspect_fit_ratio(src, dst).unwrap();
        assert!((ratio - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn alloc_id_returns_sequential_ids() {
        let mut mgr = AppManager::default();
        let id1 = mgr.alloc_id();
        let id2 = mgr.alloc_id();
        let id3 = mgr.alloc_id();
        assert_eq!(id2, id1 + 1);
        assert_eq!(id3, id2 + 1);
    }

    // --- mirror_hit_test ---

    use smithay::utils::{Point, Rectangle};

    fn src_geo(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    #[test]
    fn mirror_hit_test_center_of_equal_size_mirror() {
        // Source: 100x100 at (0,0). Mirror: 100x100 at (200,200).
        // Click at center of mirror (250, 250) → maps to source (50, 50).
        let source = src_geo(0, 0, 100, 100);
        let mirror = src_geo(200, 200, 100, 100);
        let pos: Point<f64, Logical> = (250.0, 250.0).into();
        let mapped = AppManager::mirror_hit_test(pos, source, mirror).unwrap();
        assert!((mapped.x - 50.0).abs() < 0.01);
        assert!((mapped.y - 50.0).abs() < 0.01);
    }

    #[test]
    fn mirror_hit_test_scaled_down_mirror() {
        // Source: 200x100 at (10,20). Mirror: 100x50 at (0,0) (ratio=0.5).
        // Click at (50, 25) → rel=(50,25) → downscale by 0.5 → (100,50) → + source.loc → (110,70).
        let source = src_geo(10, 20, 200, 100);
        let mirror = src_geo(0, 0, 100, 50);
        let pos: Point<f64, Logical> = (50.0, 25.0).into();
        let mapped = AppManager::mirror_hit_test(pos, source, mirror).unwrap();
        assert!((mapped.x - 110.0).abs() < 0.01);
        assert!((mapped.y - 70.0).abs() < 0.01);
    }

    #[test]
    fn mirror_hit_test_miss_outside_left() {
        let source = src_geo(0, 0, 100, 100);
        let mirror = src_geo(100, 100, 100, 100);
        let pos: Point<f64, Logical> = (99.0, 150.0).into(); // left of mirror
        assert!(AppManager::mirror_hit_test(pos, source, mirror).is_none());
    }

    #[test]
    fn mirror_hit_test_miss_outside_bottom() {
        let source = src_geo(0, 0, 100, 100);
        let mirror = src_geo(100, 100, 100, 100);
        let pos: Point<f64, Logical> = (150.0, 200.0).into(); // at bottom edge (exclusive)
        assert!(AppManager::mirror_hit_test(pos, source, mirror).is_none());
    }

    #[test]
    fn mirror_hit_test_hit_at_origin() {
        let source = src_geo(0, 0, 100, 100);
        let mirror = src_geo(50, 50, 100, 100);
        let pos: Point<f64, Logical> = (50.0, 50.0).into(); // top-left of mirror
        let mapped = AppManager::mirror_hit_test(pos, source, mirror).unwrap();
        assert!((mapped.x - 0.0).abs() < 0.01);
        assert!((mapped.y - 0.0).abs() < 0.01);
    }

    #[test]
    fn mirror_hit_test_aspect_fit_letterbox() {
        // Source: 200x100. Mirror: 100x100 (wider than needed).
        // Aspect fit: ratio = min(100/200, 100/100) = 0.5.
        // Fitted size: 100x50 (letterboxed, top-left aligned).
        // Click at (50, 25) → rel=(50,25), within (100,50) → mapped = (0,0)+(100,50) = (100,50).
        let source = src_geo(0, 0, 200, 100);
        let mirror = src_geo(0, 0, 100, 100);
        let pos: Point<f64, Logical> = (50.0, 25.0).into();
        let mapped = AppManager::mirror_hit_test(pos, source, mirror).unwrap();
        assert!((mapped.x - 100.0).abs() < 0.01);
        assert!((mapped.y - 50.0).abs() < 0.01);
    }

    #[test]
    fn mirror_hit_test_miss_in_letterbox_area() {
        // Source: 200x100. Mirror: 100x100. Fitted: 100x50.
        // Click at (50, 60) → rel.y=60 >= fit.h=50 → miss (in letterbox padding).
        let source = src_geo(0, 0, 200, 100);
        let mirror = src_geo(0, 0, 100, 100);
        let pos: Point<f64, Logical> = (50.0, 60.0).into();
        assert!(AppManager::mirror_hit_test(pos, source, mirror).is_none());
    }

    #[test]
    fn mirror_hit_test_zero_size_source_returns_none() {
        let source = src_geo(0, 0, 0, 100);
        let mirror = src_geo(0, 0, 100, 100);
        let pos: Point<f64, Logical> = (50.0, 50.0).into();
        assert!(AppManager::mirror_hit_test(pos, source, mirror).is_none());
    }
}
