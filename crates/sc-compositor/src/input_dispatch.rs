//! Routes pointer/touch events into UiState transitions.

use crate::ui_state::{UiEvent, UiState, ZoomOrigin};
use sc_input::Pt;
use sc_layout::{self, Hit};
use sc_shell_model::ShellModel;

fn normalize(x: f32, y: f32, width: f32, height: f32) -> Pt {
    Pt {
        x: x / width,
        y: y / height,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IconSource {
    Grid,
    Dock,
    /// Out of a library folder: a drop places it rather than reordering.
    Library,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropAction {
    Pin,
    Reorder { page: usize, index: usize },
    SnapBack,
}

/// A `Pin` may still fail in the model (full dock); the caller snaps back.
/// `page_len` is the visible page's icon count.
pub fn resolve_drop(
    pos: (f32, f32),
    layout: &sc_layout::Layout,
    source: IconSource,
    page: usize,
    page_len: usize,
    size: (f32, f32),
) -> DropAction {
    let (x, y) = pos;
    let (w, h) = size;
    let over_dock = layout.dock_zone.contains(x, y);
    match (source, over_dock) {
        (IconSource::Grid | IconSource::Library, true) => DropAction::Pin,
        (IconSource::Dock, true) => DropAction::SnapBack, // dock -> dock: no-op
        (_, false) => {
            let idx = sc_layout::nearest_grid_index(w, h, x, y).min(page_len);
            DropAction::Reorder { page, index: idx }
        }
    }
}

#[derive(Clone, Debug)]
pub enum DownAction {
    Event(UiEvent),
    /// Launches on a still release; movement past the tap slop makes it a swipe.
    PressIcon {
        app_id: String,
        origin: ZoomOrigin,
        start_x: f32,
        start_y: f32,
        source: IconSource,
    },
    StartPageDrag {
        start_x: f32,
    },
    StartBarDrag {
        start_x: f32,
        start_y: f32,
    },
    None,
}

fn home_press(x: f32, y: f32, page: usize, model: &ShellModel, w: f32, h: f32) -> DownAction {
    let layout = sc_layout::compute(w, h, page, model);
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
                source: IconSource::Grid,
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
                source: IconSource::Dock,
            }
        }
        Hit::Bar => DownAction::StartBarDrag {
            start_x: x,
            start_y: y,
        },
        Hit::Miss => DownAction::StartPageDrag { start_x: x },
        Hit::RemoveBadge { .. } | Hit::DoneButton => DownAction::None,
    }
}

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
        UiState::Home { page, .. } => home_press(x, y, *page, model, w, h),
        UiState::App { .. } => {
            let layout = sc_layout::compute(w, h, 0, model);
            if layout.bar_rect.contains(x, y) {
                DownAction::Event(UiEvent::GrabStart { point: pt })
            } else {
                DownAction::None
            }
        }
        // Home is live behind the shrinking app: an icon tap launches that icon.
        // Anything else interrupts and re-grabs the outgoing window.
        UiState::AppClosing { .. } => match home_press(x, y, 0, model, w, h) {
            press @ DownAction::PressIcon { .. } => press,
            _ => DownAction::Event(UiEvent::Interrupt { point: pt }),
        },
        UiState::Settling { target, .. } => {
            // Same for a settle heading Home; others keep interrupting.
            if matches!(target, sc_input::NavTarget::Home) {
                if let press @ DownAction::PressIcon { .. } = home_press(x, y, 0, model, w, h) {
                    return press;
                }
            }
            DownAction::Event(UiEvent::Interrupt { point: pt })
        }
        UiState::AppOpening { .. } => DownAction::Event(UiEvent::Interrupt { point: pt }),
        UiState::Grabbing { .. } => DownAction::None,
        UiState::Switcher { .. } => {
            // The switcher handles its own input in input_common.
            DownAction::None
        }
        UiState::QuickSwitch { .. } => DownAction::None,
    }
}

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
        UiState::Switcher { .. } => None,
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

    /// Launch happens on release, so a swipe starting on an icon still pages.
    #[test]
    fn press_on_icon_arms_pending_not_launch() {
        let m = model();
        let out = (1224, 2700);
        let layout = sc_layout::compute(out.0 as f32, out.1 as f32, 0, &m);
        let slot = &layout.grid[0];
        let (cx, cy) = (slot.icon_rect.center_x(), slot.icon_rect.center_y());

        let action = on_press(&UiState::home(0, 1), cx, cy, &m, out);
        match action {
            DownAction::PressIcon {
                app_id, source: _, ..
            } => assert_eq!(app_id, "app0"),
            other => panic!("expected PressIcon, got {other:?}"),
        }
    }

    #[test]
    fn press_on_empty_starts_page_drag() {
        let m = model();
        let out = (1224, 2700);
        let action = on_press(&UiState::home(0, 1), 5.0, 5.0, &m, out);
        assert!(matches!(action, DownAction::StartPageDrag { .. }));
    }

    fn icon_center(m: &ShellModel, out: (i32, i32)) -> (f32, f32) {
        let layout = sc_layout::compute(out.0 as f32, out.1 as f32, 0, m);
        let slot = &layout.grid[0];
        (slot.icon_rect.center_x(), slot.icon_rect.center_y())
    }

    #[test]
    fn press_on_icon_while_closing_launches_it() {
        let m = model();
        let out = (1224, 2700);
        let (cx, cy) = icon_center(&m, out);
        let closing = UiState::AppClosing {
            toplevel: 7,
            app_id: "other".into(),
            progress: sc_anim::Spring::zoom(1.0, 0.0),
            origin: ZoomOrigin::icon((0.5, 0.5)),
        };
        match on_press(&closing, cx, cy, &m, out) {
            DownAction::PressIcon { app_id, .. } => assert_eq!(app_id, "app0"),
            other => panic!("expected PressIcon, got {other:?}"),
        }
    }

    #[test]
    fn press_on_icon_while_settling_home_launches_it() {
        let m = model();
        let out = (1224, 2700);
        let (cx, cy) = icon_center(&m, out);
        let settling = UiState::Settling {
            toplevel: 7,
            app_id: "other".into(),
            target: sc_input::NavTarget::Home,
            progress: sc_anim::Spring::new(0.5),
            origin: ZoomOrigin::icon((0.5, 0.5)),
            cards: Vec::new(),
        };
        match on_press(&settling, cx, cy, &m, out) {
            DownAction::PressIcon { app_id, .. } => assert_eq!(app_id, "app0"),
            other => panic!("expected PressIcon, got {other:?}"),
        }
    }

    #[test]
    fn press_off_icon_while_closing_interrupts() {
        let m = model();
        let out = (1224, 2700);
        let closing = UiState::AppClosing {
            toplevel: 7,
            app_id: "other".into(),
            progress: sc_anim::Spring::zoom(1.0, 0.0),
            origin: ZoomOrigin::icon((0.5, 0.5)),
        };
        assert!(matches!(
            on_press(&closing, 5.0, 5.0, &m, out),
            DownAction::Event(UiEvent::Interrupt { .. })
        ));
    }

    #[test]
    fn press_on_icon_while_settling_back_to_app_interrupts() {
        let m = model();
        let out = (1224, 2700);
        let (cx, cy) = icon_center(&m, out);
        let settling = UiState::Settling {
            toplevel: 7,
            app_id: "other".into(),
            target: sc_input::NavTarget::BackToApp,
            progress: sc_anim::Spring::new(0.5),
            origin: ZoomOrigin::icon((0.5, 0.5)),
            cards: Vec::new(),
        };
        assert!(matches!(
            on_press(&settling, cx, cy, &m, out),
            DownAction::Event(UiEvent::Interrupt { .. })
        ));
    }

    #[test]
    fn resolve_drop_grid_over_dock_is_pin() {
        let (w, h) = (1224.0, 2700.0);
        let mut m = ShellModel::default();
        for i in 0..3 {
            m.place(format!("app{i}"));
        }
        let l = sc_layout::compute(w, h, 0, &m);
        let (x, y) = (l.dock_zone.center_x(), l.dock_zone.center_y());
        assert_eq!(
            resolve_drop((x, y), &l, IconSource::Grid, 0, 0, (w, h)),
            DropAction::Pin
        );
        assert_eq!(
            resolve_drop((x, y), &l, IconSource::Library, 0, 0, (w, h)),
            DropAction::Pin
        );
    }

    #[test]
    fn resolve_drop_dock_over_grid_is_reorder() {
        let (w, h) = (1224.0, 2700.0);
        let l = sc_layout::compute(w, h, 0, &ShellModel::default());
        let p = sc_layout::global_slot_pos(0, 1, w, h);
        assert_eq!(
            resolve_drop(p, &l, IconSource::Dock, 0, 3, (w, h)),
            DropAction::Reorder { page: 0, index: 1 },
        );
    }

    #[test]
    fn resolve_drop_grid_over_grid_is_reorder() {
        let (w, h) = (1224.0, 2700.0);
        let l = sc_layout::compute(w, h, 0, &ShellModel::default());
        let p = sc_layout::global_slot_pos(0, 2, w, h);
        assert_eq!(
            resolve_drop(p, &l, IconSource::Grid, 0, 5, (w, h)),
            DropAction::Reorder { page: 0, index: 2 },
        );
    }

    #[test]
    fn resolve_drop_reorder_clamps_to_page_len() {
        let (w, h) = (1224.0, 2700.0);
        let l = sc_layout::compute(w, h, 0, &ShellModel::default());
        let p = sc_layout::global_slot_pos(0, 10, w, h);
        assert_eq!(
            resolve_drop(p, &l, IconSource::Grid, 0, 3, (w, h)),
            DropAction::Reorder { page: 0, index: 3 },
        );
    }
}
