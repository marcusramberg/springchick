//! Backend-agnostic input handling.
//!
//! winit and libinput both decode their native events into these calls, so the
//! gesture behavior is identical across backends. Keyboard *forwarding* to the
//! focused client stays backend-specific (it needs the seat keyboard handle);
//! only the Esc return-home shortcut is shared here.

use crate::input_dispatch::{self, DownAction};
use crate::ui_state::{transition, UiEvent, UiState};
use crate::State;

/// Esc → return-home shortcut (dev convenience). Returns true if handled.
pub fn on_escape(state: &mut State) -> bool {
    if matches!(
        state.ui,
        UiState::App { .. } | UiState::Grabbing { .. } | UiState::Settling { .. }
    ) {
        state.handle_return_home();
        true
    } else {
        false
    }
}

/// Absolute pointer/touch position update (output pixels).
pub fn on_motion(state: &mut State, x: f32, y: f32) {
    state.last_pointer_pos = Some((x, y));

    if state.pointer_down {
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
    let action = input_dispatch::on_press(&state.ui, x, y, &state.model, state.output_size);
    match action {
        DownAction::Event(ev) => {
            transition(&mut state.ui, ev);
        }
        DownAction::LaunchApp { app_id, icon_center } => {
            state.launch_or_raise(&app_id, icon_center);
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

    // Bar drag from Home: classify swipe direction.
    if let Some((start_x, start_y)) = state.bar_drag_start.take() {
        let dx = x - start_x;
        let dy = start_y - y; // positive = swiped up
        let w = state.output_size.0 as f32;
        let h = state.output_size.1 as f32;

        if dy > h * 0.08 {
            // Swiped up from bar → raise most recent app.
            if let Some(tid) = state.history.previous() {
                if let Some(Some(tl)) = state.toplevels.get(tid) {
                    let app_id = tl.app_id.clone();
                    state.last_icon_center = (w / 2.0, h / 2.0);
                    state.history.push_foreground(tid);
                    transition(
                        &mut state.ui,
                        UiEvent::RaiseApp {
                            toplevel: tid,
                            app_id,
                        },
                    );
                }
            }
        } else if dx.abs() > w * 0.15 {
            // Horizontal swipe on bar → quick-switch.
            let dir = if dx < 0.0 { 1 } else { -1 };
            if let Some(tid) = state.history.quick_switch(dir) {
                if let Some(Some(tl)) = state.toplevels.get(tid) {
                    let app_id = tl.app_id.clone();
                    state.last_icon_center = (w / 2.0, h / 2.0);
                    state.history.push_foreground(tid);
                    transition(
                        &mut state.ui,
                        UiEvent::RaiseApp {
                            toplevel: tid,
                            app_id,
                        },
                    );
                }
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

    // Release grab if active.
    let release = if let UiState::Grabbing {
        tracker,
        toplevel,
        app_id,
    } = &state.ui
    {
        Some((sc_input::classify_release(tracker), *toplevel, app_id.clone()))
    } else {
        None
    };
    if let Some((target, cur_tid, cur_app)) = release {
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
                        transition(&mut state.ui, UiEvent::RaiseApp { toplevel: tid, app_id });
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
                transition(&mut state.ui, UiEvent::GrabRelease);
                // Settling toward Home/Switcher needs the real icon origin.
                if let UiState::Settling { icon_center, .. } = &mut state.ui {
                    *icon_center = state.last_icon_center;
                }
            }
        }
    }

    // Update page_count after returning home.
    if let UiState::Home { page_count, .. } = &mut state.ui {
        *page_count = state.model.pages.len().max(1);
    }
}
