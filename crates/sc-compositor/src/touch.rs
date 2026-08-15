//! Touch routing: layer surfaces first, then the home gesture funnel.
//!
//! A touch-down is hit-tested against the visible Top/Overlay layer surfaces
//! (e.g. the on-screen keyboard). If it lands on one, the whole sequence
//! (down/motion/up) is forwarded to that client via the seat `wl_touch`, and the
//! gesture system never sees it. Otherwise it flows into the existing gesture
//! funnel unchanged.
//!
//! Client-bound events are not flushed here: the backend calls [`frame`] when
//! the input stack reports the end of a simultaneous batch, and [`cancel`] when
//! it abandons the sequence.

use crate::input_common;
use crate::touch_viz;
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
fn surface_under(state: &State, x: f32, y: f32) -> Option<Target> {
    // 0. A locked session routes everything to the lock surface and nothing
    //    else — no popups, no layers, no app. With no lock surface (client
    //    crashed, or hasn't made one yet) input goes nowhere at all; it must
    //    never fall through to the shell underneath.
    if state.session_lock.is_locked() {
        return state
            .session_lock
            .wl_surface()
            .map(|s| Target::at(s.clone(), (0.0, 0.0), state.dpi));
    }
    // 1. Open popups (menus, dropdowns) sit above everything and win, topmost
    //    first. They render at output scale `dpi`, same mapping as apps.
    if let Some(hit) = popup_under(state, x, y) {
        return Some(hit);
    }
    // 2. Top/Overlay layer surfaces (a status panel, the OSK) win. They render
    //    at fractional scale `dpi`, so their logical coord space is physical/dpi
    //    — map input by /dpi. The rect origin is physical, so surface-local =
    //    (input-origin)/dpi.
    //
    //    While the app is rotated they are not drawn (see `render::draw_scene`),
    //    so they must not be hit-tested either: an invisible panel swallowing
    //    taps over landscape video is the worst of both.
    if !state.rotation.swaps_axes() {
        if let Some((surface, (ox, oy))) = state.layers.hit_test(x, y, state.dpi) {
            return Some(Target::at(surface, (ox as f64, oy as f64), state.dpi));
        }
    }
    // 3. The focused fullscreen app, except the bottom bar zone (home gesture).
    //    App surfaces render at `dpi`, so input maps into logical space by /dpi.
    //    The app is drawn at the usable-area origin (below a top bar / right of
    //    a left bar), so its input origin must match, not (0, 0).
    if let crate::ui_state::UiState::App { toplevel, .. } = &state.ui {
        let (w, h) = state.output_size_f();
        let bar = sc_layout::bar_rect(w, h);
        // The bar zone stays where the user sees it (portrait, at the bottom)
        // even while the app is rotated: it is springchick's own affordance, not
        // the app's, and it is drawn unrotated.
        if !bar.contains(x, y) {
            if let Some(Some(tl)) = state.toplevels.get(*toplevel) {
                // A rotated app fills the output and lives in its own rotated
                // space, so input maps through the rotation instead of through
                // the usable-area origin.
                if state.rotation.swaps_axes() {
                    return Some(Target {
                        surface: tl.surface.wl_surface().clone(),
                        origin: (0.0, 0.0),
                        scale: state.dpi,
                        rotated: true,
                    });
                }
                let u = state.layers.usable(state.dpi);
                let origin = (u.x as f64, u.y as f64);
                return Some(Target::at(
                    tl.surface.wl_surface().clone(),
                    origin,
                    state.dpi,
                ));
            }
        }
    }
    None
}

/// Stable numeric id for a touch slot, for touch-visualization keying. Kept in
/// the positive `i32` range so it never aliases [`touch_viz::POINTER_ID`].
fn slot_id(slot: TouchSlot) -> u64 {
    i32::from(slot) as u64
}

/// Convert a physical output-pixel point into a surface's local logical space.
/// Where an input event is routed: a client surface, its origin in physical
/// global space, the scale mapping physical → its logical space, and whether it
/// is the rotated fullscreen app (whose space is turned relative to the screen).
struct Target {
    surface: WlSurface,
    origin: (f64, f64),
    scale: f64,
    rotated: bool,
}

impl Target {
    /// An unrotated surface at `origin` — every target except a rotated app.
    fn at(surface: WlSurface, origin: (f64, f64), scale: f64) -> Self {
        Target {
            surface,
            origin,
            scale,
            rotated: false,
        }
    }

    /// The focus point smithay subtracts from the event location to get
    /// surface-local coordinates.
    fn focus(&self) -> Point<f64, smithay::utils::Logical> {
        Point::from((self.origin.0 / self.scale, self.origin.1 / self.scale))
    }
}

/// Physical screen coords → a surface's logical space, turning them through the
/// app rotation first when the target is the rotated app, so a tap reaches what
/// the user sees under their finger.
fn to_local(
    state: &State,
    scale: f64,
    rotated: bool,
    x: f32,
    y: f32,
) -> Point<f64, smithay::utils::Logical> {
    let (x, y) = if rotated {
        state.rotation.map_input(x, y, state.output_size)
    } else {
        (x, y)
    };
    Point::from((x as f64 / scale, y as f64 / scale))
}

/// Whether physical point `(x, y)` falls inside a popup's physical rect.
fn rect_contains(origin: (i32, i32), size: (i32, i32), x: f32, y: f32) -> bool {
    let (ox, oy) = (origin.0 as f32, origin.1 as f32);
    x >= ox && y >= oy && x < ox + size.0 as f32 && y < oy + size.1 as f32
}

/// Topmost open popup under `(x, y)`, with its physical origin and coord scale.
fn popup_under(state: &State, x: f32, y: f32) -> Option<Target> {
    let popups = state.active_popups();
    let i = popups
        .iter()
        .rposition(|(_, origin, size)| rect_contains(*origin, *size, x, y))?;
    let (kind, origin, _) = &popups[i];
    Some(Target::at(
        kind.wl_surface().clone(),
        (origin.0 as f64, origin.1 as f64),
        state.dpi,
    ))
}

/// What a press should do with respect to open popups.
enum PopupPress {
    /// Nothing to do here — no popup was hit and no *grabbing* popup is open, so
    /// the tap falls through to normal surface routing. A non-grab popup that
    /// wasn't hit (e.g. a Firefox menu, tapped outside) leaves itself open and
    /// lets the underlying app receive the tap; the client dismisses on its own.
    None,
    /// The tap landed outside a *grabbing* (modal) popup chain: the chain was
    /// dismissed and the tap must be swallowed (popup grab semantics).
    Consumed,
    /// The tap hit a popup; route input into it. Grabbing submenus above the hit
    /// popup were dismissed first.
    Route(Target),
}

/// Resolve a press against open popups.
///
/// Hit-testing considers every open popup so a tap always reaches the menu item
/// under it. Dismissal, though, is modal-only: only popups that issued an
/// `xdg_popup.grab()` swallow an outside tap and get `popup_done`. Non-grab
/// popups (wvkbd's input-hack popup, Firefox's non-grab menus, tooltips) never
/// steal or dismiss a tap they weren't hit by — that outside tap flows through
/// to the app, matching what the client expects.
fn popup_press(state: &mut State, x: f32, y: f32) -> PopupPress {
    // Nothing of the session is on screen while locked, popups included, so
    // there is nothing here to hit or dismiss; the press goes on to the lock
    // surface (or nowhere) via `surface_under`.
    if state.session_lock.is_locked() {
        return PopupPress::None;
    }
    let popups = state.active_popups();
    if popups.is_empty() {
        return PopupPress::None;
    }
    // Snapshot grab status per popup before we mutate `state`.
    let grabs: Vec<bool> = popups
        .iter()
        .map(|(kind, _, _)| state.popup_has_grab(kind.wl_surface()))
        .collect();
    let hit = popups
        .iter()
        .rposition(|(_, origin, size)| rect_contains(*origin, *size, x, y));
    // Which popups to close: the set `popups_to_dismiss` would close for this
    // hit (whole chain on a miss, descendants of the hit popup otherwise),
    // restricted to grabbing popups — non-grab popups are never force-closed.
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
            PopupPress::Route(Target::at(
                kind.wl_surface().clone(),
                (origin.0 as f64, origin.1 as f64),
                state.dpi,
            ))
        }
        // Missed every popup. Only a modal (grabbing) popup consumes the tap;
        // if nothing grabbing was open, `dismiss` is empty and we fall through.
        None if dismiss.is_empty() => PopupPress::None,
        None => PopupPress::Consumed,
    }
}

/// Pointer moved to `(x, y)` (winit/desktop). Forwards to a client surface under
/// the cursor while a press is held on it, else drives gestures.
pub fn pointer_motion(state: &mut State, x: f32, y: f32, time: u32) {
    state.last_pointer_pos = Some((x, y));
    // Track the pointer as a touch contact only while a button is held (a
    // gesture press or a client grab), so a bare hover leaves no mark.
    if state.show_touches && (state.pointer_grab || state.pointer_down) {
        state
            .touch_viz
            .contact(touch_viz::POINTER_ID, x, y, std::time::Instant::now());
        state.needs_render = true;
    }
    if state.pointer_grab {
        if let Some(target) = surface_under(state, x, y) {
            let ptr = state.seat.get_pointer().unwrap();
            let event = PointerMotionEvent {
                location: to_local(state, target.scale, target.rotated, x, y),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            };
            let focus = target.focus();
            ptr.motion(state, Some((target.surface, focus)), &event);
            ptr.frame(state);
            return;
        }
    }
    if state.session_lock.is_locked() {
        return;
    }
    input_common::on_motion(state, x, y);
}

/// Pointer button changed (winit/desktop).
pub fn pointer_button(state: &mut State, pressed: bool, button: u32, time: u32) {
    let (x, y) = state.last_pointer_pos.unwrap_or((0.0, 0.0));
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
            // Enter/position the pointer, then press.
            let focus = target.focus();
            let location = to_local(state, target.scale, target.rotated, x, y);
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
        // Locked with no lock surface: the press is dropped, never funnelled
        // into the shell's gestures.
        if state.session_lock.is_locked() {
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
        input_common::on_release(state);
    }
}

/// A finger touched down at output-pixel `(x, y)`. Routed per-slot: a slot that
/// lands on a client surface is recorded in `touch_targets` and forwarded there;
/// a slot on empty space drives the gesture funnel — but only the first such
/// slot (`gesture_slot`), since the funnel is single-touch.
pub fn down(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
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
        // Record how to map this slot's later motion: the coord scale and
        // whether it goes to the rotated app. Presence marks the slot as
        // client-routed; smithay's TouchHandle tracks the focused surface.
        state
            .touch_targets
            .insert(slot, (target.scale, target.rotated));
        let touch = state.touch.clone();
        let event = DownEvent {
            slot,
            location: to_local(state, target.scale, target.rotated, x, y),
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        let focus = target.focus();
        touch.down(state, Some((target.surface, focus)), &event);
        return;
    }
    // Not on a client surface — drive the gesture system, but only from the one
    // slot that owns it. Extra fingers on empty space are ignored until it lifts.
    // A locked session has no gestures at all: the shell is not on screen, so a
    // swipe on the blank area must not open Home behind the lock.
    if state.gesture_slot.is_none() && !state.session_lock.is_locked() {
        state.gesture_slot = Some(slot);
        input_common::on_motion(state, x, y);
        input_common::on_press(state);
    }
}

/// A finger moved to `(x, y)`.
pub fn motion(state: &mut State, x: f32, y: f32, slot: TouchSlot, time: u32) {
    if state.show_touches {
        state
            .touch_viz
            .contact(slot_id(slot), x, y, std::time::Instant::now());
        state.needs_render = true;
    }
    if let Some(&(scale, rotated)) = state.touch_targets.get(&slot) {
        let touch = state.touch.clone();
        let location = to_local(state, scale, rotated, x, y);
        let event = MotionEvent {
            slot,
            location,
            time,
        };
        // Focus is only used for DnD during motion; we pass none.
        touch.motion(state, None, &event);
        return;
    }
    if state.gesture_slot == Some(slot) {
        input_common::on_motion(state, x, y);
    }
}

/// End of a set of touch changes that happened at the same instant (libinput
/// `TOUCH_FRAME`). Flushes them to the client as one `wl_touch.frame`.
///
/// Driving this from the real frame event rather than firing one after every
/// single down/motion/up is what makes a multi-finger update atomic: two fingers
/// that moved in the same hardware report reach the client in one frame, which
/// is what toolkit gesture recognizers expect when they decide pinch-vs-scroll.
///
/// Safe to call with nothing pending — smithay skips slots whose events have
/// already been framed, so no empty frame reaches the client.
pub fn frame(state: &mut State) {
    let touch = state.touch.clone();
    touch.frame(state);
}

/// The touch stream was cancelled by the input stack (libinput `TOUCH_CANCEL` —
/// palm/thumb rejection, or the device dropping the sequence mid-gesture).
///
/// Without this the cancelled slot leaks: `gesture_slot` stays claimed forever,
/// so the *next* finger on empty space fails the single-touch guard in [`down`]
/// and its whole swipe is silently dropped; and a client-routed slot keeps a
/// phantom contact down, because its `up` never arrives.
///
/// `wl_touch.cancel` is seat-wide by protocol — the client drops *all* its touch
/// points — so this cancels every slot rather than just the reported one.
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
    // Drops the shell-side gesture too: no launch fires, no drag resumes, and
    // `gesture_slot`/`touch_targets` are cleared so the next finger starts fresh.
    state.cancel_gestures();
    state.needs_render = true;
}

/// A finger lifted.
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
