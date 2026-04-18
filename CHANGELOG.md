# Changelog

All notable changes to emskin are documented here.
Generated from conventional commits via git-cliff.

## [0.3.5] - 2026-04-18

### Bug Fixes
- Don't use {{version}} in pre-release-commit-message
## [0.3.4] - 2026-04-18

### Bug Fixes
- Generate unique buffer names for embedded apps

### Documentation
- Record KeyboardFocusTarget refactor + X11 gotchas
- README — add Vision section (Emacs deeply scripts native apps)
- README — holo-layer credit, clarify --standalone is non-invasive

### Refactor
- Unify Wayland/X11 focus via KeyboardFocusTarget
## [0.3.3] - 2026-04-18

### Bug Fixes
- Scope cargo-release hook + drop redundant replacements
- Use full default title pattern

### CI
- Rust-cache for speed, decouple release-please from check

### Documentation
- Add holo-layer acknowledgement
## [0.3.1] - 2026-04-18

### Bug Fixes
- Move pull-request-title-pattern into package
## [0.3.0] - 2026-04-18

### CI
- Gate release-please behind check job + fix PR title pattern

### Documentation
- README — recorder, recording/screenshot section, FPS-game support
- Capture-path gotchas + effect-plugins recorder/bitmap_font entries

### Features
- Support pointer-constraints + relative-pointer (#24)
- Screen recording (toggle-record) + screenshot (take_screenshot)
## [0.2.0] - 2026-04-17

### Features
- Jelly text-cursor + unified effect toggle macros (#32)
## [0.1.10] - 2026-04-17

### Bug Fixes
- Test release automation end-to-end
- Verify PAT-driven release automation

### CI
- Authenticate with PAT instead of GITHUB_TOKEN
## [0.1.9] - 2026-04-17

### Bug Fixes
- Release 0.1.9
## [0.1.8] - 2026-04-17

### Bug Fixes
- Use root package so all commits count
## [0.1.7] - 2026-04-17

### Features
- Enable by default
## [0.1.6] - 2026-04-17

### Bug Fixes
- Kill bar child on exit to prevent deadlock

### Features
- Cursor trail effect (#28)
## [0.1.5] - 2026-04-16

### Features
- Rename crosshair to measure and add Figma-style rulers
## [0.1.4] - 2026-04-16

### Documentation
- Add AUR install instructions
## [0.1.3] - 2026-04-16

### Bug Fixes
- Bump github-actions-deploy-aur to v4.1.2

### CI
- Simplify AUR publish with github-actions-deploy-aur
## [0.1.2] - 2026-04-15

### Bug Fixes
- Don't send SurfaceSize on bar transition
- Workspace switch race condition causing app migration and disappearance

### Documentation
- Add Rust version requirement to install instructions
- Update CLAUDE.md for elisp split, sub-structs, and workspace race fix
- Add Smithay acknowledgement to README

### Refactor
- Split emskin.el into 5 domain modules
- Group 17 smithay protocol fields into WaylandState sub-struct
- Extract mirror rendering to mirror_render.rs
- Extract mirror_hit_test as pure function with 8 new tests
- Extract FocusState and SelectionState from EmskinState
- Extract event loop body to tick.rs
- Extract clipboard event handling to clipboard_dispatch.rs
- Extract IPC dispatch from main.rs to ipc/dispatch.rs
- Replace HostClipboard enum with ClipboardBackend trait
- Convert to mixed crate and add unit test infrastructure
## [0.1.1] - 2026-04-14

### CI
- Switch to cargo-aur for AUR packaging

### Documentation
- Add English README and rename original to README_cn.md
- Revert to demo.gif in README
- Replace demo.gif with demo.mp4 and add zofi launcher
## [0.1.0] - 2026-04-13

### Bug Fixes
- Use ssh-keyscan for AUR host key verification
- Run makepkg --printsrcinfo in builder home dir
- Restore focus to previous window when layer surface closes
- Clean up all windows when app with mirror exits
- Workspace app migration driven by Emacs IPC, not auto-migrate
- Set tiled state on embedded toplevel to prevent cell-aligned gaps
- Use IPC connection state to guard startup clipboard skip
- Guard against dead cursor surface after client disconnect
- Software-render Surface cursors for GTK3/Emacs
- Compensate CSD shadow offset in mirror input coordinates
- Skip keyboard focus change when clicking same-client popup
- Render embedded apps within window body, hide chrome
- Skip disabled bars in skeleton + omit null window_id in set_focus
- Compensate CSD window_geometry in mirror render
- Render mirror via subsurface tree walk
- Clamp X11 window size to minimum 1×1 to prevent smithay panic
- Preserve host clipboard on Emacs startup
- Release stuck modifier keys on focus regain
- Encode visibility false as JSON boolean, not string
- Give Emacs initial keyboard focus on toplevel connect
- Address remaining rust-review CRITICAL and HIGH issues

### CI
- Add AUR publish job to release workflow
- Build release binaries in Arch Linux container
- Add release workflow for AUR binary packaging
- Add clippy, fmt checks and pin Rust 1.92.0

### Documentation
- Add cargo install --git as recommended install method
- Add FAQ for VM soft-rendering crash at high resolution
- Add Usage section and demo gif to README
- Simplify README, remove technical details
- Add rofi launcher configuration example to README
- Add tested desktop environment compatibility table
- Broaden scope — embed arbitrary programs, not just EAF apps
- Add banner and icon, embed banner in README
- Add --standalone mode to README
- Add demo gif to README and move to screenshots/
- Fix keyboard layout description — CLI args, not auto-inherited
- Add README with build and usage instructions

### Features
- Add letter-by-letter splash screen animation
- Set XDG_SESSION_DESKTOP=emskin for child process
- Redesign workspace bar with pill buttons and center title
- Handle fullscreen/unfullscreen requests for embedded apps
- Add multi-workspace support with ext-workspace-v1 protocol
- Add wlr-layer-shell support for rofi/wofi launchers
- Auto-focus new windows, focus fallback, remove xdg_activation
- Add X11 cursor tracking via XFixes for emacs-gtk
- Add X11 clipboard bridge for gtk3 Emacs via XWayland
- Add X11 Emacs (gtk3 via XWayland) support
- Add wp_cursor_shape_v1 protocol for cursor shape forwarding
- Add text_input_v3 IME support for embedded apps
- Add linux_dmabuf protocol support for GPU-accelerated clients
- Add skeleton overlay (frame layout inspector)
- Add --standalone mode to embed and auto-load elisp
- Add crosshair overlay (caliper tool)
- Prefer Wayland for child apps on X11 host; update README
- Add X11 clipboard fallback via HostClipboard abstraction
- Add XWayland support for X11 applications
- Implement xdg_activation_v1 for focus-on-launch
- Click EAF app to select corresponding Emacs window
- Add popup grab, Ctrl/Alt interception, and mirror popup rendering
- Add clipboard sync between host compositor and internal clients
- Add window mirror system for EAF app split-screen display
- Add eaf-open-native-app for launching native Wayland apps
- Add key forwarding, click-to-focus, and improved demo app
- Add visibility tracking, kill-buffer lifecycle, and sync improvements
- Add EAF app window management, geometry sync, and code quality fixes
- Add IPC layer, clap CLI, and eaf-eafvil.el Emacs client
- Inherit host keymap, hide CSD borders, exit on Emacs close
- Add eafvil nested Wayland compositor for Emacs

### Performance
- Skip GPU rendering when idle (damage-based redraw)
- Fix layer-shell client startup latency

### Refactor
- Rename buffer prefix from *eaf:* to *emskin:*
- Generalize embedded dir extraction for elisp + demo
- Rename crate directory eafvil/ to emskin/
- Rename eafvil → emskin across source and docs **(breaking)**
- Replace forward_key IPC with prefix-key focus redirect
- Event-driven X11 clipboard outgoing transfers
- Replace host keymap detection with CLI arguments

