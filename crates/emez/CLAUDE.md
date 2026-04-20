# emez

Minimal smithay-based headless Wayland compositor used as a **private test host** for emskin's E2E suite. Not a general-purpose compositor: no rendering, no input injection, no window layout. It advertises the Wayland globals emskin (and test clients like `wl-copy`, `xclip`) need, bridges selections between Wayland and its own XWayland, and otherwise stays out of the way.

## Why a sister crate instead of weston or Xvfb

- **weston**: lacks `zwlr_data_control_v1` + `ext_data_control_v1`, so emskin's `ClipboardProxy` had to fall back to the X11 selection path — missing the primary code path we ship in production. emez advertises both, forcing the wayland data-control side to be exercised.
- **Xvfb**: pure X server, no Wayland socket at all. Fine for the `NestedHost::x11()` variant, but insufficient for "Wayland host with a collaborating XWayland" tests.

## Files

```
src/
├── main.rs       — CLI (clap) + calloop EventLoop + signal-hook SIGTERM
│                   handler + X socket cleanup on shutdown
├── state.rs      — Emez struct, Wayland global init, listener socket,
│                   delegate_xwayland_shell!
├── handlers.rs   — All wayland protocol handlers:
│                   compositor (flushes frame callbacks immediately),
│                   xdg_shell, shm, seat, output, data-device,
│                   primary-selection, wlr-data-control, ext-data-control,
│                   SelectionHandler forwarding wayland selections to X
└── xwayland.rs   — start_xwayland, XwmHandler impl (stub layout +
                    selection bridging), XWaylandShellHandler
```

## CLI contract (used by the harness)

```
--socket <NAME>                   Pin the Wayland socket name
--log-file <PATH>                 Redirect tracing to a file
--xwayland                        Spawn embedded XWayland (off by default)
--xwayland-display <N>            Pin the XWayland DISPLAY number
--xwayland-ready-file <PATH>      Touch this file when XWayland reports Ready
--no-data-control                 Hide `zwlr_data_control_v1` + `ext_data_control_v1`
                                  from clients. Simulates KDE < 6.2 / GNOME
                                  Mutter; forces emskin onto its wl_data_device
                                  fallback. Also enables the built-in
                                  clipboard manager (see Design principles).
--activation-token-file <PATH>    Pre-seed one `xdg_activation_v1` token and
                                  write its string here. Clients (emskin
                                  itself, wl-copy, wl-paste) read
                                  `XDG_ACTIVATION_TOKEN` from the env and
                                  call `xdg_activation_v1.activate` to pull
                                  focus through the protocol-legal startup-
                                  notification path — the only way to get
                                  focus now that emez doesn't auto-focus
                                  new toplevels.
```

`--xwayland-ready-file` is how the harness knows XWayland is up — polling it is cheap and removes any dependency on parsing logs or wayland handshakes.

## Design principles

- **Stub most handlers, get selection bridging right.** Tests care about clipboards and whether clients can hand-shake; they don't care about window placement or input. Window-manager-ish XwmHandler callbacks (`new_window`, `configure_request`, `resize_request`, …) are minimal no-ops. Selection callbacks (`new_selection`, `send_selection`, `cleared_selection`) are fully implemented in both directions (X ↔ Wayland).
- **Fire frame callbacks on every commit immediately.** emez does no rendering, but winit-backed clients (including emskin) block their render loop waiting for frame callbacks. `CompositorHandler::commit` walks the surface tree and drains `frame_callbacks.drain(..).for_each(cb.done(now))`. Without this, capture/recording tests hang.
- **Always allow X selection access.** `XwmHandler::allow_selection_access` returns `true` unconditionally — the harness isn't testing access policy, it's testing data flow.
- **Accept the X window map.** `map_window_request` calls `window.set_mapped(true)`. Selection-owner X clients (`xclip`) stall waiting for their window to become viewable otherwise.
- **No focus policy — only protocol-driven focus.** emez does *not* auto-focus new xdg-toplevels (would be a focus-stealing policy stricter than even KWin's default `low` prevention, and unlike anything Mutter does). Clients request focus through `xdg_activation_v1.activate` using a token inherited from env — mirroring real GNOME / KWin startup-notification behaviour. See `XdgActivationHandler::request_activation` in `handlers.rs`.
- **Primary focus fallback.** The first `xdg_activation.activate` target is remembered as `primary_fallback_focus`. When no surface currently has keyboard focus (e.g. a short-lived wl-copy just exited), `SeatHandler::focus_changed(None)` sends focus back to primary. Matches real-compositor behaviour: focus doesn't evaporate, it falls back to the last in-use window.
- **Built-in clipboard manager (only under `--no-data-control`).** When running in the "no data-control" mode where wl_data_device is the only clipboard path, emez drains every offered mime from a client-owned selection into compositor memory via `request_data_device_client_selection`, then replaces the selection with a compositor-owned entry backed by the buffer. Result: tools like `wl-copy` don't need a daemon fork — the originating process can exit right after `set_selection`, and the selection survives as long as emez is alive. This is the wayland equivalent of X11's CLIPBOARD_MANAGER / SAVE_TARGETS. Gated on `clipboard_manager_enabled` (= `hide_data_control`); under normal data-control mode every clipboard client is long-lived (emskin and well-behaved DC apps), so drain would be counterproductive.

## Lifecycle (matters for clean shutdown)

1. Harness sends SIGTERM.
2. signal-hook handler thread calls `LoopSignal::stop(); LoopSignal::wakeup();` — both are required on calloop 0.14 (`stop()` alone only sets a flag; `wakeup()` pokes the poller so `epoll_wait` returns).
3. `event_loop.run` returns.
4. `main` runs `fs::remove_file` for `/tmp/.X11-unix/X<N>` and `/tmp/.X<N>-lock` as a belt-and-braces step, then `drop(state)`.
5. Dropping `state` + `event_loop` drives smithay's `X11Lock::Drop` (which would also remove those files) and `XWaylandClientData::Drop`.

The belt-and-braces step matters because signal-driven exits occasionally terminate the process with `128 + SIG` before the full Drop chain settles. Two paths both remove the files — whichever wins, the next test doesn't inherit residue.

## Gotchas

- `LoopSignal::stop()` without `wakeup()` on calloop 0.14 leaves `event_loop.run` blocked in `epoll_wait` — the flag is set but nothing wakes the loop to check it. emez always pairs them.
- `CompositorHandler::client_compositor_state` must check `XWaylandClientData` **before** the host's own `ClientState` — XWayland clients carry their own `CompositorClientState` on `XWaylandClientData`, and hitting the `ClientState::get_data().unwrap()` path first panics.
- `SelectionHandler::new_selection` is the wayland→X direction (forwards to `X11Wm::new_selection`). The X→wayland direction lives in `XwmHandler::new_selection` (calls `set_data_device_selection`). Both must be wired or paste goes one-way only.
- The `emskin/smithay` fork's `X11Wm::new_selection` flushes the x11rb connection after `set_selection_owner` — without that flush, xclip timing-sensitive tests see a transient "no owner". emez depends on that patch (carried on branch `emskin-patches`).
- `XWayland::spawn(None, ...)` scans `/tmp/.X11-unix/X0..X32` for a free slot. Two parallel emez instances calling this with `None` will race. Always pass `Some(N)` via `--xwayland-display` in test contexts.
- XWayland is **not** auto-killed by smithay's `XWayland` Drop — the child handle lives on `XWaylandClientData` and `Child::Drop` in Rust 1.58+ does not kill. The wayland-socket disconnect on parent exit is what eventually ends XWayland; that's fine when the parent (emez) really dies, but it means a SIGKILL'd emez leaves XWayland as an orphan holding `/tmp/.X11-unix/X<N>` + `.X<N>-lock`. Harness SIGTERMs emez; emez runs the belt-and-braces cleanup.
- `DndGrabHandler` / `WaylandDndGrabHandler` are wired but always `cancel()` — emez doesn't participate in drag-and-drop; tests that need DnD semantics need a different host.
- `SelectionHandler::new_selection` runs **before** smithay writes the new selection into `seat_data` (see `wayland/selection/data_device/device.rs` in the smithay fork). So `request_data_device_client_selection` called directly inside the callback returns `NoSelection`. emez defers the drain to the next event-loop idle via `loop_handle.insert_idle` — by then smithay has finished setting up the selection and the drain succeeds.
- `set_data_device_selection(..., mimes, ())` cancels the previous client source. wl-copy's `cancelled_callback` is `exit(0)`, so draining + compositor takeover is what lets `wl-copy --foreground` exit cleanly (even without `--foreground`, the forked daemon gets cancelled too). Be careful not to cancel before the source has finished responding to earlier `send` events for the same drain — emez's drain issues `request_data_device_client_selection` (→ source.send) first, then `set_data_device_selection` (→ source.cancel), relying on wayland's in-order event delivery so the client writes all pipe fds before processing cancel.
- After drain+takeover, emez explicitly re-sets keyboard focus to `primary_fallback_focus`. smithay's `KeyboardHandle` doesn't auto-reset focus when the focused surface dies — the compositor has to drive it. This triggers smithay to re-broadcast the fresh compositor-owned selection to the newly-focused primary client.
