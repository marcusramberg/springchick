//! Touch and pointer routing: client surfaces first, then the gesture funnel.
//! Client events aren't flushed here; the backend calls [`frame`] and
//! [`cancel`].

use crate::input_common;
use crate::touch_viz;
use crate::State;
use smithay::backend::input::TouchSlot;
use smithay::backend::input::{Axis, AxisRelativeDirection, AxisSource};
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::WindowSurfaceType;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent as PointerMotionEvent};
use smithay::input::touch::{DownEvent, MotionEvent, UpEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Point, SERIAL_COUNTER};

/// The client surface at output pixel `(x, y)`, with its origin and the
/// scale (`dpi`) that maps physical input into its logical space. `None`
/// falls through to the gesture funnel.
fn surface_under(state: &State, x: f32, y: f32) -> Option<Target> {
    root_under(state, x, y).map(|t| descend(t, x, y))
}

/// `wl_touch`/`wl_pointer` enter must name the subsurface hit; Firefox's
/// doorhangers are subsurfaces and take no input otherwise. Respects input
/// regions.
fn descend(t: Target, x: f32, y: f32) -> Target {
    let local = to_local(t.scale, x, y) - t.focus();
    match under_from_surface_tree(&t.surface, local, (0, 0), WindowSurfaceType::ALL) {
        Some((surface, loc)) if surface != t.surface => Target {
            surface,
            origin: (
                t.origin.0 + loc.x as f64 * t.scale,
                t.origin.1 + loc.y as f64 * t.scale,
            ),
            scale: t.scale,
        },
        _ => t,
    }
}

fn root_under(state: &State, x: f32, y: f32) -> Option<Target> {
    // Locked: only the lock surface, or nowhere. Never the shell.
    if state.session_lock.is_locked() {
        return state
            .session_lock
            .wl_surface()
            .map(|s| Target::at(s.clone(), (0.0, 0.0), state.dpi));
    }
    // Popups first, topmost wins.
    if let Some(hit) = popup_under(state, x, y) {
        return Some(hit);
    }
    // Top/Overlay layers (OSK). Not drawn while the view is turned, so not
    // hit-tested either.
    if !state.view_rotation().swaps_axes() {
        if let Some((surface, (ox, oy))) = state.layers.hit_test(x, y, state.dpi) {
            return Some(Target::at(surface, (ox as f64, oy as f64), state.dpi));
        }
        if !state.foreground_is_fullscreen() {
            if let Some((surface, (ox, oy))) = state.layers.hit_test_bottom(x, y, state.dpi) {
                return Some(Target::at(surface, (ox as f64, oy as f64), state.dpi));
            }
        }
    }
    // The focused app, minus the bar zone. It's drawn at the usable-area
    // origin, not (0, 0).
    if let crate::ui_state::UiState::App { toplevel, .. } = &state.ui {
        let (w, h) = state.output_size_f();
        let bar = sc_layout::bar_rect(w, h);
        if !bar.contains(x, y) {
            if let Some(Some(tl)) = state.toplevels.get(*toplevel) {
                let (ox, oy) = state.app_origin();
                return Some(Target::at(
                    tl.surface.wl_surface().clone(),
                    (ox as f64, oy as f64),
                    state.dpi,
                ));
            }
        }
    }
    None
}

/// Positive `i32` range, so it never aliases [`touch_viz::POINTER_ID`].
fn slot_id(slot: TouchSlot) -> u64 {
    i32::from(slot) as u64
}

struct Target {
    surface: WlSurface,
    origin: (f64, f64),
    scale: f64,
}

impl Target {
    fn at(surface: WlSurface, origin: (f64, f64), scale: f64) -> Self {
        Target {
            surface,
            origin,
            scale,
        }
    }

    /// What smithay subtracts to get surface-local coordinates.
    fn focus(&self) -> Point<f64, smithay::utils::Logical> {
        Point::from((self.origin.0 / self.scale, self.origin.1 / self.scale))
    }
}

fn to_local(scale: f64, x: f32, y: f32) -> Point<f64, smithay::utils::Logical> {
    Point::from((x as f64 / scale, y as f64 / scale))
}

/// The one place input is rotated into view space.
fn to_view(state: &State, x: f32, y: f32) -> (f32, f32) {
    state.view_rotation().map_input(x, y, state.panel_size)
}

fn rect_contains(origin: (i32, i32), size: (i32, i32), x: f32, y: f32) -> bool {
    let (ox, oy) = (origin.0 as f32, origin.1 as f32);
    x >= ox && y >= oy && x < ox + size.0 as f32 && y < oy + size.1 as f32
}

fn popup_under(state: &State, x: f32, y: f32) -> Option<Target> {
    let popups = state.active_popups();
    let i = popups
        .iter()
        .rposition(|(_, origin, size)| rect_contains(*origin, *size, x, y))?;
    let (kind, origin, _) = &popups[i];
    Some(Target {
        surface: kind.wl_surface().clone(),
        origin: (origin.0 as f64, origin.1 as f64),
        scale: state.dpi,
    })
}

enum PopupPress {
    /// No popup hit and none grabbing: the tap falls through. A non-grab popup
    /// stays open; the client dismisses it itself.
    None,
    /// Missed a grabbing chain: dismissed, and the tap is swallowed.
    Consumed,
    /// Grabbing submenus above the hit were dismissed first.
    Route(Target),
}

/// Hit-testing considers every popup; dismissal only grabbing ones. Non-grab
/// popups (wvkbd's hack popup, Firefox menus, tooltips) never swallow an
/// outside tap.
fn popup_press(state: &mut State, x: f32, y: f32) -> PopupPress {
    // Nothing is shown while locked; `surface_under` routes to the lock.
    if state.session_lock.is_locked() {
        return PopupPress::None;
    }
    let popups = state.active_popups();
    if popups.is_empty() {
        return PopupPress::None;
    }
    let grabs: Vec<bool> = popups
        .iter()
        .map(|(kind, _, _)| state.popup_has_grab(kind.wl_surface()))
        .collect();
    let hit = popups
        .iter()
        .rposition(|(_, origin, size)| rect_contains(*origin, *size, x, y));
    // Never force-close non-grab popups.
    let dismiss: Vec<usize> = crate::popups::popups_to_dismiss(popups.len(), hit)
        .into_iter()
        .filter(|&i| grabs[i])
        .collect();
    for &i in &dismiss {
        if let smithay::desktop::PopupKind::Xdg(popup) = &popups[i].0 {
            popup.send_popup_done();
        }
    }
    if !dismiss.is_empty() {
        state.needs_render = true;
    }
    match hit {
        Some(i) => {
            let (kind, origin, _) = &popups[i];
            let t = Target {
                surface: kind.wl_surface().clone(),
                origin: (origin.0 as f64, origin.1 as f64),
                scale: state.dpi,
            };
            PopupPress::Route(descend(t, x, y))
        }
        None if dismiss.is_empty() => PopupPress::None,
        None => PopupPress::Consumed,
    }
}

/// Only left-click drives shell gestures; right/middle on Home would launch
/// the icon under the cursor.
const BTN_LEFT: u32 = 0x110;

/// Screen centre before any pointer event; (0, 0) would aim at a corner.
fn cursor_pos(state: &State) -> (f32, f32) {
    let (w, h) = state.output_size_f();
    state.last_pointer_pos.unwrap_or((w * 0.5, h * 0.5))
}

/// `None` leaves the current surface.
fn send_motion(state: &mut State, target: Option<Target>, x: f32, y: f32, time: u32) {
    let ptr = state.seat.get_pointer().unwrap();
    let (focus, location) = match &target {
        Some(t) => (
            Some((t.surface.clone(), t.focus())),
            to_local(t.scale, x, y),
        ),
        None => (None, Point::from((x as f64, y as f64))),
    };
    ptr.motion(
        state,
        focus,
        &PointerMotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    ptr.frame(state);
}

pub fn pointer_motion(state: &mut State, x: f32, y: f32, time: u32) {
    let (x, y) = to_view(state, x, y);
    pointer_motion_view(state, x, y, time);
}

fn pointer_motion_view(state: &mut State, x: f32, y: f32, time: u32) {
    state.last_pointer_pos = Some((x, y));
    if !state.cursor_visible {
        state.cursor_visible = true;
    }
    state.needs_render = true;
    // Only mark while a button is held, so bare hover leaves no trace.
    if state.show_touches && (state.pointer_grab || state.pointer_down) {
        state
            .touch_viz
            .contact(touch_viz::POINTER_ID, x, y, std::time::Instant::now());
    }
    if state.pointer_grab {
        if let Some(target) = surface_under(state, x, y) {
            send_motion(state, Some(target), x, y, time);
            return;
        }
    }
    // Hover enters/leaves client surfaces, except while a shell gesture owns
    // the press.
    if !state.pointer_down {
        let target = surface_under(state, x, y);
        send_motion(state, target, x, y, time);
    }
    if state.session_lock.is_locked() {
        return;
    }
    input_common::on_motion(state, x, y);
}

pub fn pointer_button(state: &mut State, pressed: bool, button: u32, time: u32) {
    let (x, y) = cursor_pos(state);
    state.cursor_visible = true;
    if state.show_touches {
        if pressed {
            state
                .touch_viz
                .contact(touch_viz::POINTER_ID, x, y, std::time::Instant::now());
        } else {
            state
                .touch_viz
                .release(touch_viz::POINTER_ID, std::time::Instant::now());
        }
        state.needs_render = true;
    }
    if pressed {
        let target = match popup_press(state, x, y) {
            PopupPress::Consumed => return,
            PopupPress::Route(target) => Some(target),
            PopupPress::None => surface_under(state, x, y),
        };
        if let Some(target) = target {
            let ptr = state.seat.get_pointer().unwrap();
            let focus = target.focus();
            let location = to_local(target.scale, x, y);
            ptr.motion(
                state,
                Some((target.surface, focus)),
                &PointerMotionEvent {
                    location,
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
        // Locked with no lock surface: drop it, never funnel it.
        if state.session_lock.is_locked() {
            return;
        }
        if button != BTN_LEFT {
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
        if state.session_lock.is_locked() {
            return;
        }
        if button != BTN_LEFT {
            return;
        }
        input_common::on_release(state);
    }
}

/// Accumulates relative motion (e.g. a Bluetooth mouse ring) into the cursor.
pub fn pointer_motion_relative(state: &mut State, dx: f64, dy: f64, time: u32) {
    let (w, h) = state.output_size_f();
    let (mut x, mut y) = cursor_pos(state);
    // Hit-testing is half-open, so clamp to `w - 1`, not `w`.
    x = (x + dx as f32).clamp(0.0, (w - 1.0).max(0.0));
    y = (y + dy as f32).clamp(0.0, (h - 1.0).max(0.0));
    pointer_motion_view(state, x, y, time);
}

#[derive(Clone, Copy, Debug)]
pub struct AxisMotion {
    /// Logical px, for finger and continuous sources. Zero means scrolling
    /// stopped and is sent as `axis_stop`.
    pub amount: Option<f64>,
    /// 1/120ths of a click, forwarded unscaled as `axis_value120`.
    pub v120: Option<f64>,
    pub direction: AxisRelativeDirection,
}

/// Both axes in one frame. `amount` is `Some(0.0)` on an untouched axis, so
/// picking "the non-`None` one" is wrong.
pub fn pointer_axis_event<B, E>(state: &mut State, event: &E, time: u32)
where
    B: smithay::backend::input::InputBackend,
    E: smithay::backend::input::PointerAxisEvent<B>,
{
    let source = event.source();
    let read = |axis: Axis| {
        let amount = event.amount(axis);
        let v120 = event.amount_v120(axis);
        if amount.is_none() && v120.is_none() {
            return None;
        }
        Some(AxisMotion {
            amount,
            v120,
            direction: event.relative_direction(axis),
        })
    };
    pointer_axis(
        state,
        source,
        read(Axis::Horizontal),
        read(Axis::Vertical),
        time,
    );
}

/// Dropped when no client is under the cursor; the shell has no scroll
/// gestures.
pub fn pointer_axis(
    state: &mut State,
    source: AxisSource,
    horizontal: Option<AxisMotion>,
    vertical: Option<AxisMotion>,
    time: u32,
) {
    if horizontal.is_none() && vertical.is_none() {
        return;
    }
    let (x, y) = cursor_pos(state);
    let Some(target) = surface_under(state, x, y) else {
        return;
    };
    send_motion(state, Some(target), x, y, time);

    let mut frame = AxisFrame::new(time).source(source);
    for (axis, motion) in [(Axis::Horizontal, horizontal), (Axis::Vertical, vertical)] {
        let Some(m) = motion else { continue };
        frame = frame.relative_direction(axis, m.direction);
        match m.amount {
            // Kinetic scrolling waits for `axis_stop`; a zero value won't end it.
            Some(v) if v == 0.0 && matches!(source, AxisSource::Finger) => {
                frame = frame.stop(axis);
            }
            Some(v) => frame = frame.value(axis, v),
            None => {}
        }
        if let Some(v120) = m.v120 {
            frame = frame.v120(axis, v120 as i32);
        }
    }
    let ptr = state.seat.get_pointer().unwrap();
    ptr.axis(state, frame);
    ptr.frame(state);
}

/// Per slot: a slot on a client surface goes there; on empty space only the
/// first slot (`gesture_slot`) drives the single-touch funnel.
pub fn down(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    let (x, y) = to_view(state, x, y);
    state.last_touch_pos = Some((x, y));
    if state.cursor_visible {
        state.cursor_visible = false;
        state.needs_render = true;
    }
    if state.show_touches {
        state
            .touch_viz
            .contact(slot_id(slot), x, y, std::time::Instant::now());
        state.needs_render = true;
    }
    let target = match popup_press(state, x, y) {
        PopupPress::Consumed => return,
        PopupPress::Route(target) => Some(target),
        PopupPress::None => surface_under(state, x, y),
    };
    if let Some(target) = target {
        // Presence marks the slot client-routed.
        state.touch_targets.insert(slot, target.scale);
        state.layers.note_tap(&target.surface);
        let touch = state.touch.clone();
        let event = DownEvent {
            slot,
            location: to_local(target.scale, x, y),
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        let focus = target.focus();
        touch.down(state, Some((target.surface, focus)), &event);
        return;
    }
    // Locked: no gestures, or a swipe could open Home behind the lock.
    if state.gesture_slot.is_none() && !state.session_lock.is_locked() {
        state.gesture_slot = Some(slot);
        input_common::on_motion(state, x, y);
        input_common::on_press(state);
    }
}

pub fn motion(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    let (x, y) = to_view(state, x, y);
    state.last_touch_pos = Some((x, y));
    if state.show_touches {
        state
            .touch_viz
            .contact(slot_id(slot), x, y, std::time::Instant::now());
        state.needs_render = true;
    }
    if let Some(&scale) = state.touch_targets.get(&slot) {
        let touch = state.touch.clone();
        let location = to_local(scale, x, y);
        let event = MotionEvent {
            slot,
            location,
            time,
        };
        touch.motion(state, None, &event);
        return;
    }
    if state.gesture_slot == Some(slot) {
        input_common::on_motion(state, x, y);
    }
}

/// libinput `TOUCH_FRAME`. Batching on the real frame keeps multi-finger
/// updates atomic for toolkit pinch/scroll recognition. Safe with nothing
/// pending.
pub fn frame(state: &mut State) {
    let touch = state.touch.clone();
    touch.frame(state);
}

/// libinput `TOUCH_CANCEL`. Without this `gesture_slot` stays claimed and
/// the next swipe is dropped, and client slots keep phantom contacts.
/// `wl_touch.cancel` is seat-wide, so every slot is cancelled.
pub fn cancel(state: &mut State) {
    if state.show_touches {
        let now = std::time::Instant::now();
        let slots: Vec<TouchSlot> = state
            .touch_targets
            .keys()
            .copied()
            .chain(state.gesture_slot)
            .collect();
        for slot in slots {
            state.touch_viz.release(slot_id(slot), now);
        }
    }
    let touch = state.touch.clone();
    touch.cancel(state);
    state.cancel_gestures();
    state.needs_render = true;
}

pub fn up(state: &mut State, slot: TouchSlot, time: u32) {
    if state.show_touches {
        state
            .touch_viz
            .release(slot_id(slot), std::time::Instant::now());
        state.needs_render = true;
    }
    if state.touch_targets.remove(&slot).is_some() {
        let touch = state.touch.clone();
        let event = UpEvent {
            slot,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        touch.up(state, &event);
        return;
    }
    if state.gesture_slot == Some(slot) {
        state.gesture_slot = None;
        input_common::on_release(state);
    }
}
