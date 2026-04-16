# emskin workspace

This repository is a Cargo workspace with three crates:

```
emskin/                          # workspace root
├── Cargo.toml                   # [workspace] members = ["crates/*"]
├── crates/
│   ├── effect-core/             # rendering framework + Effect trait
│   ├── effect-plugins/          # built-in visual overlays
│   └── emskin/                  # compositor (window manager) + binary
├── elisp/                       # Emacs-side client (shipped embedded)
├── demo/                        # demo scripts (shipped embedded)
├── .github/workflows/           # release.yml runs cargo-aur in crates/emskin
└── ...
```

## Dep graph (hard boundary)

```
emskin  ──→  effect-core
     └──→  effect-plugins  ──→  effect-core
```

`effect-plugins` **cannot** `use emskin::*` — the crate boundary is the contract.
If you need something from emskin inside a plugin, the design is wrong.

## Crate responsibilities

### `emskin`
The compositor / window manager. Owns:
- Wayland protocol surface state (xdg-shell, layer-shell, xwayland, ipc, clipboard, cursor)
- `EmskinState` with workspace/focus/apps/IPC
- winit event loop, input routing
- Typed `Rc<RefCell<T>>` handles to each overlay (for config, click hit-tests, etc.)
- `.github/` release pipeline; `elisp/` client; `demo/`

### `effect-core`
The rendering layer. Owns:
- `trait Effect` — pure visual contract (no input, no config, no workspace)
- `struct EffectCtx` — cursor_pos / output_size / scale / present_time only
- `struct EffectChain` — registers and drives effects per frame
- `struct EffectHandle<T>(Rc<RefCell<T>>)` — lets host share an instance between a typed handle and the chain
- `fn render_workspace(...)` — composes one frame by running the chain + smithay's `render_output`
- `CustomElement<R>` / `EmskinRenderer` / `paint_buffer` / `draw_text_onto` helpers

### `effect-plugins`
The built-in overlays:
- `measure` — crosshair + rulers (pixel inspector)
- `skeleton` — wireframe debug overlay with clickable labels
- `splash` — startup animation
- `workspace_bar` — top-of-screen pill bar (future: extracted into external program — see issue tracker)

Each plugin struct implements `effect_core::Effect` (purely visual) **and** exposes typed `pub` methods (`set_enabled`, `set_rects`, `click_at`, `dismiss`, `update`, …) that the host uses for control.

## Guiding principles

1. **From effect-core's perspective, window info is fixed.** emskin freezes `Space<Window>` state before calling `render_workspace`; effects never mutate windows.
2. **Plugins do not know about IPC / workspaces / Emacs connection.** emskin pushes state to them by calling their typed setters directly.
3. **Effect trait has no input methods.** Clicks are hit-tested in emskin's `input.rs` against the overlays' typed `click_at`.
4. **`EffectHandle<T>` is the bridge**: same `Rc<RefCell<T>>` serves as typed handle in emskin + `Box<dyn Effect>` in the chain.
5. **Cargo-aur runs in `crates/emskin/`**. Because cargo-aur 0.x does not support `version.workspace = true`, `crates/emskin/Cargo.toml` keeps literal `version` / `edition` / `license` / `repository` / `authors` values (commented in the file). Other crates inherit from `[workspace.package]`.

## `chain_position` assignments

| overlay         | position | rationale |
|-----------------|----------|-----------|
| `splash`        | 95       | Covers everything during startup |
| `workspace_bar` | 90       | Always-visible UI chrome |
| `skeleton`      | 85       | Debug overlay with labels |
| `measure`       | 80       | Cursor measurement, visible when toggled |

Effects with higher positions appear earlier in the custom-element Vec (which is the topmost slot in smithay's render stack).

## When to look where

- "How does X render?" → `crates/effect-core/` (render_workspace) + the plugin's `paint`
- "How do I toggle Y?" → the plugin's typed setter, called from `crates/emskin/src/ipc/dispatch.rs`
- "Why does click Z absorb?" → `crates/emskin/src/input.rs` (window-manager-owned hit-testing)
- Per-crate architectural notes are in that crate's own `CLAUDE.md`.
