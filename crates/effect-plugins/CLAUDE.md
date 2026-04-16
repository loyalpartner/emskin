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

### `workspace_bar` — chain_position 90

Top-of-screen pill bar showing workspace buttons + centered title. Currently lives here; will be extracted into an external program using `layer-shell + ext-workspace-v1` (see issue tracker).

- `pub fn set_enabled(bool)` — reflects `--bar=none/builtin` CLI flag
- `pub fn set_buttons(Vec<ButtonSpec>)` (`update` today) — called from tick.rs when workspace list changes
- `pub fn click_at(pos) -> Option<u64>` — returns clicked workspace id
- `pub fn visible()` — `buttons.len() > 1`

## `is_active` convention

`is_active` must not depend on state that's populated by `pre_paint` (deadlock). Current plugins' `is_active`:
- `measure::is_active` = `self.enabled`
- `skeleton::is_active` = `self.enabled`
- `splash::is_active` = `!self.done`
- `workspace_bar::is_active` = `self.enabled` (not `enabled && visible()` — visible() depends on `buttons` filled by external `update()` call)

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
