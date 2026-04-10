use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyboardKeyEvent, MouseButton, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::keysyms,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::SERIAL_COUNTER,
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

                if is_prefix && self.prefix_saved_focus.is_none() {
                    self.prefix_saved_focus = Some(keyboard.current_focus());
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

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    let pos = pointer.current_location();
                    let clicked = self.surface_under(pos).map(|(s, _)| s);
                    let focus = clicked.or_else(|| self.emacs_surface.clone());

                    // Left-click on an EAF app → tell Emacs to select that window.
                    if event.button() == Some(MouseButton::Left) {
                        if let Some((window_id, view_id, _)) = self.apps.mirror_under(pos) {
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

                    keyboard.set_focus(self, focus, serial);
                    self.prefix_saved_focus = None;
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
