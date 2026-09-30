//! Pure UI state machine: `transition(state, event) -> Effect`.

use sc_anim::Spring;
use sc_input::{NavTarget, Tracker};
use tracing::debug;

/// Index into the compositor's toplevel vec.
pub type ToplevelId = usize;

/// `progress > 0` lifts toward closing, `< 0` pushes below the stack. Pinned
/// to the finger while dragging; on a short release it springs back to 0.
#[derive(Clone, Copy, Debug)]
pub struct CardClose {
    pub toplevel: ToplevelId,
    pub progress: Spring,
    pub releasing: bool,
}

impl CardClose {
    pub fn dragging(toplevel: ToplevelId, progress: f32) -> Self {
        let mut s = close_spring();
        s.value = progress;
        s.target = progress;
        s.velocity = 0.0;
        Self {
            toplevel,
            progress: s,
            releasing: false,
        }
    }

    /// Carries the finger's speed into the springback. `vy` is in heights/s,
    /// negative upward (opposite sign to progress).
    pub fn release(&mut self, vy: f32) {
        // Unclamped, a fast release dives off-screen and back. Faster upward would
        // have committed anyway.
        self.progress.velocity = (-vy).clamp(-CLOSE_RELEASE_MAX_SPEED, CLOSE_RELEASE_MAX_SPEED);
        self.progress.retarget(0.0);
        self.releasing = true;
    }
}

/// Matches the close flick threshold.
const CLOSE_RELEASE_MAX_SPEED: f32 = 0.9;

/// Under-damped (critical ≈ 36) for one small bounce past rest.
fn close_spring() -> Spring {
    let mut s = Spring::new(0.0);
    s.stiffness = 320.0;
    s.damping = 26.0;
    s
}

/// Window → icon shrink, much stiffer than `Spring::zoom`: there's nothing
/// to watch while it runs. `MINIMIZE_HANDOVER` cuts the tail.
fn minimize_spring(from: f32, to: f32) -> Spring {
    let mut s = Spring::new(from);
    s.stiffness = 760.0;
    s.damping = 55.0;
    s.retarget(to);
    s
}

/// Heights/s, for a ~3% lift settling in under a third of a second.
const HOME_BOUNCE_KICK: f32 = 1.1;

/// Hand over to the deck here instead of waiting for `is_settled`; the rest
/// is sub-pixel and the deck can't be stepped until it exists.
const SWITCHER_HANDOVER: f32 = 0.985;

/// The shrink's tail reads as a hitch before Home appears.
const MINIMIZE_HANDOVER: f32 = 0.95;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomOrigin {
    /// Logical px.
    pub center: (f32, f32),
    /// Icon ≈ 0.1, switcher card ≈ 0.62.
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpenMode {
    Zoom,
    /// The search app; it has no icon origin.
    SlideUp,
    /// Home-bar rightward swipe: Home slides right, uncovering the top card.
    SlideFromLeft,
}

#[derive(Clone, Debug)]
pub enum UiState {
    Home {
        page: usize,
        page_spring: Spring,
        page_count: usize,
        /// Fractions of screen height, positive = up. Kicked by
        /// [`UiEvent::HomeBounce`].
        bounce: Spring,
    },
    App {
        toplevel: ToplevelId,
        app_id: String,
    },
    AppOpening {
        toplevel: ToplevelId,
        app_id: String,
        /// 0 = icon size, 1 = fullscreen.
        progress: Spring,
        origin: ZoomOrigin,
        open_mode: OpenMode,
    },
    AppClosing {
        toplevel: ToplevelId,
        app_id: String,
        progress: Spring,
        origin: ZoomOrigin,
    },
    /// Finger on the bar, dragging the window.
    Grabbing {
        toplevel: ToplevelId,
        app_id: String,
        tracker: Tracker,
        /// MRU deck for the live fan, filled in by the caller after the grab starts.
        cards: Vec<ToplevelId>,
    },
    Settling {
        toplevel: ToplevelId,
        app_id: String,
        target: NavTarget,
        /// 0 = app fullscreen, 1 = target reached.
        progress: Spring,
        origin: ZoomOrigin,
        /// Kept only for a `Switcher` target, so the fan stays up through the
        /// settle.
        cards: Vec<ToplevelId>,
    },
    /// The current app follows the finger sideways, revealing the MRU
    /// neighbour. Rubber-bands at the ends.
    QuickSwitch {
        current: ToplevelId,
        current_app: String,
        /// Revealed by a rightward swipe: the older app.
        prev: Option<(ToplevelId, String)>,
        /// Revealed by a leftward swipe: the more recent app.
        next: Option<(ToplevelId, String)>,
        /// Fraction of screen width; `+` slides right, revealing `prev`.
        offset: Spring,
        /// `Some` = settling onto this app, `None` = springing back. Read once
        /// `releasing`.
        commit: Option<(ToplevelId, String)>,
        releasing: bool,
        /// Where `offset` is 0.
        start_x: f32,
        /// Normalized grab origin, so an upward drag can return to `Grabbing` with a
        /// continuous `up_progress`.
        origin: sc_input::Pt,
    },
    Switcher {
        /// cards[0] = most recent.
        cards: Vec<ToplevelId>,
        /// Continuous card index.
        scroll: Spring,
        close: Option<CardClose>,
        /// 0 = below the bottom edge, 1 = at rest. Starts at 1 from a grab (the
        /// settle already placed it); only the Home-bar swipe-up plays the rise.
        /// Retargeted to 0 to leave; settled at 0 tells `Tick` the exit is done.
        enter: Spring,
        /// Exit off the left edge instead of sinking. Set by tap-outside dismiss.
        exit_left: bool,
    },
}

/// Stiff and under-damped, so a kick reads as a rebound.
fn bounce_spring() -> Spring {
    let mut s = Spring::new(0.0);
    s.stiffness = 500.0;
    s.damping = 26.0;
    s
}

impl UiState {
    pub fn home(page: usize, page_count: usize) -> Self {
        let mut spring = Spring::new(page as f32);
        spring.retarget(page as f32);
        UiState::Home {
            page,
            page_spring: spring,
            page_count,
            bounce: bounce_spring(),
        }
    }

    /// Clients set their xdg `app_id` after map, so retag the live UI.
    pub fn retag_app(&mut self, toplevel: ToplevelId, app_id: &str) {
        let set = |a: &mut String| *a = app_id.to_string();
        match self {
            UiState::App {
                toplevel: t,
                app_id: a,
                ..
            }
            | UiState::AppOpening {
                toplevel: t,
                app_id: a,
                ..
            }
            | UiState::AppClosing {
                toplevel: t,
                app_id: a,
                ..
            }
            | UiState::Grabbing {
                toplevel: t,
                app_id: a,
                ..
            }
            | UiState::Settling {
                toplevel: t,
                app_id: a,
                ..
            } if *t == toplevel => {
                set(a);
            }
            UiState::QuickSwitch {
                current,
                current_app,
                prev,
                next,
                commit,
                ..
            } => {
                if *current == toplevel {
                    set(current_app);
                }
                for (t, a) in [prev, next, commit].into_iter().flatten() {
                    if *t == toplevel {
                        set(a);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn foreground_toplevel(&self) -> Option<ToplevelId> {
        match self {
            UiState::App { toplevel, .. }
            | UiState::AppOpening { toplevel, .. }
            | UiState::AppClosing { toplevel, .. }
            | UiState::Grabbing { toplevel, .. }
            | UiState::Settling { toplevel, .. } => Some(*toplevel),
            UiState::QuickSwitch { current, .. } => Some(*current),
            UiState::Home { .. } => None,
            UiState::Switcher { cards, .. } => cards.first().copied(),
        }
    }

    pub fn needs_animation(&self) -> bool {
        match self {
            UiState::AppOpening { progress, .. } => !progress.is_settled(),
            UiState::AppClosing { progress, .. } => !progress.is_settled(),
            UiState::Settling { progress, .. } => !progress.is_settled(),
            UiState::Home {
                page_spring,
                bounce,
                ..
            } => !page_spring.is_settled() || !bounce.is_settled(),
            UiState::Grabbing { .. } => true,
            // Dragging repaints on move; releasing must tick until settled.
            UiState::QuickSwitch {
                releasing, offset, ..
            } => *releasing && !offset.is_settled(),
            UiState::App { .. } => false,
            UiState::Switcher {
                scroll,
                close,
                enter,
                ..
            } => {
                !scroll.is_settled()
                    || !enter.is_settled()
                    || close.is_some_and(|c| c.releasing && !c.progress.is_settled())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum UiEvent {
    AppMapped {
        toplevel: ToplevelId,
        app_id: String,
        origin: ZoomOrigin,
        open_mode: OpenMode,
    },
    /// No zoom animation.
    RaiseApp {
        toplevel: ToplevelId,
        app_id: String,
    },
    ReturnHome {
        origin: ZoomOrigin,
    },
    /// `next` is the fallback when the closed one was in front, resolved by the
    /// caller from MRU. `Some` only for a dismissed dialog; an app close goes
    /// Home.
    ToplevelClosed {
        toplevel: ToplevelId,
        next: Option<(ToplevelId, String)>,
    },
    GrabStart {
        point: sc_input::Pt,
    },
    GrabMove {
        point: sc_input::Pt,
        dt: f32,
    },
    GrabRelease,
    /// Touch-down while animating.
    Interrupt {
        point: sc_input::Pt,
    },
    Tick {
        dt: f32,
    },
    /// From a grab release: the settle already fanned it open, so no entrance.
    EnterSwitcher {
        cards: Vec<ToplevelId>,
    },
    /// From Home (bar swipe-up): the deck rises from below.
    OpenSwitcherFromHome {
        cards: Vec<ToplevelId>,
    },
    /// From an app without a gesture (Super+Tab): the app shrinks into the
    /// front slot via the same `Settling` path as a grab release.
    OpenSwitcherFromApp {
        cards: Vec<ToplevelId>,
        origin: ZoomOrigin,
    },
    /// Negative = toward the more recent end. Wraps both ways.
    SwitcherStep {
        delta: i32,
    },
    /// Rubber-band Home and stay.
    HomeBounce,
    /// `app_id` is resolved by the caller; the deck tracks only ids.
    SwitcherTapCard {
        toplevel: ToplevelId,
        app_id: String,
        origin: ZoomOrigin,
    },
    SwitcherCloseCard {
        toplevel: ToplevelId,
    },
    /// Tap on empty area.
    SwitcherDismiss,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    CloseToplevel {
        toplevel: ToplevelId,
    },
    /// The caller populates the cards.
    EnterSwitcher,
    None,
}

/// Only the settled `App` state focuses a client; a hidden app must not eat
/// keys.
pub fn desired_focus(state: &UiState) -> Option<ToplevelId> {
    match state {
        UiState::App { toplevel, .. } => Some(*toplevel),
        _ => None,
    }
}

pub fn transition(state: &mut UiState, event: UiEvent) -> Effect {
    match event {
        UiEvent::AppMapped {
            toplevel,
            app_id,
            origin,
            open_mode,
        } => {
            *state = UiState::AppOpening {
                toplevel,
                app_id,
                progress: Spring::zoom(0.0, 1.0),
                origin,
                open_mode,
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
                        progress: minimize_spring(1.0, 0.0),
                        origin,
                    };
                }
                // From the deck, play its entrance backwards and land Home.
                UiState::Switcher { enter, .. } => enter.retarget(0.0),
                _ => {}
            }
            Effect::None
        }
        UiEvent::ToplevelClosed { toplevel, next } => {
            let is_foreground = match state {
                UiState::App { toplevel: t, .. }
                | UiState::AppOpening { toplevel: t, .. }
                | UiState::AppClosing { toplevel: t, .. }
                | UiState::Grabbing { toplevel: t, .. }
                | UiState::Settling { toplevel: t, .. }
                | UiState::QuickSwitch { current: t, .. } => *t == toplevel,
                _ => false,
            };
            if is_foreground {
                // A dismissed dialog (e.g. a portal file chooser, its own process) hands
                // back to the app underneath; an app close lands Home.
                *state = match next {
                    Some((t, app_id)) => UiState::App {
                        toplevel: t,
                        app_id,
                    },
                    None => UiState::home(0, 1),
                };
            }
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
                    cards: Vec::new(),
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
                cards,
            } = state
            {
                let target = sc_input::classify_release(tracker);
                debug!(target: "springchick::debug", "GrabRelease target={:?} progress={} vel={}", target, tracker.up_progress(), tracker.velocity.y);
                let toplevel = *toplevel;
                let app_id = app_id.clone();
                let cards = if matches!(target, NavTarget::Switcher) {
                    std::mem::take(cards)
                } else {
                    Vec::new()
                };
                let current_progress = tracker.up_progress().clamp(0.0, 1.0);
                let settle_target = match target {
                    NavTarget::BackToApp => 0.0,
                    NavTarget::Home | NavTarget::Switcher | NavTarget::QuickSwitch(_) => 1.0,
                };
                let mut progress = if matches!(target, NavTarget::Home) {
                    minimize_spring(current_progress, current_progress)
                } else {
                    let mut s = Spring::new(current_progress);
                    s.stiffness = 280.0;
                    s.damping = 32.0;
                    s
                };
                progress.velocity = -tracker.velocity.y; // upward velocity → positive progress
                progress.retarget(settle_target);
                *state = UiState::Settling {
                    toplevel,
                    app_id,
                    target,
                    progress,
                    origin: ZoomOrigin::icon((0.5, 0.5)), // overridden by the caller
                    cards,
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
                        cards: Vec::new(),
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
                    if progress.is_settled() || progress.value <= 1.0 - MINIMIZE_HANDOVER {
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
                    // Hand over just before the asymptotic tail, which only delays the first
                    // card step or touch.
                    let handover = progress.value
                        >= match target {
                            NavTarget::Switcher => SWITCHER_HANDOVER,
                            NavTarget::Home => MINIMIZE_HANDOVER,
                            _ => f32::INFINITY,
                        };
                    if progress.is_settled() || handover {
                        debug!(target: "springchick::debug", "Settling resolved target={:?}", target);
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
                                // The caller raises the adjacent app.
                                *state = UiState::home(0, 1);
                            }
                        }
                    }
                }
                UiState::Home {
                    page_spring,
                    bounce,
                    ..
                } => {
                    page_spring.step(dt);
                    bounce.step(dt);
                }
                UiState::Switcher {
                    scroll,
                    close,
                    enter,
                    ..
                } => {
                    scroll.step(dt);
                    enter.step(dt);
                    // Settled at 0: the deck has finished sinking.
                    if enter.target == 0.0 && enter.is_settled() {
                        *state = UiState::home(0, 1);
                        return Effect::None;
                    }
                    if let Some(c) = close {
                        if c.releasing {
                            c.progress.step(dt);
                            if c.progress.is_settled() {
                                *close = None;
                            }
                        }
                    }
                }
                UiState::Grabbing { tracker, .. } => {
                    // A still hold must not read as a flick.
                    tracker.decay(dt);
                }
                UiState::QuickSwitch {
                    current,
                    current_app,
                    commit,
                    offset,
                    releasing,
                    ..
                } if *releasing => {
                    offset.step(dt);
                    if offset.is_settled() {
                        // The committed neighbour, or back to where we started.
                        let (toplevel, app_id) = commit
                            .take()
                            .unwrap_or_else(|| (*current, current_app.clone()));
                        *state = UiState::App { toplevel, app_id };
                    }
                }
                _ => {}
            }
            Effect::None
        }
        UiEvent::EnterSwitcher { cards } => {
            debug!(target: "springchick::debug", "EnterSwitcher cards={:?}", cards);
            // Already fanned by the settle, so no entrance. Focus scrolls to cards[1]
            // (you came from cards[0]), so the card you left slides out right.
            let mut scroll = Spring::new(0.0);
            if cards.len() > 1 {
                scroll.retarget(1.0);
            }
            *state = UiState::Switcher {
                cards,
                scroll,
                close: None,
                enter: Spring::new(1.0),
                exit_left: false,
            };
            Effect::None
        }
        UiEvent::OpenSwitcherFromHome { cards } => {
            debug!(target: "springchick::debug", "OpenSwitcherFromHome cards={:?}", cards);
            // Only from Home; the grab path has its own entry.
            if matches!(state, UiState::Home { .. }) && !cards.is_empty() {
                *state = UiState::Switcher {
                    cards,
                    scroll: Spring::new(0.0),
                    close: None,
                    enter: Spring::zoom(0.0, 1.0),
                    exit_left: false,
                };
            }
            Effect::None
        }
        UiEvent::OpenSwitcherFromApp { cards, origin } => {
            if let UiState::App { toplevel, app_id } = state {
                if !cards.is_empty() {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    // Much stiffer than a finger's settle: the first Tab step waits for the
                    // handover.
                    let mut progress = Spring::zoom(0.0, 1.0);
                    progress.stiffness = 2000.0;
                    progress.damping = 90.0;
                    *state = UiState::Settling {
                        toplevel,
                        app_id,
                        target: NavTarget::Switcher,
                        progress,
                        origin,
                        cards,
                    };
                }
            }
            Effect::None
        }
        UiEvent::SwitcherStep { delta } => {
            if let UiState::Switcher { cards, scroll, .. } = state {
                let n = cards.len() as i32;
                if n > 0 {
                    // Step from the target, so repeats mid-flight each add a card.
                    let from = scroll.target.round() as i32;
                    scroll.retarget((from + delta).rem_euclid(n) as f32);
                }
            }
            Effect::None
        }
        UiEvent::HomeBounce => {
            if let UiState::Home { bounce, .. } = state {
                bounce.velocity = HOME_BOUNCE_KICK;
            }
            Effect::None
        }
        UiEvent::SwitcherTapCard {
            toplevel,
            app_id,
            origin,
        } => {
            if let UiState::Switcher { cards, .. } = state {
                if cards.contains(&toplevel) {
                    *state = UiState::AppOpening {
                        toplevel,
                        app_id,
                        progress: Spring::zoom(0.0, 1.0),
                        origin,
                        open_mode: OpenMode::Zoom,
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
            // Slide off the left edge; `Tick` lands Home when it settles at 0.
            if let UiState::Switcher {
                enter, exit_left, ..
            } = state
            {
                *exit_left = true;
                enter.retarget(0.0);
            } else {
                *state = UiState::home(0, 1);
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
    fn app_mapped_starts_opening_animation() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 1,
                app_id: "foo".into(),
                origin: ZoomOrigin::icon((100.0, 200.0)),
                open_mode: OpenMode::Zoom,
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
                open_mode: OpenMode::Zoom,
            },
        );
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
        transition(
            &mut state,
            UiEvent::GrabStart {
                point: Pt { x: 0.5, y: 0.95 },
            },
        );
        transition(
            &mut state,
            UiEvent::GrabMove {
                point: Pt { x: 0.5, y: 0.92 },
                dt: 1.0 / 90.0,
            },
        );
        transition(&mut state, UiEvent::GrabRelease);
        assert!(matches!(state, UiState::Settling { .. }));
        if let UiState::Settling { target, .. } = &state {
            assert_eq!(*target, NavTarget::BackToApp);
        }
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
        if let UiState::Grabbing { tracker, .. } = &mut state {
            tracker.current = Pt { x: 0.5, y: 0.70 };
            tracker.velocity = Pt { x: 0.0, y: -3.0 };
        }
        transition(&mut state, UiEvent::GrabRelease);
        if let UiState::Settling { target, .. } = &state {
            assert_eq!(*target, NavTarget::Home);
        }
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
                open_mode: OpenMode::Zoom,
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
            cards: Vec::new(),
        };
        transition(
            &mut state,
            UiEvent::ToplevelClosed {
                toplevel: 3,
                next: None,
            },
        );
        assert!(matches!(state, UiState::Home { .. }));
    }

    /// The portal file chooser case: closing it returns to the app that asked.
    #[test]
    fn toplevel_closed_returns_to_previous_app() {
        let mut state = UiState::App {
            toplevel: 7,
            app_id: "org.example.Picker".into(),
        };
        transition(
            &mut state,
            UiEvent::ToplevelClosed {
                toplevel: 7,
                next: Some((2, "org.example.Editor".into())),
            },
        );
        match state {
            UiState::App { toplevel, app_id } => {
                assert_eq!(toplevel, 2);
                assert_eq!(app_id, "org.example.Editor");
            }
            other => panic!("expected App, got {other:?}"),
        }
    }

    #[test]
    fn toplevel_closed_without_next_goes_home() {
        let mut state = UiState::App {
            toplevel: 7,
            app_id: "org.example.Picker".into(),
        };
        transition(
            &mut state,
            UiEvent::ToplevelClosed {
                toplevel: 7,
                next: None,
            },
        );
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
    fn entering_the_deck_from_an_app_focuses_the_second_card() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "a".into(),
        };
        transition(&mut state, UiEvent::EnterSwitcher { cards: vec![1, 2] });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("not in switcher");
        };
        assert_eq!(scroll.target, 1.0, "focus lands behind the app you left");
        assert_eq!(
            scroll.value, 0.0,
            "…by scrolling there, so card 1 slides off"
        );

        let mut state = UiState::App {
            toplevel: 1,
            app_id: "a".into(),
        };
        transition(&mut state, UiEvent::EnterSwitcher { cards: vec![1] });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("not in switcher");
        };
        assert_eq!(scroll.target, 0.0);
    }

    #[test]
    fn entering_the_deck_from_home_focuses_the_front_card() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::OpenSwitcherFromHome { cards: vec![1, 2] },
        );
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("not in switcher");
        };
        assert_eq!(scroll.target, 0.0);
    }

    #[test]
    fn settling_to_switcher_emits_effect() {
        let mut state = UiState::Settling {
            toplevel: 1,
            app_id: "a".into(),
            target: NavTarget::Switcher,
            progress: Spring::new(1.0),
            origin: ZoomOrigin::icon((0.5, 0.5)),
            cards: vec![1, 2, 3],
        };
        let eff = transition(&mut state, UiEvent::Tick { dt: 1.0 / 60.0 });
        assert!(matches!(eff, Effect::EnterSwitcher));
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn super_tab_from_an_app_settles_into_the_deck() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "a".into(),
        };
        transition(
            &mut state,
            UiEvent::OpenSwitcherFromApp {
                cards: vec![1, 2],
                origin: ZoomOrigin::card((900.0, 1350.0), 0.62),
            },
        );
        let UiState::Settling { target, cards, .. } = &state else {
            panic!("expected a settle, got {state:?}");
        };
        assert!(matches!(target, NavTarget::Switcher));
        assert_eq!(cards, &vec![1, 2], "the fan is carried through the settle");

        for _ in 0..500 {
            if matches!(
                transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 }),
                Effect::EnterSwitcher
            ) {
                return;
            }
        }
        panic!("settle never reached the switcher");
    }

    #[test]
    fn super_tab_reaches_the_deck_within_a_few_frames() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "a".into(),
        };
        transition(
            &mut state,
            UiEvent::OpenSwitcherFromApp {
                cards: vec![1, 2],
                origin: ZoomOrigin::card((900.0, 1350.0), 0.62),
            },
        );
        let mut frames = 0;
        loop {
            assert!(frames < 20, "deck took {frames} frames at 90Hz to appear");
            frames += 1;
            if matches!(
                transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 }),
                Effect::EnterSwitcher
            ) {
                break;
            }
        }
    }

    #[test]
    fn super_tab_from_an_app_needs_cards() {
        let mut state = UiState::App {
            toplevel: 1,
            app_id: "a".into(),
        };
        transition(
            &mut state,
            UiEvent::OpenSwitcherFromApp {
                cards: Vec::new(),
                origin: ZoomOrigin::icon((0.5, 0.5)),
            },
        );
        assert!(matches!(state, UiState::App { .. }));
    }

    #[test]
    fn switcher_step_springs_the_focus_and_wraps() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        transition(&mut state, UiEvent::SwitcherStep { delta: 1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 1.0);
        assert!(scroll.value < 1.0, "it springs there rather than jumping");

        transition(&mut state, UiEvent::SwitcherStep { delta: 1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 2.0);

        transition(&mut state, UiEvent::SwitcherStep { delta: 1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 0.0);

        transition(&mut state, UiEvent::SwitcherStep { delta: -1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 2.0);
    }

    #[test]
    fn home_from_the_switcher_sinks_the_deck_instead_of_cutting() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        transition(
            &mut state,
            UiEvent::ReturnHome {
                origin: ZoomOrigin::icon((100.0, 200.0)),
            },
        );
        assert!(
            matches!(state, UiState::Switcher { .. }),
            "the deck plays its exit first"
        );
        assert!(state.needs_animation());

        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if matches!(state, UiState::Home { .. }) {
                return;
            }
        }
        panic!("the deck never landed on Home");
    }

    #[test]
    fn home_bar_swipe_up_opens_the_switcher_rising_from_below() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::OpenSwitcherFromHome { cards: vec![4, 2] },
        );
        let UiState::Switcher { cards, enter, .. } = &state else {
            panic!("expected switcher, got {state:?}");
        };
        assert_eq!(cards, &vec![4, 2]);
        assert_eq!(enter.value, 0.0, "deck starts below the bottom edge");
        assert!(state.needs_animation(), "the rise must be ticked");

        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if !state.needs_animation() {
                break;
            }
        }
        let UiState::Switcher { enter, .. } = &state else {
            panic!("left the switcher");
        };
        assert!((enter.value - 1.0).abs() < 0.01, "enter={}", enter.value);
    }

    #[test]
    fn home_bar_swipe_up_with_one_app_still_opens_the_switcher() {
        let mut state = UiState::home(0, 1);
        transition(&mut state, UiEvent::OpenSwitcherFromHome { cards: vec![7] });
        assert!(matches!(state, UiState::Switcher { .. }));
    }

    #[test]
    fn opening_an_empty_switcher_is_a_noop() {
        let mut state = UiState::home(0, 1);
        transition(&mut state, UiEvent::OpenSwitcherFromHome { cards: vec![] });
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn home_bounce_rings_and_settles_back_to_rest() {
        let mut state = UiState::home(0, 1);
        assert!(!state.needs_animation());
        transition(&mut state, UiEvent::HomeBounce);
        assert!(state.needs_animation());

        let mut peak = 0.0_f32;
        for _ in 0..1000 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            let UiState::Home { bounce, .. } = &state else {
                panic!("bounce must not leave Home");
            };
            peak = peak.max(bounce.value);
            if !state.needs_animation() {
                break;
            }
        }
        assert!(!state.needs_animation(), "bounce never settled");
        assert!((0.01..0.08).contains(&peak), "peak lift was {peak}");
        let UiState::Home { bounce, .. } = &state else {
            unreachable!()
        };
        assert!(bounce.value.abs() < 0.001, "returned to rest");
    }

    #[test]
    fn home_bar_swipe_right_slides_in_from_the_side() {
        let mut state = UiState::home(0, 1);
        transition(
            &mut state,
            UiEvent::AppMapped {
                toplevel: 3,
                app_id: "x".into(),
                origin: ZoomOrigin::icon((100.0, 200.0)),
                open_mode: OpenMode::SlideFromLeft,
            },
        );
        assert!(matches!(
            state,
            UiState::AppOpening {
                toplevel: 3,
                open_mode: OpenMode::SlideFromLeft,
                ..
            }
        ));
        for _ in 0..500 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 90.0 });
            if matches!(state, UiState::App { .. }) {
                break;
            }
        }
        assert!(matches!(state, UiState::App { toplevel: 3, .. }));
    }

    #[test]
    fn tap_card_opens_that_toplevel() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        // Events carry the toplevel id, not a positional index, so z-order and MRU
        // order can't desync.
        let _eff = transition(
            &mut state,
            UiEvent::SwitcherTapCard {
                toplevel: 3,
                app_id: "org.foo.Bar".into(),
                origin: ZoomOrigin::card((600.0, 1350.0), 0.62),
            },
        );
        assert!(matches!(
            &state,
            UiState::AppOpening { toplevel: 3, app_id, .. } if app_id == "org.foo.Bar"
        ));
    }

    #[test]
    fn close_card_removes_and_emits_effect() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
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
    fn released_push_down_bounces_past_rest_then_settles() {
        let mut c = CardClose::dragging(2, -0.08);
        c.progress.retarget(0.0);
        c.releasing = true;
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: Some(c),
            enter: Spring::new(1.0),
            exit_left: false,
        };
        let dt = 1.0 / 90.0;
        let mut peak = -1.0_f32;
        for _ in 0..600 {
            transition(&mut state, UiEvent::Tick { dt });
            let UiState::Switcher { close, .. } = &state else {
                panic!("left the switcher");
            };
            match close {
                Some(c) => peak = peak.max(c.progress.value),
                None => break,
            }
        }
        assert!(peak > 0.001, "no bounce past rest: peak={peak}");
        assert!(peak < 0.02, "bounce too big: peak={peak}");
        assert!(
            matches!(&state, UiState::Switcher { close: None, .. }),
            "springback never settled"
        );
    }

    #[test]
    fn fast_downward_release_does_not_dive() {
        let mut c = CardClose::dragging(2, -0.08);
        c.release(6.0);
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: Some(c),
            enter: Spring::new(1.0),
            exit_left: false,
        };
        let dt = 1.0 / 90.0;
        let mut low = 0.0_f32;
        for _ in 0..600 {
            transition(&mut state, UiEvent::Tick { dt });
            let UiState::Switcher { close, .. } = &state else {
                panic!("left the switcher");
            };
            match close {
                Some(c) => low = low.min(c.progress.value),
                None => break,
            }
        }
        assert!(low > -0.15, "card dove off the deck: low={low}");
    }

    #[test]
    fn tap_unknown_toplevel_is_noop() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        transition(
            &mut state,
            UiEvent::SwitcherTapCard {
                toplevel: 99,
                app_id: "nope".into(),
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
            enter: Spring::new(1.0),
            exit_left: false,
        };
        transition(&mut state, UiEvent::SwitcherCloseCard { toplevel: 9 });
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn dismiss_slides_the_deck_out_left_then_lands_home() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        transition(&mut state, UiEvent::SwitcherDismiss);
        let UiState::Switcher {
            enter, exit_left, ..
        } = &state
        else {
            panic!("still on the deck while it slides out");
        };
        assert!(*exit_left && enter.target == 0.0);

        for _ in 0..600 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 60.0 });
        }
        assert!(matches!(state, UiState::Home { .. }));
    }

    #[test]
    fn toplevel_closed_removes_card_from_switcher() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        transition(
            &mut state,
            UiEvent::ToplevelClosed {
                toplevel: 2,
                next: None,
            },
        );
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
            enter: Spring::new(1.0),
            exit_left: false,
        };
        assert!(state.needs_animation());
    }

    #[test]
    fn switcher_foreground_toplevel_returns_front_card() {
        let state = UiState::Switcher {
            cards: vec![5, 3, 1],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
            exit_left: false,
        };
        assert_eq!(state.foreground_toplevel(), Some(5));
    }

    #[test]
    fn released_quick_switch_lands_on_commit() {
        let mut offset = Spring::new(0.0);
        offset.retarget(-1.0);
        let mut state = UiState::QuickSwitch {
            current: 1,
            current_app: "a".into(),
            prev: None,
            next: Some((2, "b".into())),
            offset,
            commit: Some((2, "b".into())),
            releasing: true,
            start_x: 0.0,
            origin: sc_input::Pt { x: 0.5, y: 1.0 },
        };
        assert!(state.needs_animation());
        for _ in 0..600 {
            transition(&mut state, UiEvent::Tick { dt: 1.0 / 60.0 });
        }
        assert!(matches!(state, UiState::App { toplevel: 2, .. }));
    }
}
