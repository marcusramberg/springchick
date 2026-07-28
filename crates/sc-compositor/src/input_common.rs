//! Backend-agnostic input handling.
//!
//! winit and libinput both decode their native events into these calls, so the
//! gesture behavior is identical across backends. Keys take a different route:
//! `keybinds` handles them for both backends via the seat keyboard on `State`.

use crate::input_dispatch::{self, DownAction};
use crate::switcher;
use crate::ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use crate::{DragItem, IconPress, State};
use tracing::info;

/// Upward travel (fraction of screen height) that drives close_progress from 0
/// to 1. A release past `CLOSE_COMMIT_PROGRESS` closes the card.
const CLOSE_FULL_RISE: f32 = 0.25;
const CLOSE_COMMIT_PROGRESS: f32 = 0.4;
/// Finger travel to advance the carousel one card, as a fraction of output
/// width (≈ front card width * slide distance).
const FRONT_SCALE_PX_FRAC: f32 = 0.42;
/// Pixels a finger may travel from an icon press and still count as a tap
/// (launch). Past this the press is cancelled and the gesture becomes a page
/// swipe.
const ICON_TAP_SLOP: f32 = 12.0;

/// A finger held on an app icon, waiting to see if it becomes a tap (launch)
/// or a page swipe. Cleared once movement exceeds `ICON_TAP_SLOP`.
#[derive(Clone, Debug)]
pub struct PendingLaunch {
    pub app_id: String,
    pub origin: ZoomOrigin,
    pub start_x: f32,
    pub start_y: f32,
}

/// Switcher drag state.
#[derive(Clone, Copy, Debug)]
pub enum SwitcherDrag {
    /// Finger on a card. Horizontal drag scrolls the deck; a dominant upward
    /// drag closes `toplevel` (tracked live via close_progress).
    OnCard {
        start_x: f32,
        start_y: f32,
        start_scroll: f32,
        toplevel: ToplevelId,
    },
    /// Finger on empty area, waiting to decide.
    InEmpty { start_x: f32, start_y: f32 },
    /// Disengaged.
    None,
}

/// Absolute pointer/touch position update (output pixels).
pub fn on_motion(state: &mut State, x: f32, y: f32) {
    state.last_pointer_pos = Some((x, y));

    if state.pointer_down {
        // Arrange-mode drag: track the finger directly, no launch/swipe logic.
        if state.arrange.as_ref().and_then(|a| a.drag.as_ref()).is_some() {
            let (w, h) = state.output_size_f();
            let page = if let UiState::Home { page, .. } = &state.ui { *page } else { 0 };
            let layout = sc_layout::compute(w, h, page, &state.model);
            let over_dock = layout.dock_zone.contains(x, y);
            let hover = if over_dock {
                None
            } else {
                let app = state.arrange.as_ref().unwrap().drag.as_ref().unwrap().app_id.clone();
                // Fill count on this page with the dragged app removed, so the
                // nearest index maps against the hole-removed order.
                let live_len = state.model.pages.get(page)
                    .map_or(0, |p| p.iter().filter(|a| **a != app).count());
                let idx = sc_layout::nearest_grid_index(w, h, x, y).min(live_len);
                Some((page, idx))
            };
            if let Some(drag) = state.arrange.as_mut().unwrap().drag.as_mut() {
                drag.cur = (x, y);
                drag.hover = hover;
            }
            return;
        }

        // Card drag: dominant-up closes that card, otherwise horizontal scroll.
        if let SwitcherDrag::OnCard {
            start_x,
            start_y,
            start_scroll,
            toplevel,
        } = state.switcher_drag
        {
            let dx = x - start_x;
            let dy = y - start_y;
            let h = state.output_size.1 as f32;
            if dy < 0.0 && dy.abs() > dx.abs() {
                // Swiping the card up to close: track finger via close_progress.
                let progress = ((-dy) / (h * CLOSE_FULL_RISE)).clamp(0.0, 1.0);
                if let UiState::Switcher { close, .. } = &mut state.ui {
                    *close = Some((toplevel, progress, false));
                    return;
                }
            } else if let UiState::Switcher { scroll, close, .. } = &mut state.ui {
                // Horizontal: carousel-pan the deck. Dragging right advances the
                // focus (active card slides off right, next comes forward). One
                // card advances per ~front-card width of travel.
                *close = None;
                let per_index = state.output_size.0 as f32 * FRONT_SCALE_PX_FRAC;
                scroll.value = start_scroll + dx / per_index;
                scroll.target = scroll.value;
                scroll.velocity = 0.0;
                return;
            }
        }

        // Icon press: cancel the pending launch (and its highlight) once the
        // finger travels past the tap slop — the gesture is a page swipe.
        if let Some(p) = &state.pending_launch {
            let dx = x - p.start_x;
            let dy = y - p.start_y;
            if (dx * dx + dy * dy).sqrt() > ICON_TAP_SLOP {
                state.pending_launch = None;
                // The gesture became a swipe — cancel the long-press hold too.
                state.icon_press = None;
            }
        }

        // Page drag: update spring value to follow finger.
        if let Some(start_x) = state.page_drag_start {
            let dx = x - start_x;
            let w = state.output_size.0 as f32;
            if let UiState::Home {
                page,
                page_spring,
                page_count,
                ..
            } = &mut state.ui
            {
                // Directly set spring value to track finger (no spring physics during drag).
                let raw_target = *page as f32 - dx / w;
                // Rubber-band past edges.
                let max_page = (*page_count).saturating_sub(1) as f32;
                page_spring.value = if raw_target < 0.0 {
                    raw_target * 0.3 // rubber-band left
                } else if raw_target > max_page {
                    max_page + (raw_target - max_page) * 0.3 // rubber-band right
                } else {
                    raw_target
                };
                page_spring.target = page_spring.value;
                page_spring.velocity = 0.0;
            }
        }

        // Feed movement to grab if active.
        let dt = 1.0 / 90.0;
        if let Some(ev) = input_dispatch::on_move(&state.ui, x, y, dt, state.output_size) {
            transition(&mut state.ui, ev);
        }
    }
}

/// Touch-down / button-press at the last known position.
pub fn on_press(state: &mut State) {
    let Some((x, y)) = state.last_pointer_pos else {
        return;
    };
    state.pointer_down = true;

    // Arrange-mode input: badges, Done button, and picking up a new drag all
    // take priority over the normal Home hit-testing below.
    if state.arrange.is_some() {
        let (w, h) = state.output_size_f();
        let page = if let UiState::Home { page, .. } = &state.ui {
            *page
        } else {
            0
        };
        let layout = sc_layout::compute(w, h, page, &state.model);
        match sc_layout::hit_test_arrange(&layout, x, y) {
            sc_layout::Hit::RemoveBadge { app_id } => {
                state.model.hide(&app_id);
                state.after_arrange_edit();
            }
            sc_layout::Hit::DoneButton | sc_layout::Hit::Miss | sc_layout::Hit::Bar => {
                state.arrange = None;
            }
            sc_layout::Hit::GridIcon { app_id, .. } => {
                if let Some(a) = &mut state.arrange {
                    a.drag = Some(DragItem {
                        app_id,
                        source: input_dispatch::IconSource::Grid,
                        cur: (x, y),
                        hover: None,
                    });
                }
            }
            sc_layout::Hit::DockIcon { app_id, .. } => {
                if let Some(a) = &mut state.arrange {
                    a.drag = Some(DragItem {
                        app_id,
                        source: input_dispatch::IconSource::Dock,
                        cur: (x, y),
                        hover: None,
                    });
                }
            }
        }
        return;
    }

    // Switcher deck input.
    if matches!(state.ui, UiState::Switcher { .. }) {
        let hit = switcher::hit_test(
            &state.switcher_cards,
            x,
            y,
            state.output_size_f(),
        );
        match hit {
            switcher::CardHit::Card(idx) => {
                let toplevel = state.switcher_cards.get(idx).map(|c| c.toplevel);
                if let (UiState::Switcher { scroll, .. }, Some(toplevel)) = (&state.ui, toplevel) {
                    state.switcher_drag = SwitcherDrag::OnCard {
                        start_x: x,
                        start_y: y,
                        start_scroll: scroll.value,
                        toplevel,
                    };
                }
            }
            switcher::CardHit::Empty => {
                state.switcher_drag = SwitcherDrag::InEmpty {
                    start_x: x,
                    start_y: y,
                };
            }
        }
        return;
    }

    let action = input_dispatch::on_press(&state.ui, x, y, &state.model, state.output_size);
    match action {
        DownAction::Event(ev) => {
            transition(&mut state.ui, ev);
        }
        DownAction::PressIcon {
            app_id,
            origin,
            start_x,
            start_y,
            source,
        } => {
            // Arm a launch, but also start a page drag from the same point so a
            // swipe that begins on an icon still flips pages. Whichever the
            // release resolves to (tap vs swipe) wins.
            state.pending_launch = Some(PendingLaunch {
                app_id: app_id.clone(),
                origin,
                start_x,
                start_y,
            });
            state.page_drag_start = Some(start_x);
            // Also arm the long-press hold that, if the finger stays put long
            // enough, engages arrange mode (see `advance_frame`).
            state.icon_press = Some(IconPress {
                app_id,
                source,
                start: (start_x, start_y),
                at: std::time::Instant::now(),
            });
        }
        DownAction::StartPageDrag { start_x } => {
            state.page_drag_start = Some(start_x);
        }
        DownAction::StartBarDrag { start_x, start_y } => {
            state.bar_drag_start = Some((start_x, start_y));
        }
        DownAction::None => {}
    }
}

/// Touch-up / button-release at the last known position.
pub fn on_release(state: &mut State) {
    let Some((x, y)) = state.last_pointer_pos else {
        return;
    };
    state.pointer_down = false;

    // Arrange-mode release: resolve the drag (if any) to pin/unpin/snap-back,
    // then stay in arrange mode (only Done/empty-tap exits it).
    if let Some(arrange) = &mut state.arrange {
        if let Some(drag) = arrange.drag.take() {
            let (w, h) = state.output_size_f();
            let page = if let UiState::Home { page, .. } = &state.ui {
                *page
            } else {
                0
            };
            let page_len = state.model.pages.get(page).map_or(0, |p| p.len());
            let layout = sc_layout::compute(w, h, page, &state.model);
            match input_dispatch::resolve_drop(
                drag.cur.0,
                drag.cur.1,
                &layout,
                drag.source,
                page,
                page_len,
                w,
                h,
            ) {
                input_dispatch::DropAction::Pin => {
                    if state.model.pin(&drag.app_id) {
                        state.after_arrange_edit();
                    }
                }
                input_dispatch::DropAction::Unpin => {
                    state.model.unpin(&drag.app_id);
                    state.after_arrange_edit();
                }
                input_dispatch::DropAction::Reorder { page, index } => {
                    let (pg, ix) = drag.hover.unwrap_or((page, index));
                    state.model.move_to(&drag.app_id, pg, ix);
                    state.after_arrange_edit();
                }
                input_dispatch::DropAction::SnapBack => {}
            }
        }
        return;
    }

    // Icon tap: the pending launch survived (finger never passed the tap slop),
    // so this was a tap, not a swipe. Launch and drop the page drag.
    if let Some(p) = state.pending_launch.take() {
        state.page_drag_start = None;
        state.icon_press = None;
        state.launch_or_raise(&p.app_id, p.origin);
        return;
    }
    state.icon_press = None;

    // Bar drag from Home: classify swipe direction.
    if let Some((start_x, start_y)) = state.bar_drag_start.take() {
        let dx = x - start_x;
        let dy = start_y - y; // positive = swiped up
        let (w, h) = state.output_size_f();

        if dy > h * 0.08 {
            // Swiped up from bar → raise most recent app.
            if let Some(tid) = state.history.previous() {
                state.raise_toplevel_centered(tid);
            }
        } else if dx.abs() > w * 0.15 {
            // Horizontal swipe on bar → quick-switch.
            let dir = if dx < 0.0 { 1 } else { -1 };
            if let Some(tid) = state.history.quick_switch(dir) {
                state.raise_toplevel_centered(tid);
            }
        }
    }

    // Page swipe: snap based on 30% threshold.
    if let Some(start_x) = state.page_drag_start.take() {
        let dx = x - start_x;
        let w = state.output_size.0 as f32;
        let page_delta = -dx / w; // positive = swiping to next page
        if let UiState::Home {
            page,
            page_spring,
            page_count,
            ..
        } = &mut state.ui
        {
            let target_page = if page_delta > 0.3 && *page + 1 < *page_count {
                *page + 1
            } else if page_delta < -0.3 && *page > 0 {
                *page - 1
            } else {
                *page
            };
            *page = target_page;
            page_spring.retarget(target_page as f32);
        }
    }

    // Switcher release: tap card or dismiss.
    if matches!(state.ui, UiState::Switcher { .. }) {
        let drag = std::mem::replace(&mut state.switcher_drag, SwitcherDrag::None);
        match drag {
            SwitcherDrag::OnCard {
                start_x, start_y, ..
            } => {
                // If this drag was closing a card, commit past the threshold or
                // snap it back.
                let closing = if let UiState::Switcher { close, .. } = &state.ui {
                    *close
                } else {
                    None
                };
                if let Some((ct, progress, _)) = closing {
                    if progress >= CLOSE_COMMIT_PROGRESS {
                        // Remove the card from the deck, then close the client.
                        let eff =
                            transition(&mut state.ui, UiEvent::SwitcherCloseCard { toplevel: ct });
                        if let crate::ui_state::Effect::CloseToplevel { toplevel } = eff {
                            state.detach_toplevel(toplevel);
                        }
                    } else if let UiState::Switcher { close, .. } = &mut state.ui {
                        // Cancelled — mark releasing so Tick springs it back.
                        *close = Some((ct, progress, true));
                    }
                    return;
                }

                let dx = (x - start_x).abs();
                let dy = (y - start_y).abs();
                if dx < 15.0 && dy < 15.0 {
                    // Tap: open the card.
                    let hit = switcher::hit_test(
                        &state.switcher_cards,
                        x,
                        y,
                        state.output_size_f(),
                    );
                    if let switcher::CardHit::Card(idx) = hit {
                        // `idx` indexes the z-sorted render array; resolve it to
                        // the card's toplevel id so ordering can't desync.
                        if let Some(card) = state.switcher_cards.get(idx).copied() {
                            let origin =
                                ZoomOrigin::card((card.center_x, card.center_y), card.scale);
                            // Selecting a card makes it the most-recent app, so
                            // the next switcher shows it as the front card.
                            state.history.push_foreground(card.toplevel);
                            transition(
                                &mut state.ui,
                                UiEvent::SwitcherTapCard {
                                    toplevel: card.toplevel,
                                    origin,
                                },
                            );
                            return;
                        }
                    }
                } else if let UiState::Switcher { cards, scroll, .. } = &mut state.ui {
                    // Carousel scroll released: settle to the nearest card.
                    let max = cards.len().saturating_sub(1) as f32;
                    let target = scroll.value.round().clamp(0.0, max);
                    scroll.retarget(target);
                }
            }
            SwitcherDrag::InEmpty { start_x, start_y } => {
                let dx = (x - start_x).abs();
                let dy = (y - start_y).abs();
                if dx < 15.0 && dy < 15.0 {
                    transition(&mut state.ui, UiEvent::SwitcherDismiss);
                    return;
                }
            }
            SwitcherDrag::None => {}
        }
    }

    // Release grab if active.
    let release = if let UiState::Grabbing {
        tracker,
        toplevel,
        app_id,
    } = &state.ui
    {
        Some((
            sc_input::classify_release(tracker),
            *toplevel,
            app_id.clone(),
        ))
    } else {
        None
    };
    if let Some((target, cur_tid, cur_app)) = release {
        info!(target: "springchick::debug", "on_release grab target={:?}", target);
        match target {
            sc_input::NavTarget::QuickSwitch(dir) => {
                // Grab-based quick-switch: raise the adjacent app directly.
                let adj = state
                    .history
                    .quick_switch(dir)
                    .filter(|tid| matches!(state.toplevels.get(*tid), Some(Some(_))));
                match adj {
                    Some(tid) => {
                        let app_id = state.toplevels[tid].as_ref().unwrap().app_id.clone();
                        state.history.push_foreground(tid);
                        transition(
                            &mut state.ui,
                            UiEvent::RaiseApp {
                                toplevel: tid,
                                app_id,
                            },
                        );
                    }
                    // No adjacent app — snap back to the current one.
                    None => {
                        transition(
                            &mut state.ui,
                            UiEvent::RaiseApp {
                                toplevel: cur_tid,
                                app_id: cur_app,
                            },
                        );
                    }
                }
            }
            _ => {
                let last_origin = state.last_origin;
                let size = state.output_size_f();
                transition(&mut state.ui, UiEvent::GrabRelease);
                // Settling toward Home zooms back to the launcher icon; toward
                // the switcher it settles into the front-card slot so the shrink
                // flows straight into the fan (no shrink-to-point then pop).
                if let UiState::Settling { origin, target, .. } = &mut state.ui {
                    *origin = if matches!(target, sc_input::NavTarget::Switcher) {
                        let (cx, cy, s) = switcher::front_slot(size);
                        ZoomOrigin::card((cx, cy), s)
                    } else {
                        last_origin
                    };
                }
            }
        }
    }

    // Update page_count after returning home.
    if let UiState::Home { page_count, .. } = &mut state.ui {
        *page_count = state.model.pages.len().max(1);
    }
}
