<p align="center">
  <img src="images/banner.png" alt="emskin — Emacs dressed in a Wayland compositor" width="720"/>
</p>

# emskin

> Dress Emacs in a Wayland skin.

[中文文档](README_cn.md)

emskin wraps Emacs inside a nested Wayland compositor so that **any program** — browsers, terminals, video players, etc. — can be embedded into Emacs windows as if they were native buffers.

![demo](images/demo.gif)

## Features

- **Embed any program** — Wayland and X11 apps alike
- **Window mirroring** — display the same app in multiple Emacs windows
- **Input method support** — shares the host IM with precise cursor positioning
- **Clipboard sync** — bidirectional between host and embedded apps
- **Launcher support** — rofi / wofi / zofi work out of the box
- **Automatic focus management** — new windows auto-focus; focus falls back on close

## Compatibility

| Desktop | Wayland | X11 |
|---------|---------|-----|
| GNOME   | ✓       | ✓   |
| KDE     | ✓       | ✓   |
| Sway    | ✓       | —   |
| COSMIC  | ✓       | —   |

pgtk Emacs (`--with-pgtk`) is recommended. GTK3 X11 Emacs also works via XWayland.

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

# Option 1: cargo install
cargo install --git https://github.com/emskin/emskin.git

# Option 2: build from source
git clone https://github.com/emskin/emskin.git
cd emskin/emskin && cargo build --release
```

## Quick Start

```bash
# Zero-config: auto-loads built-in elisp, no Emacs setup needed
emskin --standalone
```

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

### Workspaces

Each Emacs frame maps to a workspace:

- `C-x 5 2` — create workspace
- `C-x 5 o` — switch workspace
- `C-x 5 0` — close current workspace

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
  --no-spawn              Don't start Emacs; wait for external connection
  --command <CMD>         Program to launch (default: "emacs")
  --arg <ARG>             Arguments for --command (repeatable)
  --bar <MODE>            Workspace bar: "builtin" (default) or "none"
  --xkb-layout <LAYOUT>   Keyboard layout (e.g. "us", "cn")
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

Built on [Smithay](https://github.com/Smithay/smithay), a Wayland compositor library for Rust.

## License

GPL-3.0
