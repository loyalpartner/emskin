use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyboardKeyEvent, MouseButton, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::keysyms,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::Resource,
    utils::SERIAL_COUNTER,
    wayland::seat::WaylandFocus,
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
                let (is_prefix, mods_changed) = keyboard.input_intercept(
                    self,
                    event.key_code(),
                    event.state(),
                    |_state, modifiers, keysym_handle| {
                        let Some(sym) = keysym_handle.raw_latin_sym_or_raw_current_sym() else {
                            return false;
                        };
                        let key = sym.raw();
                        (modifiers.ctrl && matches!(key, keysyms::KEY_x | keysyms::KEY_c))
                            || (modifiers.alt && key == keysyms::KEY_x)
                    },
                );

                if is_prefix && self.focus.prefix_saved_focus.is_none() {
                    self.focus.prefix_saved_focus = Some(keyboard.current_focus());
                    if let Some(emacs) = self.emacs_surface.clone() {
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

            InputEvent::PointerMotion { .. } => {}

            InputEvent::PointerMotionAbsolute { event, .. } => {
                let Some(output) = self.space.outputs().next() else {
                    return;
                };
                let Some(output_geo) = self.space.output_geometry(output) else {
                    return;
                };
                let Some(pointer) = self.seat.get_pointer() else {
                    return;
                };
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();
                let under = self.surface_under(pos);

                if tracing::enabled!(tracing::Level::DEBUG) {
                    let new_id = under.as_ref().map(|(s, _)| s.id());
                    let old_id = pointer.current_focus().map(|s| s.id());
                    if new_id != old_id {
                        let loc = under.as_ref().map(|(_, p)| *p);
                        tracing::debug!(
                            "pointer focus change: {:?} -> {:?} pos=({:.0},{:.0}) loc={:?}",
                            old_id,
                            new_id,
                            pos.x,
                            pos.y,
                            loc,
                        );
                    }
                }

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
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

                // Skeleton panel click interception. Left-button press on
                // a visible label → flash the target rect, send
                // SkeletonClicked IPC, and swallow both the press and its
                // paired release so the downstream surface sees no click.
                if event.button() == Some(MouseButton::Left) {
                    if button_state == ButtonState::Pressed {
                        let pos = pointer.current_location();
                        if let Some(rect) = self.skeleton.click_at(pos) {
                            tracing::debug!(
                                "skeleton label click: kind={} label={:?} ({},{}) {}x{}",
                                rect.kind,
                                rect.label,
                                rect.x,
                                rect.y,
                                rect.w,
                                rect.h,
                            );
                            self.ipc.send(crate::ipc::OutgoingMessage::SkeletonClicked {
                                kind: rect.kind,
                                label: rect.label,
                                x: rect.x,
                                y: rect.y,
                                w: rect.w,
                                h: rect.h,
                            });
                            self.skeleton_click_absorbed = true;
                            return;
                        }
                    } else if self.skeleton_click_absorbed {
                        // Absorbed press was followed by its release; drop it.
                        self.skeleton_click_absorbed = false;
                        return;
                    }
                }

                // Workspace bar: click on a button → switch workspace.
                if event.button() == Some(MouseButton::Left)
                    && button_state == ButtonState::Pressed
                    && self.bar_enabled
                {
                    let pos = pointer.current_location();
                    tracing::debug!(
                        "bar click check: pos=({:.0},{:.0}) visible={} buttons={}",
                        pos.x,
                        pos.y,
                        self.workspace_bar.visible(),
                        self.workspace_bar.button_count(),
                    );
                    if let Some(ws_id) = self.workspace_bar.click_at(pos) {
                        tracing::info!("bar click → workspace {ws_id}");
                        if ws_id != self.active_workspace_id && self.switch_workspace(ws_id) {
                            self.ipc
                                .send(crate::ipc::OutgoingMessage::WorkspaceSwitched {
                                    workspace_id: ws_id,
                                });
                        }
                        return;
                    }
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
                    let focus = under.map(|(s, _)| s).or_else(|| self.emacs_surface.clone());

                    // Left-click on an embedded app → tell Emacs to select that window.
                    if event.button() == Some(MouseButton::Left) {
                        if let Some((window_id, view_id, _)) =
                            self.apps.mirror_under(pos, self.active_workspace_id)
                        {
                            self.ipc.send(crate::ipc::OutgoingMessage::FocusView {
                                window_id,
                                view_id,
                            });
                        } else if let Some(window_id) =
                            focus.as_ref().and_then(|s| self.apps.id_for_surface(s))
                        {
                            self.ipc.send(crate::ipc::OutgoingMessage::FocusView {
                                window_id,
                                view_id: 0,
                            });
                        }
                    }

                    // Only change keyboard focus when clicking a different client.
                    // Clicking a popup surface from the same client (e.g. Firefox
                    // menu) must NOT send wl_keyboard.leave to the toplevel —
                    // otherwise the client dismisses the popup before processing
                    // the button event.
                    let same_client = focus.as_ref().is_some_and(|new| {
                        keyboard
                            .current_focus()
                            .is_some_and(|old| old.same_client_as(&new.id()))
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
}
