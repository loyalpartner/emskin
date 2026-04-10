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

/// An embedded EAF application window.
pub struct AppWindow {
    pub window_id: u64,
    pub window: Window,
    /// Committed geometry (logical px) — currently used for rendering.
    pub geometry: Option<Rectangle<i32, Logical>>,
    /// Pending geometry awaiting the client's next buffer commit.
    pub pending_geometry: Option<Rectangle<i32, Logical>>,
    /// When `pending_geometry` was set (for timeout-based force-commit).
    pub pending_since: Option<Instant>,
    pub visible: bool,
    /// Mirror views: view_id → (geometry, stable render element ID).
    /// Each mirror displays a scaled copy of the source surface.
    pub mirrors: HashMap<u64, MirrorView>,
}

/// A mirror view of an EAF app window.
pub struct MirrorView {
    pub geometry: Rectangle<i32, Logical>,
    pub render_id: smithay::backend::renderer::element::Id,
    /// Stable render IDs for popup layers (index = popup layer index).
    /// Grown on demand, never shrunk — avoids per-frame Id::new() allocation.
    pub popup_render_ids: Vec<smithay::backend::renderer::element::Id>,
}

/// A renderable surface layer — toplevel or popup — with its offset relative to the toplevel origin.
pub struct SurfaceLayer {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
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
            let mut layers = vec![SurfaceLayer {
                surface: wl.clone(),
                offset: (0, 0).into(),
            }];
            for (popup, offset) in PopupManager::popups_for_surface(wl) {
                layers.push(SurfaceLayer {
                    surface: popup.wl_surface().clone(),
                    offset,
                });
            }
            layers
        } else if let Some(x11) = self.window.x11_surface() {
            x11.wl_surface()
                .map(|s| {
                    vec![SurfaceLayer {
                        surface: s,
                        offset: (0, 0).into(),
                    }]
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }
}

/// Tracks all live EAF application windows.
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

    /// Collect EAF app windows whose pending geometry has timed out.
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

    /// Check if `pos` falls inside any mirror of any app window.
    /// Returns (window_id, view_id, mapped surface coordinate) with proportional mapping.
    pub fn mirror_under(
        &self,
        pos: smithay::utils::Point<f64, Logical>,
    ) -> Option<(u64, u64, smithay::utils::Point<f64, Logical>)> {
        for app in self.windows.values() {
            let Some(source_geo) = app.geometry else {
                continue;
            };
            let src_size = source_geo.size.to_f64();

            for (&view_id, mv) in &app.mirrors {
                let m = mv.geometry.to_f64();
                let Some(ratio) = Self::aspect_fit_ratio(src_size, m.size) else {
                    continue;
                };
                let fit: smithay::utils::Size<f64, Logical> =
                    (src_size.w * ratio, src_size.h * ratio).into();
                let rel = pos - m.loc;

                // Only respond within the rendered content area (top-left aligned).
                if rel.x < 0.0 || rel.y < 0.0 || rel.x >= fit.w || rel.y >= fit.h {
                    continue;
                }

                let mapped = source_geo.loc.to_f64() + rel.downscale(ratio);
                return Some((app.window_id, view_id, mapped));
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
