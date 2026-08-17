//! Pure UI state machine for springchick.
//!
//! `UiState` + `UiEvent` → `UiState` transitions, unit-tested without Wayland/GPU.

use sc_anim::Spring;
use sc_input::{NavTarget, Tracker};
use tracing::debug;

/// Opaque toplevel identifier (index into the compositor's toplevel vec).
pub type ToplevelId = usize;

/// A switcher card being dragged along the close axis.
///
/// `progress` is signed: `>0` = lifted upward toward the close commit,
/// `<0` = pushed down below the resting stack (rubber-banded by the input
/// layer). While the finger is down the spring is pinned to the finger; on
/// release below the commit threshold it is retargeted to 0 and `Tick` runs it
/// until it settles.
#[derive(Clone, Copy, Debug)]
pub struct CardClose {
    pub toplevel: ToplevelId,
    pub progress: Spring,
    /// Finger let go below the commit threshold — springing back to rest.
    pub releasing: bool,
}

impl CardClose {
    /// Pinned to the finger: no physics while dragging.
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

    /// Hand the card back to physics: spring to rest, carrying the finger's
    /// speed over so the bounce continues the gesture instead of restarting it.
    /// `vy` is in screen heights/s, negative upward — the opposite sign to
    /// close progress.
    pub fn release(&mut self, vy: f32) {
        self.progress.velocity = -vy;
        self.progress.retarget(0.0);
        self.releasing = true;
    }
}

/// Springback for a cancelled close drag: deliberately under-damped
/// (critical ≈ 2·√320 ≈ 36) so overshooting past rest reads as a bounce —
/// most visible when the card was pushed *down* below the stack.
fn close_spring() -> Spring {
    let mut s = Spring::new(0.0);
    s.stiffness = 320.0;
    s.damping = 17.0;
    s
}

/// Velocity kick (fractions of screen height per second) given to the Home
/// bounce spring when a bar gesture has nowhere to go. Tuned against the
/// bounce spring below for a ~3% lift that settles in under a third of a second.
const HOME_BOUNCE_KICK: f32 = 1.1;

/// Settle progress at which a settle toward the switcher hands the deck over,
/// instead of waiting for the spring to reach its `is_settled` tolerance. The
/// remainder is sub-pixel on a phone panel, and the deck can't be stepped or
/// touched until it exists.
const SWITCHER_HANDOVER: f32 = 0.985;

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

/// How an app's open (and close) animation plays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpenMode {
    /// Zoom from the launch origin (icon / switcher card) to fullscreen.
    Zoom,
    /// Slide up full-size from below the bottom edge. Used by the pull-down
    /// search app, which has no icon origin.
    SlideUp,
    /// Slide in full-size from the left edge as Home is dragged off to the
    /// right. Used by the rightward Home-bar swipe: the stack sits to the left
    /// of Home, so pushing Home away uncovers its top card.
    SlideFromLeft,
}

/// The shell's UI states, including transition animations.
#[derive(Clone, Debug)]
pub enum UiState {
    Home {
        page: usize,
        page_spring: Spring,
        page_count: usize,
        /// Rubber-band lift of the whole Home screen, in fractions of screen
        /// height (positive = shifted up). Rests at 0; a bar gesture with
        /// nothing to go to kicks it (see [`UiEvent::HomeBounce`]).
        bounce: Spring,
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
        /// Which entrance animation to play.
        open_mode: OpenMode,
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
        /// MRU deck for the live switcher-preview fan (front = current app).
        /// Populated by the caller after the grab starts (the pure state machine
        /// has no history); empty until then, which just shows the single card.
        cards: Vec<ToplevelId>,
    },
    /// Released — spring-animating toward a target.
    Settling {
        toplevel: ToplevelId,
        app_id: String,
        target: NavTarget,
        /// Spring animating toward rest (0 = app fullscreen, 1 = target reached).
        progress: Spring,
        origin: ZoomOrigin,
        /// MRU deck carried from the grab (front = current app). For the
        /// `Switcher` target it keeps the neighbour fan on screen through the
        /// settle so the deck doesn't vanish and re-fan; empty otherwise.
        cards: Vec<ToplevelId>,
    },
    /// Live horizontal quick-switch: current app slides sideways following the
    /// finger, revealing the adjacent MRU app. Rubber-bands when there is no
    /// app in the swiped direction (end of the stack).
    QuickSwitch {
        current: ToplevelId,
        current_app: String,
        /// App revealed by a rightward swipe (`offset > 0`) — the older/next app
        /// (carousel handedness: most-recent on the right, so sliding right
        /// walks toward older apps).
        prev: Option<(ToplevelId, String)>,
        /// App revealed by a leftward swipe (`offset < 0`) — the more-recent
        /// (previous) app.
        next: Option<(ToplevelId, String)>,
        /// Horizontal offset as a fraction of screen width. `+` = current slides
        /// right (revealing `prev` from the left edge); `-` = slides left.
        offset: Spring,
        /// After release: `Some` = settling onto this app; `None` = rejected,
        /// springing back to `current`. Ignored until `releasing`.
        commit: Option<(ToplevelId, String)>,
        /// Finger let go — `Tick` drives `offset` to rest, then resolves.
        releasing: bool,
        /// Screen-x (px) where the slide began — the point at which `offset` is 0.
        start_x: f32,
        /// Normalized grab origin (the point the finger first went down on the
        /// bar). Kept so an upward drag can hand back to `Grabbing` with a
        /// continuous `up_progress` — see `input_common::revert_quick_switch`.
        origin: sc_input::Pt,
    },
    /// Switcher deck: fanned stack of running apps.
    Switcher {
        /// MRU card order; cards[0] = front (most recent).
        cards: Vec<ToplevelId>,
        /// Carousel focus spring (continuous card index).
        scroll: Spring,
        /// Card being dragged along the close axis. `None` at rest.
        close: Option<CardClose>,
        /// Entrance animation, 0 = deck still below the bottom edge, 1 = at
        /// rest. Entered from a grab release the deck is *already* in place
        /// (the settle animated it there), so that path starts settled at 1;
        /// only the Home-bar swipe-up plays the rise.
        ///
        /// Retargeted back to 0 to play the rise in reverse — that is how a
        /// Home shortcut leaves the deck (see [`UiEvent::ReturnHome`]), and a
        /// settled spring aimed at 0 is what tells `Tick` the exit is done.
        enter: Spring,
    },
}

/// The Home bounce spring at rest: stiff and deliberately under-damped, so a
/// velocity kick reads as a springy rebound rather than a slow drift back.
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

    /// Replace the stored `app_id` for `toplevel` wherever the current state
    /// references it. A client usually sets its xdg `app_id` *after* the
    /// toplevel maps (winit does), so the id captured at map time is a
    /// placeholder; this retags the live UI so switcher/zoom visuals resolve
    /// the real catalog icon.
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

    /// Get the foreground toplevel id if any app is visible/animating.
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

    /// Whether the state needs animation ticks (springs not settled).
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
            // Dragging (not releasing) is finger-driven and repaints on move;
            // once releasing, the spring must tick until it settles.
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

/// Events the UI state machine accepts.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// App launched and matched to a toplevel (with entrance animation).
    AppMapped {
        toplevel: ToplevelId,
        app_id: String,
        origin: ZoomOrigin,
        open_mode: OpenMode,
    },
    /// Raise an already-running app directly (no zoom animation).
    RaiseApp {
        toplevel: ToplevelId,
        app_id: String,
    },
    /// Return-home (Esc shortcut in dev).
    ReturnHome { origin: ZoomOrigin },
    /// Foreground app's toplevel was destroyed.
    /// A toplevel went away. `next` is the app to fall back to when the closed
    /// one was in the foreground — the caller resolves it from the MRU history
    /// (which it has already removed the closed id from), since this module has
    /// no view of what else is alive.
    ///
    /// It is `Some` only when a *dialog* was dismissed; an app closing passes
    /// `None` and goes Home, which is the Springboard model. See
    /// `State::close_toplevel`.
    ToplevelClosed {
        toplevel: ToplevelId,
        next: Option<(ToplevelId, String)>,
    },
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
    /// Enter switcher deck from grab release — already fanned open by the
    /// settle, so it is presented at rest with no entrance animation.
    EnterSwitcher { cards: Vec<ToplevelId> },
    /// Enter the switcher deck from Home (bar swipe-up): the deck rises into
    /// place from below the bottom edge.
    OpenSwitcherFromHome { cards: Vec<ToplevelId> },
    /// Enter the switcher deck from a running app without a gesture (the
    /// Super+Tab shortcut). The app shrinks into the front card slot exactly as
    /// a bar-grab release does — same `Settling` path, so the deck it lands in
    /// is the one `Effect::EnterSwitcher` builds.
    OpenSwitcherFromApp {
        cards: Vec<ToplevelId>,
        origin: ZoomOrigin,
    },
    /// Move the switcher's focused card by `delta` steps (negative = toward the
    /// more-recent end), wrapping at both ends. Springs the carousel there, so
    /// held-modifier stepping reads as one continuous pan.
    SwitcherStep { delta: i32 },
    /// A Home-bar gesture that had nowhere to go — rubber-band Home and stay.
    HomeBounce,
    /// Tap a card to open that app. `app_id` is the real id resolved from the
    /// toplevel by the caller — the switcher deck tracks only toplevel ids.
    SwitcherTapCard {
        toplevel: ToplevelId,
        app_id: String,
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
                        progress: Spring::zoom(1.0, 0.0),
                        origin,
                    };
                }
                // From the deck there is no window to shrink: play the deck's
                // own entrance backwards (cards sink, backdrop unblurs) and land
                // on Home when it settles.
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
                // A dismissed dialog hands the screen back to the app
                // underneath; an app close passes None and lands Home. A portal
                // file chooser is the case that makes this matter: it is a
                // toplevel of its own, in another process, so dismissing it
                // used to drop the app that asked for it.
                *state = match next {
                    Some((t, app_id)) => UiState::App {
                        toplevel: t,
                        app_id,
                    },
                    None => UiState::home(0, 1),
                };
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
                // Keep the deck through the settle only when landing in the
                // switcher; other targets have no fan.
                let cards = if matches!(target, NavTarget::Switcher) {
                    std::mem::take(cards)
                } else {
                    Vec::new()
                };
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
                    // The deck takes over a hair before the spring's asymptotic
                    // tail runs out: the last fraction of a percent is invisible
                    // motion, and holding it back only delays the first card
                    // step (Super+Tab) or the deck's first touch.
                    let handover = matches!(target, NavTarget::Switcher)
                        && progress.value >= SWITCHER_HANDOVER;
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
                                // Handled by caller raising the adjacent app.
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
                    // A settled entrance spring aimed at 0 means the deck has
                    // finished sinking off the bottom — the Home shortcut's exit.
                    if enter.target == 0.0 && enter.is_settled() {
                        *state = UiState::home(0, 1);
                        return Effect::None;
                    }
                    // Spring a cancelled close-drag back to rest (with bounce).
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
                    // Decay velocity so a stationary hold doesn't read as a flick.
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
                        // Land on the committed neighbour, or fall back to the
                        // app we started on (rejected swipe).
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
            // The settle already held the fan fully open (neighbours fanned
            // around the front card into their rest slots), so the deck is simply
            // presented at rest — there is no fan-in animation.
            *state = UiState::Switcher {
                cards,
                scroll: Spring::new(0.0),
                close: None,
                enter: Spring::new(1.0),
            };
            Effect::None
        }
        UiEvent::OpenSwitcherFromHome { cards } => {
            debug!(target: "springchick::debug", "OpenSwitcherFromHome cards={:?}", cards);
            // Only from Home — the grab path has its own (already-open) entry.
            if matches!(state, UiState::Home { .. }) && !cards.is_empty() {
                *state = UiState::Switcher {
                    cards,
                    scroll: Spring::new(0.0),
                    close: None,
                    enter: Spring::zoom(0.0, 1.0),
                };
            }
            Effect::None
        }
        UiEvent::OpenSwitcherFromApp { cards, origin } => {
            if let UiState::App { toplevel, app_id } = state {
                if !cards.is_empty() {
                    let toplevel = *toplevel;
                    let app_id = app_id.clone();
                    // Keyboard-only entry (Super+Tab): the first card step can't
                    // start until this settle hands over, so it runs much
                    // stiffer than a finger's settle — the shrink still reads,
                    // but the deck is there to step almost at once.
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
                    // Step from where the spring is *headed*, not where it is,
                    // so repeats while it is still flying each add one card.
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

    /// Dismissing a foreground toplevel returns to whatever the caller named as
    /// next, not Home. This is the portal file chooser case: the picker is its
    /// own toplevel in its own process, so closing it must hand the screen back
    /// to the app that asked for it.
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

    /// ...but with nothing left alive, Home is still the right answer.
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
        // The app shrinks into the front slot rather than cutting to the deck.
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

    /// The deck must be up fast enough that the first Tab step reads as
    /// immediate — the step can't be applied until the settle hands over.
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
        };
        transition(&mut state, UiEvent::SwitcherStep { delta: 1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 1.0);
        assert!(scroll.value < 1.0, "it springs there rather than jumping");

        // Repeats accumulate off the target, even mid-flight...
        transition(&mut state, UiEvent::SwitcherStep { delta: 1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 2.0);

        // ...and the deck is a ring: past the last card, back to the front.
        transition(&mut state, UiEvent::SwitcherStep { delta: 1 });
        let UiState::Switcher { scroll, .. } = &state else {
            panic!("left the switcher");
        };
        assert_eq!(scroll.target, 0.0);

        // Backwards from the front wraps to the last card.
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

        // And it settles in place.
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
        // Regression: a single running app used to leave the bar gesture inert.
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
        // A visible but modest lift: a few percent of screen height.
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
        };
        // Events carry the toplevel id, not a positional index — so the render
        // z-order and the MRU order can never desync (regression: tapping the
        // front card used to open the mirrored back card).
        let _eff = transition(
            &mut state,
            UiEvent::SwitcherTapCard {
                toplevel: 3,
                app_id: "org.foo.Bar".into(),
                origin: ZoomOrigin::card((600.0, 1350.0), 0.62),
            },
        );
        // The real app_id from the event must carry through — not a fabricated
        // `app_{toplevel}` placeholder.
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
                // Settled and cleared.
                None => break,
            }
        }
        // Under-damped: it overshoots rest (upward) before settling...
        assert!(peak > 0.005, "no bounce past rest: peak={peak}");
        // ...but the bounce stays small — nowhere near the close commit.
        assert!(peak < 0.4, "bounce too big: peak={peak}");
        assert!(
            matches!(&state, UiState::Switcher { close: None, .. }),
            "springback never settled"
        );
    }

    #[test]
    fn tap_unknown_toplevel_is_noop() {
        let mut state = UiState::Switcher {
            cards: vec![1, 2, 3],
            scroll: Spring::new(0.0),
            close: None,
            enter: Spring::new(1.0),
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
            enter: Spring::new(1.0),
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
            enter: Spring::new(1.0),
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
        };
        assert_eq!(state.foreground_toplevel(), Some(5));
    }
}
