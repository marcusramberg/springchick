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
/// `(x, y)`, returning it with its origin in global space and its coordinate
/// scale. The scale (`dpi` — OSK layer surfaces render at fractional scale
/// `dpi`, app surfaces at output scale `dpi`) is what physical input coords are
/// divided by to reach the surface's logical space. Checks Top/Overlay layer
/// surfaces (the OSK); everything else
/// falls through to the gesture funnel by returning `None`.
fn surface_under(state: &State, x: f32, y: f32) -> Option<(WlSurface, (f64, f64), f64)> {
    // 0. Open popups (menus, dropdowns) sit above everything and win, topmost
    //    first. They render at output scale `dpi`, same mapping as apps.
    if let Some(hit) = popup_under(state, x, y) {
        return Some(hit);
    }
    // 1. Top/Overlay layer surfaces (the OSK) win. They render at fractional
    //    scale `dpi`, so their logical coord space is physical/dpi — map input by
    //    /dpi. The rect origin is physical, so surface-local = (input-origin)/dpi.
    if let Some((surface, (ox, oy))) = state.layers.hit_test(x, y, state.dpi) {
        return Some((surface, (ox as f64, oy as f64), state.dpi as f64));
    }
    // 2. The focused fullscreen app, except the bottom bar zone (home gesture).
    //    App surfaces render at `dpi`, so input maps into logical space by /dpi.
    //    The app is drawn at the usable-area origin (below a top bar / right of
    //    a left bar), so its input origin must match, not (0, 0).
    if let crate::ui_state::UiState::App { toplevel, .. } = &state.ui {
        let (w, h) = state.output_size_f();
        let bar = sc_layout::bar_rect(w, h);
        if !bar.contains(x, y) {
            if let Some(Some(tl)) = state.toplevels.get(*toplevel) {
                let u = state.layers.usable(state.dpi);
                let origin = (u.x as f64, u.y as f64);
                return Some((tl.surface.wl_surface().clone(), origin, state.dpi as f64));
            }
        }
    }
    None
}

/// Convert a physical output-pixel point into a surface's local logical space.
fn to_local(x: f32, y: f32, scale: f64) -> Point<f64, smithay::utils::Logical> {
    Point::from((x as f64 / scale, y as f64 / scale))
}

/// Whether physical point `(x, y)` falls inside a popup's physical rect.
fn rect_contains(origin: (i32, i32), size: (i32, i32), x: f32, y: f32) -> bool {
    let (ox, oy) = (origin.0 as f32, origin.1 as f32);
    x >= ox && y >= oy && x < ox + size.0 as f32 && y < oy + size.1 as f32
}

/// Topmost open popup under `(x, y)`, with its physical origin and coord scale.
fn popup_under(state: &State, x: f32, y: f32) -> Option<(WlSurface, (f64, f64), f64)> {
    let popups = state.active_popups();
    let i = popups
        .iter()
        .rposition(|(_, origin, size)| rect_contains(*origin, *size, x, y))?;
    let (kind, origin, _) = &popups[i];
    Some((
        kind.wl_surface().clone(),
        (origin.0 as f64, origin.1 as f64),
        state.dpi as f64,
    ))
}

/// What a press should do with respect to open popups.
enum PopupPress {
    /// No popups open — fall through to normal surface routing.
    None,
    /// The tap landed outside every popup: the chain was dismissed and the tap
    /// must be swallowed (popup grab semantics).
    Consumed,
    /// The tap hit a popup; route input into it. Any submenus above it were
    /// dismissed first.
    Route(WlSurface, (f64, f64), f64),
}

/// Resolve a press against open popups: dismiss the chain (fully on an
/// outside-tap, or just the descendants of the popup that was hit) and decide
/// whether to route into a popup or swallow the tap.
fn popup_press(state: &mut State, x: f32, y: f32) -> PopupPress {
    let popups = state.active_popups();
    if popups.is_empty() {
        return PopupPress::None;
    }
    let hit = popups
        .iter()
        .rposition(|(_, origin, size)| rect_contains(*origin, *size, x, y));
    for i in crate::popups::popups_to_dismiss(popups.len(), hit) {
        if let smithay::desktop::PopupKind::Xdg(popup) = &popups[i].0 {
            popup.send_popup_done();
        }
    }
    state.needs_render = true;
    match hit {
        Some(i) => {
            let (kind, origin, _) = &popups[i];
            PopupPress::Route(
                kind.wl_surface().clone(),
                (origin.0 as f64, origin.1 as f64),
                state.dpi as f64,
            )
        }
        None => PopupPress::Consumed,
    }
}

/// Pointer moved to `(x, y)` (winit/desktop). Forwards to a client surface under
/// the cursor while a press is held on it, else drives gestures.
pub fn pointer_motion(state: &mut State, x: f32, y: f32, time: u32) {
    state.last_pointer_pos = Some((x, y));
    if state.pointer_grab {
        if let Some((surface, origin, scale)) = surface_under(state, x, y) {
            let ptr = state.seat.get_pointer().unwrap();
            let event = PointerMotionEvent {
                location: to_local(x, y, scale),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            };
            let focus = Point::from((origin.0 / scale, origin.1 / scale));
            ptr.motion(state, Some((surface, focus)), &event);
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
        let target = match popup_press(state, x, y) {
            PopupPress::Consumed => return,
            PopupPress::Route(surface, origin, scale) => Some((surface, origin, scale)),
            PopupPress::None => surface_under(state, x, y),
        };
        if let Some((surface, origin, scale)) = target {
            state.input_scale = scale;
            let ptr = state.seat.get_pointer().unwrap();
            // Enter/position the pointer, then press.
            let focus = Point::from((origin.0 / scale, origin.1 / scale));
            ptr.motion(
                state,
                Some((surface, focus)),
                &PointerMotionEvent {
                    location: to_local(x, y, scale),
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
        // A popup grab cancelled this press mid-flight; drop its release.
        if std::mem::take(&mut state.swallow_release) {
            return;
        }
        input_common::on_release(state);
    }
}

/// A finger touched down at output-pixel `(x, y)`.
pub fn down(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    let target = match popup_press(state, x, y) {
        PopupPress::Consumed => return,
        PopupPress::Route(surface, origin, scale) => Some((surface, origin, scale)),
        PopupPress::None => surface_under(state, x, y),
    };
    if let Some((surface, origin, scale)) = target {
        state.touch_grab = Some(surface.clone());
        state.input_scale = scale;
        let touch = state.touch.clone();
        let event = DownEvent {
            slot,
            location: to_local(x, y, scale),
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        let focus = Point::from((origin.0 / scale, origin.1 / scale));
        touch.down(state, Some((surface, focus)), &event);
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
            location: to_local(x, y, state.input_scale),
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
    // A popup grab cancelled this sequence mid-flight; drop its release.
    if std::mem::take(&mut state.swallow_release) {
        return;
    }
    input_common::on_release(state);
}
