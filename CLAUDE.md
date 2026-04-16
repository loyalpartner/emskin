# emskin workspace

This repository is a Cargo workspace with four crates:

```
emskin/                          # workspace root
├── Cargo.toml                   # [workspace] members = ["crates/*"]
├── crates/
│   ├── effect-core/             # rendering framework + Effect trait
│   ├── effect-plugins/          # built-in visual overlays
│   ├── emskin/                  # compositor (window manager) + binary
│   └── emskin-bar/              # external workspace bar (Wayland client binary)
├── elisp/                       # Emacs-side client (shipped embedded)
├── demo/                        # demo scripts (shipped embedded)
├── .github/workflows/           # release.yml runs cargo-aur in crates/emskin
└── ...
```

## Dep graph (hard boundary)

```
emskin      ──→  effect-core
       └──→  effect-plugins  ──→  effect-core
emskin-bar  ──→  (no workspace deps — pure Wayland client on SCTK)
```

`effect-plugins` **cannot** `use emskin::*` — the crate boundary is the contract.
`emskin-bar` links against **nothing** in this workspace; it's deliberately a
standalone program speaking only standard Wayland protocols, so any
third-party bar (waybar / eww / …) could replace it.

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
- `struct EffectCtx` — cursor_pos / canvas / scale / present_time only (canvas = `LayerMap::non_exclusive_zone()`, the single coordinate source for all effects)
- `struct EffectChain` — registers and drives effects per frame
- `struct EffectHandle<T>(Rc<RefCell<T>>)` — lets host share an instance between a typed handle and the chain
- `fn render_workspace(...)` — composes one frame by running the chain + smithay's `render_output`
- `CustomElement<R>` / `EmskinRenderer` / `paint_buffer` / `draw_text_onto` helpers

### `effect-plugins`
The built-in overlays:
- `measure` — crosshair + rulers (pixel inspector)
- `skeleton` — wireframe debug overlay with clickable labels
- `splash` — startup animation

Each plugin struct implements `effect_core::Effect` (purely visual) **and** exposes typed `pub` methods (`set_enabled`, `set_rects`, `click_at`, `dismiss`, `update`, …) that the host uses for control.

### `emskin-bar`
Standalone Wayland client binary. Anchors a `zwlr-layer-shell-v1` surface at the top when `ext-workspace-v1` announces ≥ 2 workspaces, unmaps it below 2. On left-click, sends `ext_workspace_handle_v1.activate` + `manager.commit` — the compositor's existing action pump (`tick.rs` → `WorkspaceAction::Activate`) handles the rest. See `crates/emskin-bar/CLAUDE.md`.

## Guiding principles

1. **From effect-core's perspective, window info is fixed.** emskin freezes `Space<Window>` state before calling `render_workspace`; effects never mutate windows.
2. **Plugins do not know about IPC / workspaces / Emacs connection.** emskin pushes state to them by calling their typed setters directly.
3. **Effect trait has no input methods.** Clicks are hit-tested in emskin's `input.rs` against the overlays' typed `click_at`.
4. **`EffectHandle<T>` is the bridge**: same `Rc<RefCell<T>>` serves as typed handle in emskin + `Box<dyn Effect>` in the chain.
5. **Compositor is self-adaptive via layer-shell.** Emacs's geometry is `EmskinState::usable_area()` = `LayerMap::non_exclusive_zone()`. There is no `bar_height()` or "bar is enabled" concept in the compositor — if any layer surface declares `exclusive_zone`, the non-exclusive rect shrinks and `relayout_emacs()` pushes the new size to Emacs. `emskin-bar` is just one such client; swapping it for waybar works out of the box.
6. **Cargo-aur runs in `crates/emskin/`**. Because cargo-aur 0.x does not support `version.workspace = true`, `crates/emskin/Cargo.toml` keeps literal `version` / `edition` / `license` / `repository` / `authors` values (commented in the file). Other crates inherit from `[workspace.package]`. The release workflow pre-builds `emskin-bar` and copies it into `crates/emskin/` so `[package.metadata.aur].files` can ship it next to the main binary.
7. **`cargo run -p emskin` does not rebuild sibling binaries.** emskin-bar is not in emskin's dep graph, so `-p` targeting won't pick up bar changes. `default-members` ensures plain `cargo build` / `cargo run` rebuild both, but if you pass `-p`, also run `cargo build -p emskin-bar` explicitly.

## `chain_position` assignments

| overlay    | position | rationale |
|------------|----------|-----------|
| `splash`   | 95       | Covers everything during startup |
| `skeleton` | 85       | Debug overlay with labels |
| `measure`  | 80       | Cursor measurement, visible when toggled |

Effects with higher positions appear earlier in the custom-element Vec (which is the topmost slot in smithay's render stack).

## When to look where

- "How does X render?" → `crates/effect-core/` (render_workspace) + the plugin's `paint`
- "How do I toggle Y?" → the plugin's typed setter, called from `crates/emskin/src/ipc/dispatch.rs`
- "Why does click Z absorb?" → `crates/emskin/src/input.rs` (window-manager-owned hit-testing)
- Per-crate architectural notes are in that crate's own `CLAUDE.md`.
