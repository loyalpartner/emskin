<p align="center">
  <img src="images/banner.png" alt="emskin — Emacs dressed in a Wayland compositor" width="720"/>
</p>

# emskin

> Dress Emacs in a Wayland skin.

[中文文档](README_cn.md)

emskin wraps Emacs inside a nested Wayland compositor so that **any program** — browsers, terminals, video players, etc. — can be embedded into Emacs windows as if they were native buffers.

![demo](images/demo.gif)

## Vision

Embedding is the first 5%. The endgame is **Emacs deeply scripting the native apps it hosts** — querying and orchestrating them with the same uniformity Emacs already gives its own buffers. Concretely:

- **Browser** — read the focused tab's DOM into a buffer, eval JS, drive forms from Elisp, route LLM tool calls into the live page.
- **Terminal** — file/line heuristics in output become clickable jumps back into Emacs; rerun the last command into a fresh buffer.
- **Video / image apps** — scriptable seek, frame extraction, OCR — all exposed as Elisp commands.
- **Anything with a surface** — that surface becomes addressable from Elisp.

Same IPC layer (compositor ↔ Elisp) that powers embedding today; each future integration adds one IPC verb on top.

## Features

- **Embed any program** — Wayland and X11 apps alike, including FPS games / browser Pointer Lock (pointer constraints + raw mouse delta)
- **Window mirroring** — display the same app in multiple Emacs windows
- **Input method support** — shares the host IM with precise cursor positioning
- **Clipboard sync** — bidirectional between host and embedded apps
- **Launcher support** — rofi / wofi / zofi work out of the box
- **Automatic focus management** — new windows auto-focus; focus falls back on close
- **Built-in screen recording & screenshots** — toggle MP4 capture or snap a PNG, no external tools

## Compatibility

Hosts we've actually tested emskin under. The columns indicate which
kind of desktop session you launched emskin from, **not** which kinds
of clients it can embed — emskin always embeds both Wayland and X11
clients (X11 via the external [`xwayland-satellite`] process, spawned
on demand when an X client connects), regardless of host. `n/a` just
means that compositor or window manager doesn't have a session of
that type to nest into.

[`xwayland-satellite`]: https://github.com/Supreeeme/xwayland-satellite

| Host    | Wayland session | X11 session |
|---------|-----------------|-------------|
| GNOME   | ✓               | ✓           |
| KDE     | ✓               | ✓           |
| Sway    | ✓               | n/a         |
| COSMIC  | ✓               | n/a         |
| niri    | ✓               | n/a         |
| i3wm    | n/a             | ✓           |

pgtk Emacs (`--with-pgtk`) is recommended. GTK3 X11 Emacs also works when `xwayland-satellite` is installed (it's spawned lazily the moment an X client first connects).

## Install

**Requires Rust ≥ 1.89** (`rust-toolchain.toml` pins 1.92.0). If your distro ships an older rustc, install via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Arch Linux (AUR)

```bash
yay -S emskin-bin
```

### From source

```bash
# Dependencies (Arch Linux)
sudo pacman -S wayland libxkbcommon mesa

# Optional: embed X11 applications via xwayland-satellite (AUR).
# Without it emskin runs Wayland-only — X clients can't be embedded
# but everything else (pgtk Emacs, Wayland apps, clipboard, IME) works.
yay -S xwayland-satellite

# Option 1: cargo install
cargo install --git https://github.com/emskin/emskin.git

# Option 2: build from source
git clone https://github.com/emskin/emskin.git
cd emskin && cargo build --release
```

## Quick Start

**Recommended.** `--standalone` is non-invasive:

```bash
emskin --standalone
```

What it does:

- Pre-loads the bundled `emskin.el` from `$XDG_RUNTIME_DIR/emskin-<pid>/elisp/` so you don't need a `(require 'emskin)` of your own.
- Your **`~/.emacs.d/init.el` still loads as usual** — no `-Q`, no `--no-init-file`. Your packages, keybindings, themes, `(setq emskin-cursor-trail t)` etc. all keep working.
- Cleans up the extracted elisp dir on exit.

The only edge case: if you separately cloned emskin and added `(require 'emskin)` from your own `load-path`, the bundled copy will have already been loaded and your `require` becomes a no-op. Developers working on emskin itself should skip `--standalone` and load the elisp manually (see [Emacs Configuration](#emacs-configuration)).

## Usage

### Open embedded apps

Inside Emacs running in emskin:

```
M-x emskin-open-native-app RET firefox
M-x emskin-open-native-app RET foot
```

The app embeds into the current Emacs window and receives keyboard focus.

### Keyboard interaction

When an embedded app has focus, keystrokes go directly to it. Emacs prefix keys (`C-x`, `C-c`, `M-x`) are intercepted and sent back to Emacs; focus restores automatically after the key sequence completes.

- `C-x o` — switch Emacs windows (embedded apps follow buffer switches)
- `C-x 1` / `C-x 2` / `C-x 3` — normal window operations; embedded apps resize automatically

### Effects

emskin ships five live-toggleable effects, plus a non-toggleable startup splash.

| Effect | Variable | Toggle | What it does |
|--------|----------|--------|--------------|
| measure | `emskin-measure` | `M-x emskin-toggle-measure` | Figma-style pixel inspector: crosshair, coordinates, rulers |
| skeleton | `emskin-skeleton` | `M-x emskin-toggle-skeleton` | Frame-layout wireframes (debug overlay, clickable labels) |
| cursor trail | `emskin-cursor-trail` | `M-x emskin-toggle-cursor-trail` | Elastic spring trail behind the mouse pointer |
| jelly cursor | `emskin-jelly-cursor` | `M-x emskin-toggle-jelly-cursor` | Jelly-style animation on Emacs's text caret (pgtk-only color sync) |
| recorder | `emskin-record` | `M-x emskin-toggle-record` | MP4 screen capture with on-screen indicator (red dot + MM:SS timer) |

All default to off. Configure in `~/.emacs.d/init.el`:

```elisp
(setq emskin-cursor-trail t
      emskin-jelly-cursor t)
```

Values sync automatically on IPC connect, so `setq` works unchanged. After changing a variable mid-session, run `M-x emskin-apply-config` to push it immediately.

### Recording & screenshots

Two independent commands; either one works while the other is active:

| Command | Output | Customize |
|---------|--------|-----------|
| `M-x emskin-toggle-record` | `~/Videos/emskin/emskin-YYYYMMDD-HHMMSS.mp4` | `emskin-record-dir`, `emskin-record-fps` (default 30) |
| `M-x emskin-screenshot` | `~/Videos/emskin/emskin-YYYYMMDD-HHMMSS.png` | `emskin-screenshot-dir` (defaults to `emskin-record-dir`) |

The recorder is also exposed as a regular toggle (above), so it picks up the same `setq` + `emskin-apply-config` lifecycle as the other effects. Bind to your key of choice — for example:

```elisp
(global-set-key (kbd "C-c C-r") #'emskin-toggle-record)
```

### Workspaces

Each Emacs frame maps to a workspace:

- `C-x 5 2` — create workspace
- `C-x 5 o` — switch workspace
- `C-x 5 0` — close current workspace

A top-anchored workspace bar (`emskin-bar`) appears automatically once a
second workspace exists and disappears when you drop back to one. Control it
via `--bar=<mode>` on the `emskin` CLI:

- `--bar=auto` *(default)* — find `emskin-bar` next to the emskin binary, falling back to `PATH`
- `--bar=none` — don't launch a bar (e.g. you run waybar yourself)
- `--bar=/path/to/binary` — launch a custom bar instead (anything speaking `zwlr-layer-shell-v1 + ext-workspace-v1`, such as waybar with the right modules)

The bar is a standalone Wayland client — it never talks to emskin's private
IPC, only standard Wayland protocols — and its lifecycle follows the
compositor: it starts when emskin starts and exits when the Wayland socket
closes.

### Launchers

Bind a key to launch rofi / zofi:

```elisp
;; zofi — a launcher designed for emskin, see https://github.com/emskin/zskins
(defun my/emskin-zofi ()
  (interactive)
  (start-process "zofi" nil "setsid" "zofi"))
(global-set-key (kbd "C-c z") #'my/emskin-zofi)

;; rofi
(defun my/emskin-rofi ()
  (interactive)
  (start-process "rofi" nil
                 "setsid" "rofi"
                 "-show" "combi"
                 "-combi-modi" "drun,ssh"
                 "-terminal" "foot"
                 "-show-icons" "-i"))
(global-set-key (kbd "C-c r") #'my/emskin-rofi)
```

## Emacs Configuration

Without `--standalone`, load the elisp manually:

```elisp
(add-to-list 'load-path "/path/to/emskin/elisp")
(require 'emskin)
```

## CLI Options

```
emskin [OPTIONS]

  --standalone            Standalone mode: auto-load built-in elisp
  --fullscreen            Request fullscreen for the host compositor window on startup
  --no-spawn              Don't start Emacs; wait for external connection
  --command <CMD>         Program to launch (default: "emacs")
  --arg <ARG>             Arguments for --command (repeatable)
  --ipc-path <PATH>       IPC socket path (default: $XDG_RUNTIME_DIR/emskin-<pid>.ipc)
  --wayland-socket <NAME> Pin Wayland display socket name (default: wayland-N, auto)
  --bar <MODE>            Workspace bar: "auto" (default), "none", or a path
  --xkb-layout <LAYOUT>   Keyboard layout (e.g. "us", "cn")
  --xkb-model <MODEL>     Keyboard model (e.g. "pc105")
  --xkb-variant <VAR>     Layout variant (e.g. "nodeadkeys")
  --xkb-options <OPTS>    XKB options (e.g. "ctrl:nocaps")
  --log-file <PATH>       Write tracing logs to this file instead of stderr
```

## FAQ

### Crash on startup in a VM

emskin supports software rendering (llvmpipe), but older Mesa (< 21.0) may crash at high resolutions:

```bash
# Check renderer
glxinfo | grep "OpenGL renderer"

# If llvmpipe at high resolution, reduce it
xrandr --output Virtual-1 --mode 1920x1080
```

Make sure mesa is installed: `sudo pacman -S mesa mesa-utils` (Arch) or `sudo apt install mesa-utils` (Debian/Ubuntu).

## Acknowledgements

emskin began as a purpose-built Wayland compositor for the
[Emacs Application Framework (EAF)](https://github.com/emacs-eaf/emacs-application-framework).
The original goal was narrow: get EAF's apps running properly under
Wayland. The broader "embed any Wayland or X11 client into an Emacs
window" capability you see here today grew out of solving that one
problem. Huge thanks to [@manateelazycat](https://github.com/manateelazycat)
for EAF and for years of pushing what an Emacs UI can be — and again
for [holo-layer](https://github.com/manateelazycat/holo-layer), from
which the jelly text-cursor effect and the elisp caret-tracking
pattern (`post-command-hook` + `pos-visible-in-window-p`) are adapted.

emskin is built on [Smithay](https://github.com/Smithay/smithay) — the
Rust Wayland compositor library that does most of the heavy protocol
work.

The on-demand XWayland path (`crates/emskin/src/xwayland_satellite/`) is
ported from [niri](https://github.com/YaLTeR/niri) (`src/utils/xwayland/`,
GPL-3.0-or-later) — attribution and original license preserved in each
file header. The external X server process itself is
[xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite)
by Shawn Wallace — it shoulders the whole X ↔ Wayland protocol
translation so emskin never has to speak X.

## License

GPL-3.0
