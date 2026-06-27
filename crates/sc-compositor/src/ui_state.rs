//! Pure UI state machine for springchick.
//!
//! `UiState` + `UiEvent` → `UiState` transitions, unit-tested without Wayland/GPU.

use sc_anim::Spring;
use sc_input::{NavTarget, Tracker};

/// Opaque toplevel identifier (index into the compositor's toplevel vec).
pub type ToplevelId = usize;

/// The shell's UI states, including transition animations.
#[derive(Clone, Debug)]
pub enum UiState {
    Home {
        page: usize,
        page_spring: Spring,
        page_count: usize,
    },
    App {
        toplevel: ToplevelId,
        app_id: String,
    },
    /// Icon → fullscreen zoom animation.
    AppOpening {
        toplevel: ToplevelId,
        app_id: String,
        /// Spring 0→1 (0 = icon size, 1 = fullscreen).
        progress: Spring,
        /// Icon center in logical pixels.
        icon_center: (f32, f32),
    },
    /// Fullscreen → icon shrink animation.
    AppClosing {
        toplevel: ToplevelId,
        app_id: String,
        /// Spring 1→0.
        progress: Spring,
        icon_center: (f32, f32),
    },
    /// Finger is on the bar, dragging the window.
    Grabbing {
        toplevel: ToplevelId,
        app_id: String,
        tracker: Tracker,
    },
    /// Released — spring-animating toward a target.
    Settling {
        toplevel: ToplevelId,
        app_id: String,
        target: NavTarget,
        /// Spring animating toward rest (0 = app fullscreen, 1 = target reached).
        progress: Spring,
        icon_center: (f32, f32),
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

    /// Get the foreground toplevel id if any app is visible/animating.
    pub fn foreground_toplevel(&self) -> Option<ToplevelId> {
        match self {
            UiState::App { toplevel, .. }
            | UiState::AppOpening { toplevel, .. }
            | UiState::AppClosing { toplevel, .. }
            | UiState::Grabbing { toplevel, .. }
            | UiState::Settling { toplevel, .. } => Some(*toplevel),
            UiState::Home { .. } => None,
        }
    }

    /// Whether the state needs animation ticks (springs not settled).
    pub fn needs_animation(&self) -> bool {
        match self {
            UiState::AppOpening { progress, .. } => !progress.is_settled(),
            UiState::AppClosing { progress, .. } => !progress.is_settled(),
            UiState::Settling { progress, .. } => !progress.is_settled(),
            UiState::Home { page_spring, .. } => !page_spring.is_settled(),
            UiState::Grabbing { .. } => true,
            UiState::App { .. } => false,
        }
    }
}

/// Events the UI state machine accepts.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// Icon tapped — launch or raise.
    TapIcon {
        app_id: String,
        /// Icon center for zoom-origin.
        icon_center: (f32, f32),
    },
    /// App launched and matched to a toplevel (with zoom animation).
    AppMapped {
        toplevel: ToplevelId,
        app_id: String,
        icon_center: (f32, f32),
    },
    /// Raise an already-running app directly (no zoom animation).
    RaiseApp {
        toplevel: ToplevelId,
        app_id: String,
    },
    /// Return-home (Esc shortcut in dev).
    ReturnHome { icon_center: (f32, f32) },
    /// Foreground app's toplevel was destroyed.
    ToplevelClosed { toplevel: ToplevelId },
    /// Horizontal page swipe delta.
    PageDrag { delta: f32 },
    /// Page swipe released.
    PageRelease,
    /// Finger down on bar zone — start grab.
    GrabStart { point: sc_input::Pt },
    /// Finger moved during grab.
    GrabMove { point: sc_input::Pt, dt: f32 },
    /// Finger released during grab.
    GrabRelease,
    /// Touch-down while animating (interrupt).
    Interrupt { point: sc_input::Pt },
    /// Animation tick — advance springs by dt.
    Tick { dt: f32 },
}

/// Side effect from a transition.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Launch { app_id: String },
    None,
}

/// Advance the state machine.
pub fn transition(state: &mut UiState, event: UiEvent) -> Effect {
    match event {
        UiEvent::TapIcon { app_id, .. } => {
            if matches!(state, UiState::Home { .. }) {
                return Effect::Launch { app_id };
            }
            Effect::None
        }
        UiEvent::AppMapped {
            toplevel,
            app_id,
            icon_center,
        } => {
            let mut progress = Spring::new(0.0);
            progress.stiffness = 300.0;
            progress.damping = 35.0;
            progress.retarget(1.0);
            *state = UiState::AppOpening {
                toplevel,
                app_id,
                progress,
                icon_center,
            };
            Effect::None
        }
        UiEvent::RaiseApp { toplevel, app_id } => {
            *state = UiState::App { toplevel, app_id };
            Effect::None
        }
        UiEvent::ReturnHome { icon_center } => {
            match state {
                UiState::App { toplevel, app_id, .. } => {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    let mut progress = Spring::new(1.0);
                    progress.stiffness = 300.0;
                    progress.damping = 35.0;
                    progress.retarget(0.0);
                    *state = UiState::AppClosing {
                        toplevel,
                        app_id,
                        progress,
                        icon_center,
                    };
                }
                UiState::Grabbing { toplevel, app_id, .. }
                | UiState::Settling { toplevel, app_id, .. } => {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    let mut progress = Spring::new(1.0);
                    progress.stiffness = 300.0;
                    progress.damping = 35.0;
                    progress.retarget(0.0);
                    *state = UiState::AppClosing {
                        toplevel,
                        app_id,
                        progress,
                        icon_center,
                    };
                }
                _ => {}
            }
            Effect::None
        }
        UiEvent::ToplevelClosed { toplevel } => {
            let is_foreground = match state {
                UiState::App { toplevel: t, .. }
                | UiState::AppOpening { toplevel: t, .. }
                | UiState::AppClosing { toplevel: t, .. }
                | UiState::Grabbing { toplevel: t, .. }
                | UiState::Settling { toplevel: t, .. } => *t == toplevel,
                _ => false,
            };
            if is_foreground {
                *state = UiState::home(0, 1);
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
                let target =
                    (*page as f32 + delta).clamp(0.0, (*page_count).saturating_sub(1) as f32);
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
                let nearest = page_spring
                    .value
                    .round()
                    .clamp(0.0, (*page_count).saturating_sub(1) as f32)
                    as usize;
                *page = nearest;
                page_spring.retarget(nearest as f32);
            }
            Effect::None
        }
        UiEvent::GrabStart { point } => {
            if let UiState::App { toplevel, app_id, .. } = state {
                let toplevel = *toplevel;
                let app_id = app_id.clone();
                *state = UiState::Grabbing {
                    toplevel,
                    app_id,
                    tracker: Tracker::begin(point),
                };
            }
            Effect::None
        }
        UiEvent::GrabMove { point, dt } => {
            if let UiState::Grabbing { tracker, .. } = state {
                tracker.update(point, dt);
            }
            Effect::None
        }
        UiEvent::GrabRelease => {
            if let UiState::Grabbing {
                toplevel,
                app_id,
                tracker,
            } = state
            {
                let target = sc_input::classify_release(tracker);
                let toplevel = *toplevel;
                let app_id = app_id.clone();
                // Start from current drag progress.
                let current_progress = tracker.up_progress().clamp(0.0, 1.0);
                let settle_target = match target {
                    NavTarget::BackToApp => 0.0,
                    NavTarget::Home | NavTarget::Switcher => 1.0,
                    NavTarget::QuickSwitch(_) => 1.0,
                };
                let mut progress = Spring::new(current_progress);
                progress.stiffness = 280.0;
                progress.damping = 32.0;
                progress.velocity = -tracker.velocity.y; // upward velocity → positive progress velocity
                progress.retarget(settle_target);
                *state = UiState::Settling {
                    toplevel,
                    app_id,
                    target,
                    progress,
                    icon_center: (0.5, 0.5), // will be overridden by caller with actual icon center
                };
            }
            Effect::None
        }
        UiEvent::Interrupt { point } => {
            match state {
                UiState::Settling {
                    toplevel, app_id, ..
                }
                | UiState::AppClosing {
                    toplevel, app_id, ..
                } => {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    *state = UiState::Grabbing {
                        toplevel,
                        app_id,
                        tracker: Tracker::begin(point),
                    };
                }
                UiState::AppOpening {
                    toplevel, app_id, ..
                } => {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    *state = UiState::App { toplevel, app_id };
                }
                _ => {}
            }
            Effect::None
        }
        UiEvent::Tick { dt } => {
            match state {
                UiState::AppOpening {
                    toplevel,
                    app_id,
                    progress,
                    ..
                } => {
                    progress.step(dt);
                    if progress.is_settled() {
                        let toplevel = *toplevel;
                        let app_id = app_id.clone();
                        *state = UiState::App { toplevel, app_id };
                    }
                }
                UiState::AppClosing { progress, .. } => {
                    progress.step(dt);
                    if progress.is_settled() {
                        *state = UiState::home(0, 1);
                    }
                }
                UiState::Settling {
                    toplevel,
                    app_id,
                    target,
                    progress,
                    ..
                } => {
                    progress.step(dt);
                    if progress.is_settled() {
                        match target {
                            NavTarget::BackToApp => {
                                let toplevel = *toplevel;
                                let app_id = app_id.clone();
                                *state = UiState::App { toplevel, app_id };
                            }
                            NavTarget::Home | NavTarget::Switcher => {
                                *state = UiState::home(0, 1);
                            }
                            NavTarget::QuickSwitch(_) => {
                                // Handled by caller raising the adjacent app.
                                *state = UiState::home(0, 1);
                            }
                        }
                    }
                }
                UiState::Home { page_spring, .. } => {
                    page_spring.step(dt);
                }
                _ => {}
            }
            Effect::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_input::Pt;

    #[test]
    fn tap_icon_produces_launch_effect() {
        let mut state = UiState::home(0, 1);
        let effect = transition(
            &mut state,
            UiEvent::TapIcon {
                app_id: "org.foo.Bar".into(),
                icon_center: (100.0, 200.0),
            },
        );
        assert!(matches!(effect, Effect::Launch { app_id } if app_id == "org.foo.Bar"));
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn app_mapped_starts_opening_animation() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "foo".into(),
                icon_center: (100.0, 200.0),
            },
        );
        assert!(matches!(state, UiState::AppOpening { toplevel: 1, .. }));
    }

    #[test]
    fn opening_settles_to_app() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "foo".into(),
                icon_center: (100.0, 200.0),
            },
        );
        // Tick until settled.
        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if matches!(state, UiState::App { .. }) {
                break;
            }
        }
        assert!(matches!(state, UiState::App { toplevel: 1, .. }));
    }

    #[test]
    fn grab_start_from_app() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "x".into(),
        };
        transition(
            &mut state,
            UiEvent::GrabStart {
                point: Pt { x: 0.5, y: 0.97 },
            },
        );
        assert!(matches!(state, UiState::Grabbing { toplevel: 1, .. }));
    }

    #[test]
    fn grab_release_back_to_app() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "x".into(),
        };
        // Start grab.
        transition(
            &mut state,
            UiEvent::GrabStart {
                point: Pt { x: 0.5, y: 0.95 },
            },
        );
        // Tiny move up (below threshold).
        transition(
            &mut state,
            UiEvent::GrabMove {
                point: Pt { x: 0.5, y: 0.92 },
                dt: 1.0 / 90.0,
            },
        );
        // Release.
        transition(&mut state, UiEvent::GrabRelease);
        assert!(matches!(state, UiState::Settling { .. }));
        if let UiState::Settling { target, .. } = &state {
            assert_eq!(*target, NavTarget::BackToApp);
        }
        // Tick until settled.
        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if matches!(state, UiState::App { .. }) {
                break;
            }
        }
        assert!(matches!(state, UiState::App { toplevel: 1, .. }));
    }

    #[test]
    fn grab_release_home_on_flick() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "x".into(),
        };
        transition(
            &mut state,
            UiEvent::GrabStart {
                point: Pt { x: 0.5, y: 0.95 },
            },
        );
        // Fast upward flick.
        if let UiState::Grabbing { tracker, .. } = &mut state {
            tracker.current = Pt { x: 0.5, y: 0.70 };
            tracker.velocity = Pt { x: 0.0, y: -3.0 };
        }
        transition(&mut state, UiEvent::GrabRelease);
        if let UiState::Settling { target, .. } = &state {
            assert_eq!(*target, NavTarget::Home);
        }
        // Tick until home.
        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if matches!(state, UiState::Home { .. }) {
                break;
            }
        }
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn interrupt_settling_returns_to_grab() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "x".into(),
        };
        transition(
            &mut state,
            UiEvent::GrabStart {
                point: Pt { x: 0.5, y: 0.95 },
            },
        );
        if let UiState::Grabbing { tracker, .. } = &mut state {
            tracker.current = Pt { x: 0.5, y: 0.70 };
            tracker.velocity = Pt { x: 0.0, y: -3.0 };
        }
        transition(&mut state, UiEvent::GrabRelease);
        assert!(matches!(state, UiState::Settling { .. }));
        // Interrupt mid-settle.
        transition(
            &mut state,
            UiEvent::Interrupt {
                point: Pt { x: 0.5, y: 0.80 },
            },
        );
        assert!(matches!(state, UiState::Grabbing { toplevel: 1, .. }));
    }

    #[test]
    fn interrupt_opening_jumps_to_app() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "foo".into(),
                icon_center: (100.0, 200.0),
            },
        );
        assert!(matches!(state, UiState::AppOpening { .. }));
        transition(
            &mut state,
            UiEvent::Interrupt {
                point: Pt { x: 0.5, y: 0.5 },
            },
        );
        assert!(matches!(state, UiState::App { toplevel: 1, .. }));
    }

    #[test]
    fn toplevel_closed_during_grab() {
        let mut state = UiState::Grabbing {
            toplevel: 3,
            app_id: "x".into(),
            tracker: Tracker::begin(Pt { x: 0.5, y: 0.9 }),
        };
        transition(&mut state, UiEvent::ToplevelClosed { toplevel: 3 });
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn return_home_starts_closing() {
        let mut state = UiState::App {
            toplevel: 2,
            app_id: "x".into(),
        };
        transition(
            &mut state,
            UiEvent::ReturnHome {
                icon_center: (200.0, 400.0),
            },
        );
        assert!(matches!(state, UiState::AppClosing { toplevel: 2, .. }));
    }

    #[test]
    fn closing_settles_to_home() {
        let mut state = UiState::App {
            toplevel: 2,
            app_id: "x".into(),
        };
        transition(
            &mut state,
            UiEvent::ReturnHome {
                icon_center: (200.0, 400.0),
            },
        );
        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if matches!(state, UiState::Home { .. }) {
                break;
            }
        }
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn page_release_snaps_to_nearest() {
        let mut state = UiState::home(0, 3);
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
