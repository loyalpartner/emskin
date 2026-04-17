# effect-plugins

Built-in visual overlays for emskin. Depends only on `effect-core` + external crates (`smithay`, `cosmic-text`, `serde`, `serde_json`, `tracing`). **Cannot depend on `emskin`.**

## Overview

Each plugin struct has two faces:
1. **`impl Effect`** — purely visual per-frame lifecycle
2. **Typed `pub` methods** — state control, hit-testing, one-shot triggers called by the host

The host (emskin) keeps an `Rc<RefCell<T>>` handle, registers the same instance into the effect chain via `EffectHandle::new(handle.clone())`, and drives state via the typed methods.

## Plugins

### `measure` — chain_position 80

Figma-style pixel inspector: crosshair + rulers following the cursor.

- `pub fn set_enabled(bool)` — toggle (called from `IncomingMessage::SetMeasure`)
- `impl Effect` reads `ctx.cursor_pos` for the crosshair position

### `skeleton` — chain_position 85

Frame-layout debug overlay. Wireframe rectangles + labels stacked in a right-side panel; clicking a label flashes the target rect.

- `pub fn set_enabled(bool)` — toggle (also resets click_absorbed when disabling)
- `pub fn set_rects(Vec<SkeletonRect>)` — replace rect list
- `pub fn clear()` — drop all rects
- `pub fn click_at(pos) -> Option<SkeletonHit>` — hit-test + flash, called from emskin's `input.rs`
- `pub fn enabled() -> bool`

**`SkeletonRect`** is defined here and re-exported from `emskin::ipc` via `pub use effect_plugins::skeleton::SkeletonRect`. Internal rect is `smithay::utils::Rectangle<i32, Logical>`; wire format round-trips through a private flat `{kind, label, x, y, w, h, selected}` JSON struct to preserve the wire compatibility with the Emacs client.

### `splash` — chain_position 95

Startup animation (letters + underline bar). Dismissed when Emacs connects.

- `pub fn dismiss()` — triggers fade-out (called from emskin's winit.rs as an edge-detect when `emacs_surface` appears)
- `impl Effect::post_paint` returns `!done` to drive continuous redraws during animation
- `should_remove = done` — chain drops the plugin after animation finishes

### `cursor_trail` — chain_position 75

Elastic trailing animation: 10 spring-damped nodes follow the cursor in a chain. Stretches on fast movement, bounces back when stopped.

- `pub fn set_enabled(bool)` — toggle (called from `IncomingMessage::SetCursorTrail`)
- `impl Effect` reads `ctx.cursor_pos` + `ctx.present_time` to step physics; `post_paint` returns `!settled` to keep redrawing during settle

### `jelly_cursor` — chain_position 77

Port of holo-layer's `jelly` text-cursor animation. When Emacs's caret moves, a filled quadrilateral stretches from the previous caret rect to the new one over 200 ms, then collapses into the new rect (two-phase deformation around `p = 0.5`). Scanline-fills the polygon into a bounding-box-sized `MemoryRenderBuffer` each frame with a linear gradient (lightened tail → solid head).

- `pub fn set_enabled(bool)` — toggle
- `pub fn update(rect: Option<Rectangle<i32, Logical>>, now: Duration)` — host hands in the current caret rect in **canvas** coordinates:
  - `None` → cancel animation, forget last rect (so re-entry re-primes)
  - first `Some` after a `None` → prime, no animation
  - `Some` equal to last → no-op
  - `Some` different → seed a new animation from the previous rect
- Data source: `zwp_text_input_v3.set_cursor_rectangle`. pgtk Emacs reports this on every caret move via GTK's IM framework — **pgtk-only**. GTK3 Emacs (XWayland) has no Wayland-side caret signal.
- Host (emskin) drives this from `EmskinState::sync_jelly_caret()` called once per frame. It polls `seat.text_input().focus()` + `cursor_rectangle()`, filters for the active Emacs surface, and resets on focus boundary transitions (app → Emacs, Emacs → app) so the animation never spans two surfaces.

The workspace bar used to live here. It was extracted into a standalone Wayland client (`crates/emskin-bar/`) that talks to the compositor via `zwlr-layer-shell-v1` + `ext-workspace-v1` — effect-plugins no longer carries workspace semantics.

## Canvas-only drawing

Every plugin paints **only within** `ctx.canvas` (an `Rectangle<i32, Logical>` equal to `EmskinState::usable_area()` at ctx-build time). When an external bar claims an exclusive zone at the top, `canvas.loc.y > 0` — effects must anchor on `canvas.loc`, not `(0, 0)`, or they'll draw behind the bar. Never fall back to "output-absolute" coordinates: `effect-core` intentionally doesn't expose `output_size` anymore (see `effect-core/CLAUDE.md`).

## `is_active` convention

`is_active` must not depend on state that's populated by `pre_paint` (deadlock). Current plugins' `is_active`:
- `measure::is_active` = `self.enabled`
- `skeleton::is_active` = `self.enabled`
- `splash::is_active` = `!self.done`
- `cursor_trail::is_active` = `self.enabled`
- `jelly_cursor::is_active` = `self.enabled`

## Adding a new plugin

1. Add `src/my_plugin.rs` with `pub struct MyOverlay`
2. `impl MyOverlay { pub fn new(); pub fn typed_setter(...) }`
3. `impl effect_core::Effect for MyOverlay { ... }`
4. Add `pub mod my_plugin;` to `lib.rs`
5. In `crates/emskin/src/state.rs::EmskinState::new`, register via the helper:
   ```rust
   let my = register_overlay(&mut effect_chain, my_plugin::MyOverlay::new());
   ```
6. Expose `my` as an `Rc<RefCell<MyOverlay>>` field on `EmskinState`
7. Wire IPC / input from emskin to typed setters on the `.borrow_mut()`
