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
use smithay::input::pointer::{ButtonEvent, MotionEvent as PointerMotionEvent};
use smithay::input::touch::{DownEvent, MotionEvent, UpEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Point, SERIAL_COUNTER};

/// Resolve which client surface (if any) should receive input at output-pixel
/// `(x, y)`, returning it with its origin in global space. Checks Top/Overlay
/// layer surfaces (the OSK); everything else falls through to the gesture
/// funnel by returning `None`.
fn surface_under(state: &State, x: f32, y: f32) -> Option<(WlSurface, (f64, f64))> {
    // 1. Top/Overlay layer surfaces (the OSK) win.
    if let Some(m) = state.layers.hit_test(x, y) {
        return Some((
            m.surface.wl_surface().clone(),
            (m.rect.x as f64, m.rect.y as f64),
        ));
    }
    // 2. The focused fullscreen app, except the bottom bar zone (home gesture).
    if let crate::ui_state::UiState::App { toplevel, .. } = &state.ui {
        let bar = sc_layout::bar_rect(state.output_size.0 as f32, state.output_size.1 as f32);
        if !bar.contains(x, y) {
            if let Some(Some(tl)) = state.toplevels.get(*toplevel) {
                return Some((tl.surface.wl_surface().clone(), (0.0, 0.0)));
            }
        }
    }
    None
}

/// Pointer moved to `(x, y)` (winit/desktop). Forwards to a client surface under
/// the cursor while a press is held on it, else drives gestures.
pub fn pointer_motion(state: &mut State, x: f32, y: f32, time: u32) {
    state.last_pointer_pos = Some((x, y));
    if state.pointer_grab {
        if let Some((surface, origin)) = surface_under(state, x, y) {
            let ptr = state.seat.get_pointer().unwrap();
            let event = PointerMotionEvent {
                location: Point::from((x as f64, y as f64)),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            };
            ptr.motion(state, Some((surface, Point::from(origin))), &event);
            ptr.frame(state);
            return;
        }
    }
    input_common::on_motion(state, x, y);
}

/// Pointer button changed (winit/desktop).
pub fn pointer_button(state: &mut State, pressed: bool, button: u32, time: u32) {
    let (x, y) = state.last_pointer_pos.unwrap_or((0.0, 0.0));
    if pressed {
        if let Some((surface, origin)) = surface_under(state, x, y) {
            let ptr = state.seat.get_pointer().unwrap();
            // Enter/position the pointer, then press.
            ptr.motion(
                state,
                Some((surface, Point::from(origin))),
                &PointerMotionEvent {
                    location: Point::from((x as f64, y as f64)),
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            ptr.button(
                state,
                &ButtonEvent {
                    button,
                    state: smithay::backend::input::ButtonState::Pressed,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            ptr.frame(state);
            state.pointer_grab = true;
            return;
        }
        input_common::on_press(state);
    } else {
        if state.pointer_grab {
            let ptr = state.seat.get_pointer().unwrap();
            ptr.button(
                state,
                &ButtonEvent {
                    button,
                    state: smithay::backend::input::ButtonState::Released,
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
            ptr.frame(state);
            state.pointer_grab = false;
            return;
        }
        input_common::on_release(state);
    }
}

/// A finger touched down at output-pixel `(x, y)`.
pub fn down(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    if let Some((surface, origin)) = surface_under(state, x, y) {
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
    // Not on a client surface — drive the gesture system.
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
