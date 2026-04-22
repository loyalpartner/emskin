use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, MouseButton, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::{keysyms, xkb, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::Resource,
    utils::SERIAL_COUNTER,
    wayland::{
        pointer_constraints::{with_pointer_constraint, PointerConstraint},
        seat::WaylandFocus,
    },
};

use crate::state::EmskinState;

impl EmskinState {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);

                // Peek at keysym: only Emacs prefix keys (C-x, C-c, M-x)
                // redirect focus to Emacs; everything else goes to the focused app.
                // Side-effect: push the chord into key_cast on press only —
                // releases would clutter the overlay with duplicate labels.
                let pressed = matches!(event.state(), KeyState::Pressed);
                let (is_prefix, mods_changed) = keyboard.input_intercept(
                    self,
                    event.key_code(),
                    event.state(),
                    |state, modifiers, keysym_handle| {
                        let Some(sym) = keysym_handle.raw_latin_sym_or_raw_current_sym() else {
                            return false;
                        };
                        let key = sym.raw();

                        if pressed && !is_modifier_keysym(key) {
                            if let Some(label) = format_chord(modifiers, key) {
                                state.effects.key_cast.borrow_mut().push(label);
                            }
                        }

                        (modifiers.ctrl && matches!(key, keysyms::KEY_x | keysyms::KEY_c))
                            || (modifiers.alt && key == keysyms::KEY_x)
                    },
                );

                if is_prefix {
                    // Save original focus only once per prefix sequence.
                    // If prefix_saved_focus is already set (stale from a
                    // previous sequence whose prefix_done was lost), we
                    // still redirect to Emacs — otherwise the user gets
                    // stuck unable to use any prefix key.
                    if self.focus.prefix_saved_focus.is_none() {
                        self.focus.prefix_saved_focus = Some(keyboard.current_focus());
                    }
                    if let Some(emacs) = self.emacs_focus_target() {
                        if keyboard.current_focus().as_ref() != Some(&emacs) {
                            keyboard.set_focus(self, Some(emacs), SERIAL_COUNTER.next_serial());
                        }
                    }
                }

                keyboard.input_forward(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    mods_changed,
                );
            }

            // Smithay's winit backend never emits relative motion; we
            // synthesize a delta from successive absolutes in the
            // `PointerMotionAbsolute` arm below.
            InputEvent::PointerMotion { .. } => {}

            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.workspace.active_space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.workspace.active_space.output_geometry(output) else {
                    return;
                };
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let new_abs = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                // Diff against last raw host position to feed
                // zwp_relative_pointer_v1 — independent of
                // `pointer.current_location()`, which freezes under a
                // pointer lock while clients still need raw delta.
                let delta = self.cursor.consume_raw_location(new_abs);
                let time_msec = event.time_msec();
                let new_under = self.surface_under(new_abs);

                // Constraints attach per (surface, pointer) and only apply
                // while the pointer is focused on that surface — query the
                // focus directly instead of an extra surface_under(tracked)
                // walk.
                let constrained_surface = pointer.current_focus();
                let mut pointer_locked = false;
                let mut pointer_confined = false;
                let mut active_region = None;
                if let Some(surface) = constrained_surface.as_ref() {
                    with_pointer_constraint(surface, &pointer, |constraint| match constraint {
                        Some(c) if c.is_active() => match &*c {
                            PointerConstraint::Locked(l) => {
                                pointer_locked = true;
                                active_region = l.region().cloned();
                            }
                            PointerConstraint::Confined(c) => {
                                pointer_confined = true;
                                active_region = c.region().cloned();
                            }
                        },
                        _ => {}
                    });
                }

                // Rare: constraint restricts which sub-region of the surface
                // it applies to. Reuse `new_under`'s surface_loc when it
                // matches, otherwise pay for a second lookup.
                if let (Some(region), Some(surface)) =
                    (active_region.as_ref(), constrained_surface.as_ref())
                {
                    let tracked_loc = pointer.current_location();
                    let surface_loc = new_under
                        .as_ref()
                        .filter(|(s, _)| s == surface)
                        .map(|(_, loc)| *loc)
                        .or_else(|| {
                            self.surface_under(tracked_loc)
                                .filter(|(s, _)| s == surface)
                                .map(|(_, loc)| loc)
                        });
                    let in_region = surface_loc
                        .is_some_and(|loc| region.contains((tracked_loc - loc).to_i32_round()));
                    if !in_region {
                        pointer_locked = false;
                        pointer_confined = false;
                    }
                }

                // Always emit relative motion — no-op for clients that
                // haven't bound zwp_relative_pointer_v1, and the only signal
                // for clients that locked the pointer.
                pointer.relative_motion(
                    self,
                    new_under.clone(),
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        utime: time_msec as u64 * 1000,
                    },
                );

                if pointer_locked {
                    pointer.frame(self);
                    return;
                }

                let serial = SERIAL_COUNTER.next_serial();

                if pointer_confined {
                    if let Some(surface) = constrained_surface.as_ref() {
                        let leaves_surface = new_under
                            .as_ref()
                            .map(|(s, _)| s != surface)
                            .unwrap_or(true);
                        let leaves_region = active_region.as_ref().is_some_and(|r| {
                            new_under
                                .as_ref()
                                .filter(|(s, _)| s == surface)
                                .is_some_and(|(_, loc)| {
                                    !r.contains((new_abs - *loc).to_i32_round())
                                })
                        });
                        if leaves_surface || leaves_region {
                            pointer.frame(self);
                            return;
                        }
                    }
                }

                if tracing::enabled!(tracing::Level::DEBUG) {
                    let new_id = new_under.as_ref().map(|(s, _)| s.id());
                    let old_id = pointer.current_focus().map(|s| s.id());
                    if new_id != old_id {
                        let loc = new_under.as_ref().map(|(_, p)| *p);
                        tracing::debug!(
                            "pointer focus change: {:?} -> {:?} pos=({:.0},{:.0}) loc={:?}",
                            old_id,
                            new_id,
                            new_abs.x,
                            new_abs.y,
                            loc,
                        );
                    }
                }

                pointer.motion(
                    self,
                    new_under.clone(),
                    &MotionEvent {
                        location: new_abs,
                        serial,
                        time: time_msec,
                    },
                );
                pointer.frame(self);

                // Smithay doesn't auto-activate — every surface enter must
                // be checked.
                if let Some((surface, surface_loc)) = new_under {
                    with_pointer_constraint(&surface, &pointer, |constraint| match constraint {
                        Some(c) if !c.is_active() => {
                            let point = (new_abs - surface_loc).to_i32_round();
                            if c.region().is_none_or(|r| r.contains(point)) {
                                c.activate();
                            }
                        }
                        _ => {}
                    });
                }
            }

            InputEvent::PointerButton { event, .. } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let Some(keyboard) = self.seat.get_keyboard() else {
                    return;
                };

                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();
                let button_state = event.state();

                // Window-manager-owned overlay hit-testing. emskin drives these
                // directly against the overlays' typed click methods — the
                // Effect trait itself has no input hook.
                if button_state == ButtonState::Pressed && event.button() == Some(MouseButton::Left)
                {
                    let pos = pointer.current_location();

                    // Skeleton label click → flash only (no outbound IPC).
                    // Scope the borrow so subsequent pointer-focus code can
                    // still reborrow `self`.
                    let skeleton_hit = {
                        let mut sk = self.effects.skeleton.borrow_mut();
                        sk.enabled() && sk.click_at(pos).is_some()
                    };
                    if skeleton_hit {
                        self.effects.skeleton_click_absorbed = true;
                        return;
                    }
                }
                if button_state == ButtonState::Released && self.effects.skeleton_click_absorbed {
                    self.effects.skeleton_click_absorbed = false;
                    return;
                }

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    let pos = pointer.current_location();
                    let under = self.surface_under(pos);
                    tracing::debug!(
                        "button press: pos=({:.0},{:.0}) under={:?} ptr_focus={:?}",
                        pos.x,
                        pos.y,
                        under.as_ref().map(|(s, _)| s.id()),
                        pointer.current_focus().map(|s| s.id()),
                    );
                    let under_surface = under.map(|(s, _)| s);

                    // Left-click on an embedded app → tell Emacs to select that window.
                    if event.button() == Some(MouseButton::Left) {
                        if let Some((window_id, view_id, _)) =
                            self.apps.mirror_under(pos, self.workspace.active_id)
                        {
                            self.ipc.send(crate::ipc::OutgoingMessage::FocusView {
                                window_id,
                                view_id,
                            });
                        } else if let Some(window_id) = under_surface
                            .as_ref()
                            .and_then(|s| self.apps.id_for_surface(s))
                        {
                            self.ipc.send(crate::ipc::OutgoingMessage::FocusView {
                                window_id,
                                view_id: 0,
                            });
                        }
                    }

                    let focus = under_surface
                        .as_ref()
                        .and_then(|s| self.focus_target_for_surface(s))
                        .or_else(|| self.emacs_focus_target());

                    // Only change keyboard focus when clicking a different client.
                    // Clicking a popup surface from the same client (e.g. Firefox
                    // menu) must NOT send wl_keyboard.leave to the toplevel —
                    // otherwise the client dismisses the popup before processing
                    // the button event.
                    let same_client = focus.as_ref().is_some_and(|new| {
                        keyboard.current_focus().is_some_and(|old| {
                            new.wl_surface()
                                .is_some_and(|s| old.same_client_as(&s.id()))
                        })
                    });
                    if !same_client {
                        keyboard.set_focus(self, focus, serial);
                        self.focus.prefix_saved_focus = None;
                    }
                }

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event, .. } => {
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let source = event.source();

                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                pointer.axis(self, frame);
                pointer.frame(self);
            }

            _ => {}
        }
    }

    /// emskin's winit window lost focus (Alt+Tab away, minimize, etc.).
    /// Save the current keyboard focus and clear it so embedded clients
    /// stop thinking they still have focus. `focus_changed` cascades the
    /// clear to IME, data_device, and primary_selection. Pointer focus
    /// is released here too (bundled with keyboard until winit gives us
    /// separate `CursorLeft` events — YAGNI for now).
    pub fn on_focus_leave(&mut self) {
        let serial = SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            self.focus.host_saved_focus = keyboard.current_focus();
            keyboard.set_focus(self, None, serial);
        }
        if let Some(pointer) = self.seat.get_pointer() {
            pointer.motion(
                self,
                None,
                &MotionEvent {
                    location: pointer.current_location(),
                    serial,
                    time: 0,
                },
            );
            pointer.frame(self);
        }
    }

    /// emskin's winit window regained focus. Restore the keyboard focus
    /// saved by `on_focus_leave`; the `focus_changed` cascade re-enables
    /// IME (if the restored client has text_input_v3 bound) and rewires
    /// data_device / primary_selection.
    pub fn on_focus_enter(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let Some(saved) = self.focus.host_saved_focus.take() else {
            return;
        };
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(saved), serial);
    }
}

/// Pure-modifier keysyms — pressing one alone shouldn't generate a
/// chord entry (Shift / Control / Alt / Super / AltGr).
fn is_modifier_keysym(key: u32) -> bool {
    matches!(
        key,
        keysyms::KEY_Shift_L
            | keysyms::KEY_Shift_R
            | keysyms::KEY_Control_L
            | keysyms::KEY_Control_R
            | keysyms::KEY_Alt_L
            | keysyms::KEY_Alt_R
            | keysyms::KEY_Meta_L
            | keysyms::KEY_Meta_R
            | keysyms::KEY_Super_L
            | keysyms::KEY_Super_R
            | keysyms::KEY_Hyper_L
            | keysyms::KEY_Hyper_R
            | keysyms::KEY_ISO_Level3_Shift
            | keysyms::KEY_Caps_Lock
            | keysyms::KEY_Num_Lock
    )
}

/// Build an Emacs-style chord label like `C-x`, `M-RET`, `<f5>`,
/// `s-SPC`. Returns `None` for keysyms we can't sensibly name (the
/// overlay just skips that press rather than showing a noise label).
fn format_chord(modifiers: &ModifiersState, key: u32) -> Option<String> {
    let key_name = keysym_label(key)?;

    let mut s = String::new();
    if modifiers.ctrl {
        s.push_str("C-");
    }
    if modifiers.alt {
        s.push_str("M-");
    }
    if modifiers.logo {
        s.push_str("s-");
    }
    // Single printable chars already encode shift in the keysym (`X` vs
    // `x`). Only show "S-" for multi-char names like "S-<f5>" or "S-RET".
    if modifiers.shift && key_name.len() > 1 {
        s.push_str("S-");
    }
    s.push_str(&key_name);
    Some(s)
}

/// Map an X11 keysym to a short human-readable label.
///
/// Naming follows Emacs's `single-key-description` convention (`RET`,
/// `<f5>`, `<deletechar>`) rather than xev / GTK's, so the on-screen
/// chord matches what Emacs users see in `C-h k` and the message area.
fn keysym_label(key: u32) -> Option<String> {
    let label = match key {
        keysyms::KEY_Return | keysyms::KEY_KP_Enter => "RET",
        keysyms::KEY_Tab | keysyms::KEY_ISO_Left_Tab => "TAB",
        keysyms::KEY_BackSpace => "DEL",
        keysyms::KEY_Delete | keysyms::KEY_KP_Delete => "<deletechar>",
        keysyms::KEY_Escape => "ESC",
        keysyms::KEY_space | keysyms::KEY_KP_Space => "SPC",
        keysyms::KEY_Up | keysyms::KEY_KP_Up => "<up>",
        keysyms::KEY_Down | keysyms::KEY_KP_Down => "<down>",
        keysyms::KEY_Left | keysyms::KEY_KP_Left => "<left>",
        keysyms::KEY_Right | keysyms::KEY_KP_Right => "<right>",
        keysyms::KEY_Page_Up | keysyms::KEY_KP_Page_Up => "<prior>",
        keysyms::KEY_Page_Down | keysyms::KEY_KP_Page_Down => "<next>",
        keysyms::KEY_Home | keysyms::KEY_KP_Home => "<home>",
        keysyms::KEY_End | keysyms::KEY_KP_End => "<end>",
        _ => {
            // F-keys span KEY_F1 ..= KEY_F35 contiguously.
            if (keysyms::KEY_F1..=keysyms::KEY_F35).contains(&key) {
                return Some(format!("<f{}>", key - keysyms::KEY_F1 + 1));
            }
            // Printable ASCII — round-trip through char.
            if (0x20..=0x7e).contains(&key) {
                return char::from_u32(key).map(|c| c.to_string());
            }
            // Fall back to xkbcommon's keysym name for less common keys
            // (e.g. multimedia keys). Returns "NoSymbol" for unknown — drop
            // those so the overlay isn't spammed with noise labels.
            let name = xkb::keysym_get_name(xkb::Keysym::new(key));
            if name == "NoSymbol" || name.is_empty() {
                return None;
            }
            return Some(format!("<{}>", name));
        }
    };
    Some(label.to_string())
}
