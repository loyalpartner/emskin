# effect-core

The rendering layer for emskin. Owns the `Effect` trait contract, the chain that drives effects per frame, and the wrapper around smithay's damage-tracked renderer.

## What this crate exports

```
Effect              trait — purely visual
EffectCtx           per-frame read-only snapshot
EffectChain         register + drive pre_paint / paint / post_paint
EffectHandle<T>     Rc<RefCell<T>> wrapper that implements Effect by delegation
CustomElement<R>    sum type of render elements (Surface/Mirror/Solid/Label)
EmskinRenderer      blanket trait bundling Renderer + ImportAll + ImportMem
paint_buffer        MemoryRenderBuffer draw helper
draw_text_onto      cosmic-text glyph blit helper (BGRA)
render_workspace    the "compose a frame" entry point
```

## Trait contract

```rust
pub trait Effect {
    fn name(&self) -> &'static str;
    fn is_active(&self) -> bool;
    fn chain_position(&self) -> u8 { 50 }     // 0..=100, higher = topmost
    fn pre_paint(&mut self, _ctx: &EffectCtx) {}
    fn paint(&mut self, r: &mut GlesRenderer, ctx: &EffectCtx)
        -> Vec<CustomElement<GlesRenderer>>;
    fn post_paint(&mut self) -> bool { false }  // true = request next frame
    fn should_remove(&self) -> bool { false }   // true = chain drops after post_paint
}
```

**There are no input / config / command methods.** Plugins interact with the world only through:
- `EffectCtx` (cursor_pos, output_size, scale, present_time) — read-only inputs
- Their own `pub` methods on the concrete struct — called by the host (emskin) via a typed `Rc<RefCell<T>>`

## Key principle

**From this crate's perspective, any window/workspace/connection info is fixed.** The host freezes per-frame state before calling `render_workspace`; this crate only reads.

## `render_workspace` signature

```rust
pub fn render_workspace(
    output: &Output,
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    space: &Space<Window>,
    chain: &mut EffectChain,
    ctx: &EffectCtx,
    extras: Vec<CustomElement<GlesRenderer>>,
    damage_tracker: &mut OutputDamageTracker,
    clear_color: impl Into<Color32F>,
) -> Result<RenderWorkspaceOutcome, DamageError<GlesError>>;
```

Z-order (top → bottom): chain output → extras → `space`'s client windows.
`extras` are non-effect elements the host captures itself (software cursor, layer-shell surfaces, window mirrors).

## `EffectHandle<T>` pattern

Why: lets the host keep a typed handle for state control **and** register the same instance into the render chain as a trait object.

```rust
let skeleton = Rc::new(RefCell::new(SkeletonOverlay::new()));
chain.register(EffectHandle::new(skeleton.clone()));    // for rendering
// later, from the host's input.rs:
skeleton.borrow_mut().click_at(pos);                    // typed control
```

`EffectHandle<T>` implements `Effect` for any `T: Effect + 'static` by delegating through `borrow()` / `borrow_mut()`. Single-threaded (smithay's compositor loop) so no `Arc<Mutex>` needed.

## Deps

- `smithay` (shared across workspace)
- `cosmic-text` — for text rendering in `draw_text_onto`. Mirrors KWin's `EffectFrame` design decision (text rendering is part of the effect SDK).
- `serde` — derive only

No direct `serde_json` dep; plugins that need JSON config handle it locally.
