//! Pure UI state machine for springchick.
//!
//! `UiState` + `UiEvent` → `UiState` transitions, unit-tested without Wayland/GPU.

use sc_anim::Spring;
use sc_input::{NavTarget, Tracker};
use tracing::info;

/// Opaque toplevel identifier (index into the compositor's toplevel vec).
pub type ToplevelId = usize;

/// Seconds for a cancelled close-swipe to spring back to rest.
const CLOSE_SPRINGBACK_SECS: f32 = 0.18;

/// Origin of a zoom animation: where the window grows from / shrinks to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomOrigin {
    /// Center in logical pixels.
    pub center: (f32, f32),
    /// Start scale (icon ≈ 0.1, switcher card ≈ 0.62).
    pub scale: f32,
}

impl ZoomOrigin {
    pub fn icon(center: (f32, f32)) -> Self {
        Self { center, scale: 0.1 }
    }
    pub fn card(center: (f32, f32), scale: f32) -> Self {
        Self { center, scale }
    }
}

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
        /// Zoom origin (center + start scale).
        origin: ZoomOrigin,
    },
    /// Fullscreen → icon shrink animation.
    AppClosing {
        toplevel: ToplevelId,
        app_id: String,
        /// Spring 1→0.
        progress: Spring,
        origin: ZoomOrigin,
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
        origin: ZoomOrigin,
    },
    /// Switcher deck: fanned stack of running apps.
    Switcher {
        /// MRU card order; cards[0] = front (most recent).
        cards: Vec<ToplevelId>,
        /// Carousel focus spring (continuous card index).
        scroll: Spring,
        /// Card being swiped up to close: `(toplevel, progress 0..1, releasing)`.
        /// `releasing` = finger let go below the commit threshold, so `Tick`
        /// decays progress back to 0. `None` at rest.
        close: Option<(ToplevelId, f32, bool)>,
        /// Deck fan-in animation, 0→1 (cards spread out from behind the front).
        entry: Spring,
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
            UiState::Switcher { cards, .. } => cards.first().copied(),
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
            UiState::Switcher {
                scroll,
                close,
                entry,
                ..
            } => !scroll.is_settled() || !entry.is_settled() || matches!(close, Some((_, _, true))),
        }
    }
}

/// Events the UI state machine accepts.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// App launched and matched to a toplevel (with zoom animation).
    AppMapped {
        toplevel: ToplevelId,
        app_id: String,
        origin: ZoomOrigin,
    },
    /// Raise an already-running app directly (no zoom animation).
    RaiseApp {
        toplevel: ToplevelId,
        app_id: String,
    },
    /// Return-home (Esc shortcut in dev).
    ReturnHome { origin: ZoomOrigin },
    /// Foreground app's toplevel was destroyed.
    ToplevelClosed { toplevel: ToplevelId },
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
    /// Enter switcher deck from grab release.
    EnterSwitcher { cards: Vec<ToplevelId> },
    /// Tap a card to open that app.
    SwitcherTapCard {
        toplevel: ToplevelId,
        origin: ZoomOrigin,
    },
    /// Swipe a card up to close.
    SwitcherCloseCard { toplevel: ToplevelId },
    /// Dismiss the switcher (tap empty area).
    SwitcherDismiss,
}

/// Side effect from a transition.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    CloseToplevel {
        toplevel: ToplevelId,
    },
    /// Settling animation resolved to Switcher — caller should populate cards.
    EnterSwitcher,
    None,
}

/// Which toplevel should hold keyboard focus in this state.
///
/// Only the settled `App` state focuses a client: during zoom, grab, settle and
/// switcher the compositor owns the screen, and a mapped-but-hidden app must not
/// eat keys.
pub fn desired_focus(state: &UiState) -> Option<ToplevelId> {
    match state {
        UiState::App { toplevel, .. } => Some(*toplevel),
        _ => None,
    }
}

/// Advance the state machine.
pub fn transition(state: &mut UiState, event: UiEvent) -> Effect {
    match event {
        UiEvent::AppMapped {
            toplevel,
            app_id,
            origin,
        } => {
            *state = UiState::AppOpening {
                toplevel,
                app_id,
                progress: Spring::zoom(0.0, 1.0),
                origin,
            };
            Effect::None
        }
        UiEvent::RaiseApp { toplevel, app_id } => {
            *state = UiState::App { toplevel, app_id };
            Effect::None
        }
        UiEvent::ReturnHome { origin } => {
            match state {
                UiState::App {
                    toplevel, app_id, ..
                }
                | UiState::Grabbing {
                    toplevel, app_id, ..
                }
                | UiState::Settling {
                    toplevel, app_id, ..
                } => {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    *state = UiState::AppClosing {
                        toplevel,
                        app_id,
                        progress: Spring::zoom(1.0, 0.0),
                        origin,
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
            // Remove from switcher deck if present.
            if let UiState::Switcher { cards, .. } = state {
                cards.retain(|&t| t != toplevel);
                if cards.is_empty() {
                    *state = UiState::home(0, 1);
                }
            }
            Effect::None
        }
        UiEvent::GrabStart { point } => {
            if let UiState::App {
                toplevel, app_id, ..
            } = state
            {
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
                info!(target: "springchick::debug", "GrabRelease target={:?} progress={} vel={}", target, tracker.up_progress(), tracker.velocity.y);
                let toplevel = *toplevel;
                let app_id = app_id.clone();
                // Start from current drag progress.
                let current_progress = tracker.up_progress().clamp(0.0, 1.0);
                let settle_target = match target {
                    NavTarget::BackToApp => 0.0,
                    NavTarget::Home | NavTarget::Switcher | NavTarget::QuickSwitch(_) => 1.0,
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
                    origin: ZoomOrigin::icon((0.5, 0.5)), // will be overridden by caller with actual origin
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
                        info!(target: "springchick::debug", "Settling resolved target={:?}", target);
                        match target {
                            NavTarget::BackToApp => {
                                let toplevel = *toplevel;
                                let app_id = app_id.clone();
                                *state = UiState::App { toplevel, app_id };
                            }
                            NavTarget::Home => {
                                *state = UiState::home(0, 1);
                            }
                            NavTarget::Switcher => {
                                *state = UiState::home(0, 1);
                                return Effect::EnterSwitcher;
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
                UiState::Switcher {
                    scroll,
                    close,
                    entry,
                    ..
                } => {
                    scroll.step(dt);
                    entry.step(dt);
                    // Decay a cancelled close-swipe back to rest.
                    if let Some((_, progress, true)) = close {
                        *progress -= dt / CLOSE_SPRINGBACK_SECS;
                        if *progress <= 0.0 {
                            *close = None;
                        }
                    }
                }
                UiState::Grabbing { tracker, .. } => {
                    // Decay velocity so a stationary hold doesn't read as a flick.
                    tracker.decay(dt);
                }
                _ => {}
            }
            Effect::None
        }
        UiEvent::EnterSwitcher { cards } => {
            info!(target: "springchick::debug", "EnterSwitcher cards={:?}", cards);
            let mut entry = Spring::new(0.0);
            // Snappy launch so the fan unfolds immediately (no perceived delay
            // from a slow spring ease-in). Near-critical damping keeps it clean.
            entry.stiffness = 900.0;
            entry.damping = 54.0;
            entry.retarget(1.0);
            *state = UiState::Switcher {
                cards,
                scroll: Spring::new(0.0),
                close: None,
                entry,
            };
            Effect::None
        }
        UiEvent::SwitcherTapCard { toplevel, origin } => {
            if let UiState::Switcher { cards, .. } = state {
                if cards.contains(&toplevel) {
                    let app_id = format!("app_{}", toplevel);
                    *state = UiState::AppOpening {
                        toplevel,
                        app_id,
                        progress: Spring::zoom(0.0, 1.0),
                        origin,
                    };
                }
            }
            Effect::None
        }
        UiEvent::SwitcherCloseCard { toplevel } => {
            if let UiState::Switcher { cards, close, .. } = state {
                if let Some(pos) = cards.iter().position(|&t| t == toplevel) {
                    cards.remove(pos);
                    *close = None;
                    if cards.is_empty() {
                        *state = UiState::home(0, 1);
                    }
                    return Effect::CloseToplevel { toplevel };
                }
            }
            Effect::None
        }
        UiEvent::SwitcherDismiss => {
            *state = UiState::home(0, 1);
            Effect::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_input::Pt;

    #[test]
    fn app_mapped_starts_opening_animation() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "foo".into(),
                origin: ZoomOrigin::icon((100.0, 200.0)),
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
                origin: ZoomOrigin::icon((100.0, 200.0)),
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
                origin: ZoomOrigin::icon((100.0, 200.0)),
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
                origin: ZoomOrigin::icon((200.0, 400.0)),
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
                origin: ZoomOrigin::icon((200.0, 400.0)),
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

    // --- Switcher tests ---

    #[test]
    fn switcher_preview_release_enters_switcher() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "a".into(),
        };
        transition(
            &mut state,
            UiEvent::EnterSwitcher {
                cards: vec![1, 2, 3],
            },
        );
        assert!(matches!(state, UiState::Switcher { .. }));
        if let UiState::Switcher { cards, .. } = &state {
            assert_eq!(cards, &vec![1, 2, 3]);
        }
    }

    #[test]
    fn settling_to_switcher_emits_effect() {
        let mut state = UiState::Settling {
            toplevel: 1,
            app_id: "a".into(),
            target: sc_input::NavTarget::Switcher,
            progress: Spring::new(1.0),
            origin: ZoomOrigin::icon((0.5, 0.5)),
        };
        let eff = transition(&mut state, UiEvent::Tick { dt: 1.0 / 60.0 });
        assert!(matches!(eff, Effect::EnterSwitcher));
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn tap_card_opens_that_toplevel() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        // Events carry the toplevel id, not a positional index — so the render
        // z-order and the MRU order can never desync (regression: tapping the
        // front card used to open the mirrored back card).
        let _eff = transition(
            &mut state,
            UiEvent::SwitcherTapCard {
                toplevel: 3,
                origin: ZoomOrigin::card((600.0, 1350.0), 0.62),
            },
        );
        assert!(matches!(state, UiState::AppOpening { toplevel: 3, .. }));
    }

    #[test]
    fn close_card_removes_and_emits_effect() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        let eff = transition(&mut state, UiEvent::SwitcherCloseCard { toplevel: 2 });
        assert_eq!(eff, Effect::CloseToplevel { toplevel: 2 });
        if let UiState::Switcher { cards, .. } = &state {
            assert_eq!(cards, &vec![1, 3]);
        } else {
            panic!("still in switcher");
        }
    }

    #[test]
    fn tap_unknown_toplevel_is_noop() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        transition(
            &mut state,
            UiEvent::SwitcherTapCard {
                toplevel: 99,
                origin: ZoomOrigin::card((600.0, 1350.0), 0.62),
            },
        );
        assert!(matches!(state, UiState::Switcher { .. }));
    }

    #[test]
    fn close_last_card_goes_home() {
        let mut state = UiState::Switcher {
            cards: vec![9],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        transition(&mut state, UiEvent::SwitcherCloseCard { toplevel: 9 });
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn dismiss_goes_home() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        transition(&mut state, UiEvent::SwitcherDismiss);
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn toplevel_closed_removes_card_from_switcher() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        transition(&mut state, UiEvent::ToplevelClosed { toplevel: 2 });
        if let UiState::Switcher { cards, .. } = &state {
            assert_eq!(cards, &vec![1, 3]);
        } else {
            panic!("expected still switcher");
        }
    }

    #[test]
    fn switcher_needs_animation_while_scroll_moving() {
        let mut spring = Spring::new(0.0);
        spring.retarget(1.0);
        let state = UiState::Switcher {
            cards: vec![1],
            scroll: spring,
            close: None,
            entry: Spring::new(1.0),
        };
        assert!(state.needs_animation());
    }

    #[test]
    fn switcher_foreground_toplevel_returns_front_card() {
        let state = UiState::Switcher {
            cards: vec![5, 3, 1],
            scroll: Spring::new(0.0),
            close: None,
            entry: Spring::new(1.0),
        };
        assert_eq!(state.foreground_toplevel(), Some(5));
    }
}
