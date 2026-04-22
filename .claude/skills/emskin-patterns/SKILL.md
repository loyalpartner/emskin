---
name: emskin-patterns
description: Coding and workflow patterns extracted from the emskin repo (nested Wayland compositor for Emacs). Use when contributing code, writing commits, or adding plugins/IPC messages.
version: 1.0.0
source: local-git-analysis
analyzed_commits: 154
---

# emskin Patterns

Patterns derived from 154 commits of git history. Use these as defaults when
working in this repo; override only with explicit reason.

## Commit conventions

This repo uses **Conventional Commits**, filtered by `cliff.toml` into the
changelog. Distribution across the analyzed window:

| type       | count | share | notes                                   |
|------------|------:|------:|-----------------------------------------|
| `feat:`    |    36 |   24% | new user-facing feature                 |
| `fix:`     |    25 |   16% | bug fix                                 |
| `docs:`    |    25 |   16% | README, CLAUDE.md, changelog notes      |
| `chore:`   |    18 |   12% | releases, asset refreshes; filtered out |
| `refactor:`|    17 |   11% | no behavior change                      |
| `ci:`      |     8 |    5% | workflow edits                          |
| `perf:`    |     2 |    1% | performance                             |

**Scopes observed:** `release`, `ci`, `focus`, `elisp`, `key-cast`, `cli`,
`readme`, `emskin`, `cursor_trail`. Scopes are optional but preferred when
the change is localized — e.g. `feat(key-cast): …`, `refactor(focus): …`.

`chore:`, `style:`, merge, and revert commits are stripped by `cliff.toml`.
Pick a different type if the change deserves a changelog line.

## Repository layout

Cargo workspace with four crates and a hard dep boundary:

```
crates/
├── effect-core/      # Effect trait, EffectChain, render helpers (no host state)
├── effect-plugins/   # built-in overlays — one file per plugin in src/
│   ├── measure.rs   skeleton.rs   splash.rs
│   ├── cursor_trail.rs   jelly_cursor.rs
│   ├── recorder.rs   key_cast.rs
│   └── bitmap_font.rs
├── emskin/           # compositor binary, IPC, handlers/, tests/
└── emskin-bar/       # standalone Wayland client (zero workspace deps)
elisp/                # Emacs-side client, embedded via include_dir!
```

Rules enforced by history and `CLAUDE.md`:

- `effect-plugins` **never** imports from `emskin`. Plugins are purely
  visual via `effect_core::Effect`; host concerns (IPC, workspaces, focus)
  stay in `emskin`.
- `emskin-bar` links against nothing in this workspace — any third-party
  layer-shell bar (waybar, eww) must remain a drop-in replacement.

## Co-change patterns

These files tend to move together. When you touch one, check the others:

### IPC change → three sides in lockstep

A protocol change touches **all three**:

1. `crates/emskin/src/ipc/messages.rs` — add enum variant
2. `crates/emskin/src/ipc/dispatch.rs` — handle the variant
3. `elisp/emskin*.el` — send/receive on the elisp side

`ipc/messages.rs` uses `#[serde(rename_all = "snake_case")]`; acronyms like
`XWaylandReady` wire as `x_wayland_ready` (not `xwayland_ready`).

### New effect plugin — 5-file pattern

History shows this exact sequence for every new effect (`key_cast`,
`cursor_trail`, `jelly_cursor`, `recorder`):

1. `crates/effect-plugins/src/<name>.rs` — new file, `impl Effect`
2. `crates/effect-plugins/src/lib.rs` — `pub mod <name>;`
3. `crates/emskin/src/state/effects.rs` — add an `Rc<RefCell<T>>` field to `EffectsState` and a `register(&mut chain, YourOverlay::new())` call inside `EffectsState::default()`
4. `crates/emskin/src/ipc/messages.rs` + `ipc/dispatch.rs` — `Set<Name>` variant; dispatch reaches the overlay via `state.effects.<name>.borrow_mut()`
5. `elisp/emskin-<name>.el` — `emskin-define-bool-effect` or
   `emskin-define-toggle` macro; auto-registered on `emskin-connected-hook`

### Code change → CLAUDE.md update

`crates/emskin/CLAUDE.md` accumulates non-obvious invariants, gotchas,
and protocol quirks alongside the code. If your fix was tricky enough
to deserve a comment, also add a bullet under the matching section in
`CLAUDE.md`.

## Versioning & release

- Workspace version lives in `[workspace.package]` in root `Cargo.toml`
  (inherited by effect-core / effect-plugins / emskin-bar).
- `crates/emskin/Cargo.toml` **also** keeps a literal `version = "x.y.z"`
  because cargo-aur 0.x doesn't support `version.workspace = true`. Both
  sites must stay in sync — `cargo release` bumps them together via
  `release.toml` pre-release-replacements anchored by
  `# x-release-please-version`.
- Release is `cargo release patch --execute` (or `minor` / explicit).
  That runs `git-cliff` → updates `CHANGELOG.md` → single `chore: release`
  commit → tag → tag push triggers `.github/workflows/release.yml` →
  cargo-aur → GitHub Release + AUR publish.
- Historical note: the repo migrated from release-please to
  cargo-release + git-cliff; don't re-introduce release-please config.

## Local verification (matches CI)

Before pushing, run what `.github/workflows/ci.yml` runs:

```
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo build --workspace
```

Plus, if the change plausibly affects IPC or window lifecycle:

```
cargo build -p emez
cargo test -p emskin
```

Each test spawns its own private host compositor — **emez**
(`crates/emez/`, smithay-based headless Wayland host) for the Wayland
variants, Xvfb for X11 — so tests never touch the developer's real
compositor. Tests run in parallel: the harness pre-allocates a unique
X DISPLAY number per emez/emskin pair from a process-wide reservation
pool and passes them via `--xwayland-display`, so smithay's XWayland
bootstrap never races.

## Code-style defaults

- **Comments / logs / docs in Rust source: English only.** No Chinese in
  `.rs` files — it's in `CLAUDE.md` and the feedback memory.
- **`-p` targeting doesn't rebuild siblings.** `cargo run -p emskin` will
  happily use a stale `emskin-bar`. Plain `cargo run` / `cargo build` hits
  both via `default-members`; with `-p`, also run `cargo build -p emskin-bar`
  explicitly.
- **Never `git push` without explicit user approval.** `git commit` does not
  include push. Same for creating releases.

## smithay is a fork

When reading smithay source to trace behavior, use the vendored checkout at
`~/.cargo/git/checkouts/smithay-*/<commit>/` — that revision carries the
emskin patches (`backend/winit/mod.rs`, `text_input/text_input_handle.rs`,
`selection/seat_data.rs`). A clean upstream clone won't match what the
compositor actually links against.
