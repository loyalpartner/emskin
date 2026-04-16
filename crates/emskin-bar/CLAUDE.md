# emskin-bar

External workspace bar for emskin. A standalone Wayland client — **not** a compositor plugin. Speaks only standard protocols:

- `wl_compositor` / `wl_shm` / `wl_seat` / `wl_output` (SCTK helpers)
- `zwlr-layer-shell-v1` (top-anchored, `exclusive_zone = BAR_HEIGHT`)
- `ext-workspace-v1` (workspace list + activate)

## Protocol boundary

The bar **never** touches emskin's private JSON-over-UnixSocket IPC at `/tmp/emskin-<pid>.sock`. That channel belongs to Emacs ↔ emskin. This crate only speaks Wayland, so any third-party bar that also speaks `ext-workspace-v1` (waybar, eww, …) could replace it — emskin just wouldn't auto-spawn those.

## Files

```
src/
├── main.rs       — entry: connect → build BarState → blocking_dispatch loop
├── state.rs      — BarState + SCTK delegate traits (compositor / shm / seat /
│                   output / layer-shell / pointer / registry)
├── workspace.rs  — ext-workspace-v1 Dispatch impls: pending_workspaces during
│                   a batch → promoted into `workspaces` on `done`; removed
│                   events purge from both lists
└── render.rs     — argb8888 SHM buffer draw: bar background fill, flat pills
                    with active-accent, cosmic-text centred label
```

## Lifecycle

- `visibility = workspaces.len() >= 2` — evaluated on every `ext_workspace_manager_v1.done`.
- `false → true`: `create_layer()` — binds a `Layer::Top` surface with `Anchor::TOP|LEFT|RIGHT`, size `(0, BAR_HEIGHT)`, and `exclusive_zone = BAR_HEIGHT`. Compositor's `LayerMap::non_exclusive_zone()` shrinks by that amount automatically, which is what drives the Emacs frame resize on the server side.
- `true → false`: drop the `LayerSurface` (destroys it on the server).
- Compositor disconnect (socket closed by emskin exiting): `blocking_dispatch` errors out → `main` exits.
- `finished` event: explicit "manager going away" — bar sets `exit = true` and the loop terminates.

## Why not SCTK for ext-workspace-v1

SCTK doesn't provide bindings for that protocol. The three interfaces (`manager`, `group_handle`, `handle`) are implemented with plain `wayland_client::Dispatch` in `workspace.rs`. UserData is `()` for every object — events find their `WorkspaceEntry` by scanning `pending_workspaces` + `workspaces` for a matching handle (PartialEq on Wayland proxies).

## Gotchas

- ext-workspace-v1 `Removed` event: the server destroys the handle immediately after emitting this — the client must NOT call `handle.destroy()` afterwards (triggers protocol error). Just filter it out of your local lists.
- Any event carrying a `new_id` (here: manager's `workspace_group` @ opcode 0 and `workspace` @ opcode 1) requires `event_created_child!` in the Dispatch impl, or wayland-client panics at dispatch time. Staging protocols don't export `EVT_*` constants — hardcode opcodes from the XML.
- Frame-callback-driven redraw is a trap: if `frame()` skips `draw()` (nothing changed), no next callback gets scheduled, and any later state event has no way to trigger a repaint. Always `draw()` directly on state change (configure, `update_visibility` with `configured_once`), and treat frame callbacks only as a pacing hint.
- Workspace wire id is a string like `"emskin-ws-3"` — `parse_id` takes the last `-`-separated token. Non-numeric ids fall through to 0 and still render.
- `SlotPool::new(size, &shm)` panics if shm doesn't advertise argb8888 — emskin does, so we assert rather than handle.
- `blocking_dispatch` returns `Err` on compositor disconnect (socket closed). `main`'s `?` then propagates and we exit — no need for an explicit SIGTERM from emskin.
- `BTN_LEFT = 0x110` from `<linux/input-event-codes.h>`; avoid pulling in `libc` / `input-linux` just for the constant.
