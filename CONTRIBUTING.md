# Contributing to emskin

Thanks for considering a patch. This document covers what a contributor needs
to know — setup, local checks, commit style, PR flow, and filing issues. For
architecture and internals, read [`CLAUDE.md`](CLAUDE.md) and the per-crate
`crates/*/CLAUDE.md` files.

## Setup

Install the pinned Rust toolchain (currently 1.92.0 — `rustup show` inside
`crates/emskin/` picks it up from `rust-toolchain.toml`) and the system libs.
CI's [apt install list](.github/workflows/ci.yml) is the source of truth for
Debian/Ubuntu. On Arch:

```
sudo pacman -S wayland libxkbcommon libinput mesa seatd pixman \
               libxcb xcb-util-cursor xcb-util-wm fontconfig freetype2
```

E2E tests run against two private host backends:
- **Wayland host**: our own `emez` crate (`crates/emez/`), a smithay-based
  headless compositor that ships with the workspace — no extra system
  package needed.
- **X11 host**: `Xvfb` from `xorg-server-xvfb`.

Plus the test clients themselves: `wl-clipboard`, `xclip`, `ffmpeg`. On Arch:

```
sudo pacman -S xorg-server-xvfb wl-clipboard xclip ffmpeg
```

`xwayland-satellite` is **not** required to run the test suite —
emskin's satellite supervisor probes the binary at startup and falls
back to "Wayland-only" if missing, and the E2E clipboard tests don't
drive any internal-X client. Install it (AUR on Arch) only when you
want to exercise emskin end-to-end against real X applications.

## Local checks

Match CI before pushing:

```
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo build --workspace
```

Unit tests run with `cargo test --workspace`.

E2E tests spawn per-test **private** host compositors (`emez` for the
"Wayland host" variant, `Xvfb` for the "X11 host" variant) and run
emskin on top, fully decoupled from your desktop compositor. Build
the emez binary once, then invoke cargo test directly:

```
cargo build -p emez
cargo test -p emskin
```

Tests run in parallel. The harness pre-allocates a unique X DISPLAY
number per test (both for emez's embedded XWayland and for emskin's
own `xwayland-satellite` supervisor) from a process-wide reservation
pool, passes them through `--xwayland-display`, and isolates every
test's `XDG_RUNTIME_DIR`/Wayland socket, so concurrent spawns don't
race.

## Commits & PRs

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/).
Use one of the types that `cliff.toml` puts in the changelog:

```
feat:     new user-facing feature
fix:      bug fix
perf:     performance improvement
refactor: no behavior change
docs:     documentation only
test:     tests only
ci:       CI config
build:    build system / deps
```

`chore`, `style`, merge, and revert commits are filtered out. If your commit
wouldn't be worth a line in the release notes, pick one of those.

Open PRs against `main`. Rebase on current `main` and clean up WIP commits
before review. CI (`fmt --check`, `clippy -D warnings`, `build --workspace`)
must be green.
