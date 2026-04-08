use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use smithay::{
    desktop::Window,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Rectangle},
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

    /// Find the window_id for a given Wayland surface.
    pub fn id_for_surface(&self, wl: &WlSurface) -> Option<u64> {
        self.windows
            .values()
            .find(|w| w.window.toplevel().is_some_and(|t| t.wl_surface() == wl))
            .map(|w| w.window_id)
    }

    /// Find a mutable reference to the AppWindow for a given Wayland surface.
    pub fn get_mut_by_surface(&mut self, wl: &WlSurface) -> Option<&mut AppWindow> {
        self.windows
            .values_mut()
            .find(|w| w.window.toplevel().is_some_and(|t| t.wl_surface() == wl))
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
