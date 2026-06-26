//! Pure UI state machine for springchick.
//!
//! `UiState` + `UiEvent` → `UiState` transitions, unit-tested without Wayland/GPU.

use sc_anim::Spring;

/// Opaque toplevel identifier (index into the compositor's toplevel vec).
pub type ToplevelId = usize;

/// The two modes the shell can be in.
#[derive(Clone, Debug)]
pub enum UiState {
    Home {
        page: usize,
        /// Active page-swipe spring (value = fractional page offset for smooth scrolling).
        page_spring: Spring,
        /// Total pages (cached from model).
        page_count: usize,
    },
    App {
        toplevel: ToplevelId,
        app_id: String,
    },
}

impl UiState {
    pub fn home(page: usize, page_count: usize) -> Self {
        let mut spring = Spring::new(page as f32);
        spring.retarget(page as f32);
        UiState::Home {
            page,
            page_spring: spring,
            page_count,
        }
    }
}

/// Events the UI state machine accepts.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// Icon tapped — launch or raise.
    TapIcon { app_id: String },
    /// App launched and matched to a toplevel.
    AppMapped { toplevel: ToplevelId, app_id: String },
    /// Return-home affordance triggered (bar tap or Esc).
    ReturnHome,
    /// Foreground app's toplevel was destroyed.
    ToplevelClosed { toplevel: ToplevelId },
    /// Horizontal page swipe: delta in pages (fractional, e.g. -0.3 = dragged 30% left).
    PageDrag { delta: f32 },
    /// Page swipe released — snap to nearest page.
    PageRelease,
}

/// Result of a state transition.
#[derive(Clone, Debug)]
pub enum Effect {
    /// Launch an app by app_id.
    Launch { app_id: String },
    /// Raise an existing toplevel to foreground.
    Raise { toplevel: ToplevelId },
    /// No side effect.
    None,
}

/// Advance the state machine. Returns new state + optional side effect.
pub fn transition(state: &mut UiState, event: UiEvent) -> Effect {
    match event {
        UiEvent::TapIcon { app_id } => {
            // Effect only — actual state change happens on AppMapped.
            Effect::Launch { app_id }
        }
        UiEvent::AppMapped { toplevel, app_id } => {
            *state = UiState::App { toplevel, app_id };
            Effect::None
        }
        UiEvent::ReturnHome => {
            if let UiState::App { .. } = state {
                let page = 0; // Return to first page; could preserve last page.
                *state = UiState::home(page, 1);
            }
            Effect::None
        }
        UiEvent::ToplevelClosed { toplevel } => {
            if let UiState::App {
                toplevel: current, ..
            } = state
            {
                if *current == toplevel {
                    *state = UiState::home(0, 1);
                }
            }
            Effect::None
        }
        UiEvent::PageDrag { delta } => {
            if let UiState::Home {
                page,
                page_spring,
                page_count,
            } = state
            {
                let target = (*page as f32 + delta).clamp(0.0, (*page_count).saturating_sub(1) as f32);
                page_spring.retarget(target);
            }
            Effect::None
        }
        UiEvent::PageRelease => {
            if let UiState::Home {
                page,
                page_spring,
                page_count,
            } = state
            {
                // Snap to nearest page.
                let nearest = page_spring.value.round().clamp(0.0, (*page_count).saturating_sub(1) as f32) as usize;
                *page = nearest;
                page_spring.retarget(nearest as f32);
            }
            Effect::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_icon_produces_launch_effect() {
        let mut state = UiState::home(0, 1);
        let effect = transition(
            &mut state,
            UiEvent::TapIcon {
                app_id: "org.foo.Bar".into(),
            },
        );
        assert!(matches!(effect, Effect::Launch { app_id } if app_id == "org.foo.Bar"));
        // State stays Home until AppMapped.
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn app_mapped_transitions_to_app() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 42,
                app_id: "foo".into(),
            },
        );
        assert!(matches!(state, UiState::App { toplevel: 42, .. }));
    }

    #[test]
    fn return_home_from_app() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "x".into(),
        };
        transition(&mut state, UiEvent::ReturnHome);
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn toplevel_closed_returns_home_if_foreground() {
        let mut state = UiState::App {
            toplevel: 5,
            app_id: "x".into(),
        };
        transition(&mut state, UiEvent::ToplevelClosed { toplevel: 5 });
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn toplevel_closed_ignored_if_not_foreground() {
        let mut state = UiState::App {
            toplevel: 5,
            app_id: "x".into(),
        };
        transition(&mut state, UiEvent::ToplevelClosed { toplevel: 99 });
        assert!(matches!(state, UiState::App { toplevel: 5, .. }));
    }

    #[test]
    fn page_release_snaps_to_nearest() {
        let mut state = UiState::home(0, 3);
        // Drag to ~1.7 pages
        if let UiState::Home { page_spring, .. } = &mut state {
            page_spring.value = 1.7;
        }
        transition(&mut state, UiEvent::PageRelease);
        if let UiState::Home { page, page_spring, .. } = &state {
            assert_eq!(*page, 2);
            assert!((page_spring.target - 2.0).abs() < 0.01);
        } else {
            panic!("expected Home");
        }
    }
}
