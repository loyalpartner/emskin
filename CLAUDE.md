# emskin workspace

Cargo workspace, five crates with hard dep boundaries:

```
crates/
├── effect-core/       # Effect trait, EffectChain, render helpers
├── effect-plugins/    # built-in overlays (one file per plugin)
├── emskin/            # compositor binary, IPC, handlers/, tests/
├── emskin-bar/        # standalone Wayland client (zero workspace deps)
└── emskin-clipboard/  # smithay-free host clipboard proxy (data-control / wl_data_device / X11)
elisp/                 # Emacs-side client, embedded via include_dir!
demo/                  # demo scripts, embedded
```

```
emskin      ──→  effect-core
       └──→  effect-plugins   ──→  effect-core
       └──→  emskin-clipboard
emskin-bar  ──→  (nothing in this workspace)
```

- `effect-plugins` **cannot** `use emskin::*` — purely visual via the
  `Effect` trait. Host concerns (IPC, workspaces, focus, input) stay in
  `emskin`.
- `emskin-bar` links against nothing in this workspace on purpose: any
  third-party layer-shell bar (waybar, eww) must remain a drop-in
  replacement.
- `emskin-clipboard` **cannot** `use smithay` — it's a self-contained
  host clipboard proxy usable by any nested Wayland compositor. The
  smithay-aware glue (SelectionTarget ↔ SelectionKind mapping, XWM
  replay, async pipe drain for X11) lives in `emskin/src/clipboard_bridge.rs`.

Deeper per-crate notes live in each `crates/*/CLAUDE.md`.

## Invariants (every session)

1. **effect-core sees window state as frozen.** emskin freezes
   `Space<Window>` before calling `render_workspace`; effects never mutate.
2. **Plugins don't know about IPC / workspaces / Emacs.** The host drives
   them by calling typed setters (`set_enabled`, `set_rects`, …).
3. **`Effect` trait has no input methods.** Click hit-testing happens in
   `emskin/src/input.rs` against the overlay's typed `click_at`.
4. **`EffectHandle<T>` is the bridge** — same `Rc<RefCell<T>>` serves as
   typed handle in emskin and `Box<dyn Effect>` in the chain.
5. **Compositor is self-adaptive via layer-shell.** Emacs's geometry is
   `EmskinState::usable_area() = LayerMap::non_exclusive_zone()`. There is
   no `bar_height()` concept; any layer-shell client declaring
   `exclusive_zone` shrinks it and `relayout_emacs()` pushes the new size.
6. **`crates/emskin/Cargo.toml` keeps literal `version`/`edition`/… values**
   because cargo-aur 0.x doesn't support `version.workspace = true`. Both
   this and root `[workspace.package].version` must bump together
   (`cargo release` handles both via `release.toml`).
7. **`cargo run -p emskin` doesn't rebuild `emskin-bar`.** Plain
   `cargo run` / `cargo build` hits both (via `default-members`); with
   `-p`, also run `cargo build -p emskin-bar` explicitly.

## Testing

E2E tests each spawn their own private host compositor — **emez**
(`crates/emez/`, a smithay-based headless wayland host with
data-control, wl_data_device fallback mode via `--no-data-control`,
`xdg_activation_v1`, and an internal clipboard manager that lets
short-lived clients like `wl-copy` exit cleanly without forking a
daemon) for Wayland-host tests, Xvfb for X11-host tests — so
tests run in full isolation from the developer's desktop. Invoke
directly with cargo:

```
cargo build -p emez
cargo test -p emskin
```

**Two-step, not one**: emez is a binary crate and cargo stable has
no bindeps (`-Z bindeps` is nightly), so there's no first-class way
to declare "build this binary before my tests". The harness
discovers the emez binary at runtime via `find_emez_binary()`
scanning `target/{debug,release}/emez`; if it's not there, the first
test that needs it panics with a clear error. Run the two commands
in order or wrap them in a shell alias.

Tests run in parallel: the harness pre-allocates unique X DISPLAY
numbers per test (from a process-wide reservation pool) and passes
them to both emez and emskin via `--xwayland-display`, so smithay's
XWayland bootstrap never races. See `crates/emskin/CLAUDE.md` →
"Testing" for harness details.

## See also

- `.claude/skills/emskin-patterns/SKILL.md` — commit conventions, co-change
  patterns, release flow, `chain_position` table, "when to look where"
  navigation. Loaded on demand when writing commits, adding plugins, or
  cutting releases.
- `CONTRIBUTING.md` — setup, local checks, PR flow for outside contributors.
