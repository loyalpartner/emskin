//! Pointer grabs. Currently only one — `MoveDialogGrab` — used by
//! the floating-dialog path so users can drag wechat-style login
//! boxes around. Mirrors anvil's `PointerMoveSurfaceGrab`
//! (smithay/anvil/src/shell/grabs.rs:27) but limited to the windows
//! the compositor itself owns the layout for; embedded apps and
//! Emacs are managed elsewhere and never participate.

use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
};

use crate::EmskinState;

pub struct MoveDialogGrab {
    pub start_data: GrabStartData<EmskinState>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
}

impl PointerGrab<EmskinState> for MoveDialogGrab {
    fn motion(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // Suppress pointer focus while dragging — the dialog tracks
        // the pointer wholesale, no other surface should think it's
        // hovered.
        handle.motion(data, None, event);

        let delta = event.location - self.start_data.location;
        let new_location = self.initial_window_location.to_f64() + delta;
        data.workspace.active_space.map_element(
            self.window.clone(),
            new_location.to_i32_round::<i32>(),
            true,
        );
    }

    fn relative_motion(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut EmskinState, handle: &mut PointerInnerHandle<'_, EmskinState>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut EmskinState,
        handle: &mut PointerInnerHandle<'_, EmskinState>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<EmskinState> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut EmskinState) {}
}
