# emskin - Nested Wayland Compositor for Emacs

## Build
- `cargo check` / `cargo clippy -- -D warnings` / `cargo fmt`
- smithay: forked at `loyalpartner/smithay` branch `emskin-patches`. Upstream: `Smithay/smithay`
- smithay patches: `backend/winit/mod.rs` (expose `WinitEvent::Ime`, 8-bit pixel format priority), `text_input/text_input_handle.rs` (remove `has_instance()` guard, add `cursor_rectangle` accessor), `selection/seat_data.rs` (fix GTK3 clipboard on focus change)

## Architecture
- Nested Wayland compositor using smithay, hosting Emacs inside a winit window
- First toplevel = Emacs (fullscreen), subsequent toplevels = **arbitrary embedded programs** (any Wayland or XWayland client) managed by AppManager. Not limited to EAF — any GTK/Qt/Electron/X11 app can be embedded as a child window whose geometry is controlled by Emacs via IPC.
- IPC protocol: length-prefixed JSON over Unix socket. Emacs→compositor: set_geometry, close, set_visibility, prefix_done, set_focus, set_crosshair, add_mirror, update_mirror_geometry, remove_mirror, promote_mirror, request_activation_token. Compositor→Emacs: connected, surface_size, window_created, window_destroyed, title_changed, focus_view, activation_token, xwayland_ready
- Elisp client: `elisp/emskin.el` — auto-connects via parent PID socket discovery, syncs geometry on `window-size-change-functions` with change-detection guard
- Mirror system: same embedded program displays in multiple Emacs windows. Source = first window (real surface), mirrors = subsequent windows (TextureRenderElement from same GPU texture). Elisp tracks source/mirror in `emskin--mirror-table`
- Keyboard input: compositor detects Emacs prefix keys (C-x, C-c, M-x) via `input_intercept`, redirects focus to Emacs; `prefix_done` IPC restores focus. `set_focus` IPC for explicit focus control. Prefix state: `Option<Option<WlSurface>>` (outer None = inactive)
- IME input: text_input_v3 bridges host IME (via winit Ime events) to embedded Wayland clients (Chrome). GTK/Qt apps use their own IM module (fcitx5-gtk via DBus) and are unaffected — `set_ime_allowed` is only enabled when the focused client has bound text_input_v3. Smithay patches required: expose `WinitEvent::Ime`, remove `has_instance()` guard in text_input dispatch, add `cursor_rectangle()` accessor
- `AppWindow::wl_surface()` returns primary WlSurface (Wayland toplevel or X11 fallback)
- grabs/ directory is placeholder code for future move/resize support

## Key Gotchas
- smithay winit backend defaults to 10-10-10-2 pixel format (2-bit alpha) — breaks GTK semi-transparent UI. Fixed by prioritizing 8-bit in smithay's `backend/winit/mod.rs`
- winit `scale_factor()` returns 1.0 at init time; real scale arrives later via `ScaleFactorChanged` → `WinitEvent::Resized { scale_factor }`
- Use `Scale::Fractional(scale_factor)` not `Scale::Integer(ceil)` to match host compositor's actual DPI
- `render_scale` in `render_output()` should be 1.0 (smallvil pattern); smithay handles client buffer_scale internally
- `Transform::Flipped180` is required for correct orientation with the winit EGL backend
- Use smithay's type-safe geometry: `size.to_f64().to_logical(scale).to_i32_round()` instead of manual arithmetic
- GTK3 Emacs does NOT support xdg-decoration protocol — setting `Fullscreen` state on the toplevel is what actually hides CSD titlebar/borders
- GTK4/GTK3 will send `unmaximize_request`/`unfullscreen_request` immediately on connect if those states are set in initial configure — must ignore these for single-window compositor
- Host keyboard layout: smithay winit backend does NOT expose the host's keymap. Use `wayland-client` to separately connect, receive `wl_keyboard.keymap`, then `KeyboardHandle::set_keymap_from_string()` — env vars (`XKB_DEFAULT_*`) are unreliable on KDE Wayland
- pgtk Emacs: `frame-geometry` returns 0 for `menu-bar-size` (GTK external menu-bar architectural limitation, not a bug). Compute exact bar height via compositor IPC: `offset = surface_height - frame-pixel-height`
- `window-pixel-edges` is relative to native frame (excludes external menu-bar/toolbar); `window-body-pixel-edges` bottom = top of mode-line
- embedded app windows must be mapped to space at 1×1 in `new_toplevel` (otherwise on_commit and initial configure don't fire); actual size arrives via `set_geometry` IPC
- Host resize must only resize the Emacs surface; embedded app window sizes are controlled by Emacs via IPC
- Mirror rendering: `TextureRenderElement` position is Physical coords — must use `output.current_scale().fractional_scale()` for logical→physical conversion, NOT hardcode 1.0
- Mirror rendering must walk the full `wl_subsurface` tree via `with_surface_tree_downward` — GTK/Firefox paint content onto subsurface children, so reading only the root surface yields an empty mirror
- Mirror rendering: call `import_surface_tree` once per layer, then walk each layer's subsurface tree *once* (not per mirror) and scale the collected snapshots — avoids O(mirrors × tree) traversals in the render hot path
- Mirror element Id must be `Id::from_wayland_resource(surface).namespaced(view_id as usize)` — same surface in different mirrors needs distinct Ids or the damage tracker collapses them. `render_elements_from_surface_tree` cannot replace the manual walk because its Id is hardcoded to `from_wayland_resource(surface)` with no namespace hook
- Mirror rendering must subtract `window.geometry().loc` (and `popup.geometry().loc` for popups) from the render origin — GTK/Chrome put CSD shadow padding in the buffer and use `xdg_surface.set_window_geometry` to mark where the visible window actually starts. Smithay's `Space::render_location()` does `space_loc - element.geometry().loc` automatically; custom mirror paths must match or visible content gets pushed inward by the shadow amount. Precompute this into `SurfaceLayer::render_offset` (popup offset minus geometry offset) so per-layer walks don't redo the math
- Mirror rendering: `TextureRenderElement` needs `buffer_scale`, `buffer_transform`, and viewport `src` from `RendererSurfaceState` — otherwise size is wrong under fractional scaling
- Mirror input: `surface_under()` must check mirrors BEFORE space — Emacs is fullscreen and `element_under()` always hits it first, blocking mirror detection
- Mirror input: pointer `under_position` for mirrors needs offset compensation (`pos - mapped_pos`) so smithay computes correct surface-local coords
- Mirror scaling: aspect-fit with top-left alignment; coordinate mapping in `mirror_under` uses `rel.downscale(ratio)` to map mirror→source; `AppManager::aspect_fit_ratio()` returns None for zero-size to prevent NaN
- `render_output`'s second type param is the custom_elements type (not space element type); `render_scale` (value 1.0) is actually the `alpha` parameter
- `render_elements!` macro cannot parse associated-type bounds (`Renderer<TextureId = GlesTexture>`) — define a blanket helper trait as workaround
- Custom overlays: `SolidColorRenderElement` for shapes, `MemoryRenderBuffer` + bitmap font for text. `CommitCounter` must be stored in struct and incremented on change — `default()` every frame defeats damage tracking
- Elisp `defcustom` with `:set` that references later-defined vars: use `:initialize #'custom-initialize-default` + `bound-and-true-p` to avoid void-variable at load time
- IME: registering `TextInputManagerState` causes fcitx5-gtk to switch from DBus to text_input_v3 path — must dynamically toggle `set_ime_allowed` per focused client (only enable when client has bound text_input_v3, check via `with_focused_text_input`)
- IME: smithay's keyboard.rs gates `text_input.enter()/leave()` behind `input_method.has_instance()` — without input_method, must manually call enter/leave in `SeatHandler::focus_changed` with temporary focus swap to send leave to the correct old client
- IME: `pending_ime_allowed: Option<bool>` pattern — `focus_changed` cannot access winit backend, so deferred to `apply_pending_state` (same pattern as `pending_fullscreen`/`pending_maximize`)
- Elisp: use `window-body-pixel-edges` for embedded app geometry (excludes fringes/margins/header-line/mode-line). Set buffer-local `left-fringe-width`, `right-fringe-width`, `left-margin-width`, `right-margin-width` to 0 and `cursor-type` to nil for EAF buffers
- Elisp: `set-window-scroll-bars` is non-persistent across buffer switches — re-apply in `emskin--sync-all` with `window-scroll-bars` change-detection guard
- Elisp skeleton: guard bar height fallback with `frame-parameter 'menu-bar-lines/tool-bar-lines/tab-bar-lines` — pgtk fallback incorrectly derives non-zero heights for disabled bars without this check

## Wayland Protocols Implemented
- xdg_shell (toplevel, popup)
- xdg-decoration (force ServerSide — no decorations drawn)
- wl_seat (keyboard + pointer)
- wl_data_device (DnD)
- fractional_scale, viewporter
- text_input_v3 (IME bridge to host — see smithay fork patches)
- linux-dmabuf (GPU buffer sharing for hardware-accelerated clients)
