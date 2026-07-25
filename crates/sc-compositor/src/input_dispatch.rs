//! Structured input dispatch for springchick.
//!
//! Routes pointer/touch events into UiState transitions based on current state.

use crate::ui_state::{UiEvent, UiState, ZoomOrigin};
use sc_input::Pt;
use sc_layout::{self, Hit};
use sc_shell_model::ShellModel;

/// Normalized point from pixel coordinates.
fn normalize(x: f32, y: f32, width: f32, height: f32) -> Pt {
    Pt {
        x: x / width,
        y: y / height,
    }
}

/// Result of processing a pointer/touch down event.
#[derive(Clone, Debug)]
pub enum DownAction {
    /// Emit this UiEvent.
    Event(UiEvent),
    /// Finger went down on an app icon. Track it as a pending launch: a release
    /// with little movement launches; movement past the tap threshold cancels it
    /// and the gesture becomes a page swipe instead.
    PressIcon {
        app_id: String,
        origin: ZoomOrigin,
        start_x: f32,
        start_y: f32,
    },
    /// Start tracking a page drag from this x position.
    StartPageDrag { start_x: f32 },
    /// Start tracking a bar drag (for app switching from Home).
    StartBarDrag { start_x: f32, start_y: f32 },
    /// No action.
    None,
}

/// Process a pointer/touch down event.
pub fn on_press(
    state: &UiState,
    x: f32,
    y: f32,
    model: &ShellModel,
    output_size: (i32, i32),
) -> DownAction {
    let (w, h) = (output_size.0 as f32, output_size.1 as f32);
    let pt = normalize(x, y, w, h);

    match state {
        UiState::Home { page, .. } => {
            let layout = sc_layout::compute(w, h, *page, model);
            match sc_layout::hit_test(&layout, x, y) {
                Hit::GridIcon { app_id, index } => {
                    let slot = &layout.grid[index];
                    let cx = slot.icon_rect.center_x();
                    let cy = slot.icon_rect.center_y();
                    DownAction::PressIcon {
                        app_id,
                        origin: ZoomOrigin::icon((cx, cy)),
                        start_x: x,
                        start_y: y,
                    }
                }
                Hit::DockIcon { app_id, index } => {
                    let slot = &layout.dock[index];
                    let cx = slot.icon_rect.center_x();
                    let cy = slot.icon_rect.center_y();
                    DownAction::PressIcon {
                        app_id,
                        origin: ZoomOrigin::icon((cx, cy)),
                        start_x: x,
                        start_y: y,
                    }
                }
                Hit::Bar => DownAction::StartBarDrag {
                    start_x: x,
                    start_y: y,
                },
                Hit::Miss => DownAction::StartPageDrag { start_x: x },
            }
        }
        UiState::App { .. } => {
            // Check if finger is in bar zone → start grab.
            let layout = sc_layout::compute(w, h, 0, model);
            if layout.bar_rect.contains(x, y) {
                DownAction::Event(UiEvent::GrabStart { point: pt })
            } else {
                DownAction::None
            }
        }
        UiState::Settling { .. } | UiState::AppClosing { .. } => {
            // Interrupt the animation.
            DownAction::Event(UiEvent::Interrupt { point: pt })
        }
        UiState::AppOpening { .. } => DownAction::Event(UiEvent::Interrupt { point: pt }),
        UiState::Grabbing { .. } => {
            // Already grabbing — no action on additional press.
            DownAction::None
        }
        UiState::Switcher { .. } => {
            // Switcher handles its own input in input_common.
            DownAction::None
        }
    }
}

/// Process pointer/touch move during a grab.
pub fn on_move(
    state: &UiState,
    x: f32,
    y: f32,
    dt: f32,
    output_size: (i32, i32),
) -> Option<UiEvent> {
    let (w, h) = (output_size.0 as f32, output_size.1 as f32);
    let pt = normalize(x, y, w, h);
    match state {
        UiState::Grabbing { .. } => Some(UiEvent::GrabMove { point: pt, dt }),
        UiState::Switcher { .. } => {
            // Switcher handles its own move in input_common.
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_shell_model::ShellModel;

    fn model() -> ShellModel {
        let mut m = ShellModel::default();
        for i in 0..6 {
            m.place(format!("app{i}"));
        }
        m
    }

    /// Pressing an icon must NOT launch immediately — it arms a pending tap so a
    /// swipe that starts on the icon can still flip pages. Launch happens on
    /// release (in input_common), not here.
    #[test]
    fn press_on_icon_arms_pending_not_launch() {
        let m = model();
        let out = (1224, 2700);
        let layout = sc_layout::compute(out.0 as f32, out.1 as f32, 0, &m);
        let slot = &layout.grid[0];
        let (cx, cy) = (slot.icon_rect.center_x(), slot.icon_rect.center_y());

        let action = on_press(&UiState::home(0, 1), cx, cy, &m, out);
        match action {
            DownAction::PressIcon { app_id, .. } => assert_eq!(app_id, "app0"),
            other => panic!("expected PressIcon, got {other:?}"),
        }
    }

    /// Pressing empty grid space starts a page drag straight away.
    #[test]
    fn press_on_empty_starts_page_drag() {
        let m = model();
        let out = (1224, 2700);
        // Top-left corner of the status-bar padding — no icon there.
        let action = on_press(&UiState::home(0, 1), 5.0, 5.0, &m, out);
        assert!(matches!(action, DownAction::StartPageDrag { .. }));
    }
}
