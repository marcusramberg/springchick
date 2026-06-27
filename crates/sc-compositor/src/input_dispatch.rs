//! Structured input dispatch for springchick.
//!
//! Routes pointer/touch events into UiState transitions based on current state.

use crate::ui_state::{UiEvent, UiState};
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
    /// Launch/raise an app by id (with icon center for zoom).
    LaunchApp { app_id: String, icon_center: (f32, f32) },
    /// Start tracking a page drag from this x position.
    StartPageDrag { start_x: f32 },
    /// Start tracking a bar drag (for app switching from Home).
    StartBarDrag { start_x: f32, start_y: f32 },
    /// No action.
    None,
}

/// Process a pointer/touch down event.
pub fn on_press(state: &UiState, x: f32, y: f32, model: &ShellModel, output_size: (i32, i32)) -> DownAction {
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
                    DownAction::LaunchApp {
                        app_id,
                        icon_center: (cx, cy),
                    }
                }
                Hit::DockIcon { app_id, index } => {
                    let slot = &layout.dock[index];
                    let cx = slot.icon_rect.center_x();
                    let cy = slot.icon_rect.center_y();
                    DownAction::LaunchApp {
                        app_id,
                        icon_center: (cx, cy),
                    }
                }
                Hit::Bar => DownAction::StartBarDrag { start_x: x, start_y: y },
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
        UiState::AppOpening { .. } => {
            DownAction::Event(UiEvent::Interrupt { point: pt })
        }
        UiState::Grabbing { .. } => {
            // Already grabbing — no action on additional press.
            DownAction::None
        }
    }
}

/// Process pointer/touch move during a grab.
pub fn on_move(state: &UiState, x: f32, y: f32, dt: f32, output_size: (i32, i32)) -> Option<UiEvent> {
    let (w, h) = (output_size.0 as f32, output_size.1 as f32);
    let pt = normalize(x, y, w, h);
    match state {
        UiState::Grabbing { .. } => Some(UiEvent::GrabMove { point: pt, dt }),
        _ => None,
    }
}
