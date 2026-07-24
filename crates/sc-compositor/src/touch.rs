//! Touch routing: layer surfaces first, then the home gesture funnel.
//!
//! A touch-down is hit-tested against the visible Top/Overlay layer surfaces
//! (e.g. the on-screen keyboard). If it lands on one, the whole sequence
//! (down/motion/up) is forwarded to that client via the seat `wl_touch`, and the
//! gesture system never sees it. Otherwise it flows into the existing gesture
//! funnel unchanged.

use crate::input_common;
use crate::State;
use smithay::backend::input::TouchSlot;
use smithay::input::touch::{DownEvent, MotionEvent, UpEvent};
use smithay::utils::{Point, SERIAL_COUNTER};

/// A finger touched down at output-pixel `(x, y)`.
pub fn down(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    if let Some(m) = state.layers.hit_test(x, y) {
        let surface = m.surface.wl_surface().clone();
        let origin = (m.rect.x as f64, m.rect.y as f64);
        state.touch_grab = Some(surface.clone());
        let touch = state.touch.clone();
        let event = DownEvent {
            slot,
            location: Point::from((x as f64, y as f64)),
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        touch.down(state, Some((surface, Point::from(origin))), &event);
        touch.frame(state);
        return;
    }
    // Not on a layer surface — drive the gesture system.
    input_common::on_motion(state, x, y);
    input_common::on_press(state);
}

/// A finger moved to `(x, y)`.
pub fn motion(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    if state.touch_grab.is_some() {
        let touch = state.touch.clone();
        let event = MotionEvent {
            slot,
            location: Point::from((x as f64, y as f64)),
            time,
        };
        // Focus is only used for DnD during motion; we pass none.
        touch.motion(state, None, &event);
        touch.frame(state);
        return;
    }
    input_common::on_motion(state, x, y);
}

/// A finger lifted.
pub fn up(state: &mut State, slot: TouchSlot, time: u32) {
    if state.touch_grab.take().is_some() {
        let touch = state.touch.clone();
        let event = UpEvent {
            slot,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        touch.up(state, &event);
        touch.frame(state);
        return;
    }
    input_common::on_release(state);
}
