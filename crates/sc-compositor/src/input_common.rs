//! Backend-agnostic input handling.
//!
//! winit and libinput both decode their native events into these calls, so the
//! gesture behavior is identical across backends. Keys take a different route:
//! `keybinds` handles them for both backends via the seat keyboard on `State`.
//!
//! What a gesture *means* is not decided here. Every threshold and
//! classification lives in [`sc_input::home`] (Home screen and switcher deck)
//! and [`sc_input::nav`] (the in-app grab), which are pure and unit-tested. This
//! module measures the finger, asks those functions for a verdict, and applies
//! it to `State` — so the parts that are easy to get subtly wrong are testable
//! without a compositor.

use crate::input_dispatch::{self, DownAction};
use crate::switcher;
use crate::ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use crate::{DragItem, IconPress, State};
use sc_input::home;
use tracing::debug;

/// A finger held on an app icon, waiting to see if it becomes a tap (launch)
/// or a page swipe. Cleared once movement exceeds the icon tap slop.
#[derive(Clone, Debug)]
pub struct PendingLaunch {
    pub app_id: String,
    pub origin: ZoomOrigin,
    pub start_x: f32,
    pub start_y: f32,
}

/// A live horizontal page drag on the Home grid.
///
/// Holds the press origin plus a low-passed horizontal velocity, so a release
/// can commit on a flick as well as on distance — see
/// [`sc_input::home::page_after_swipe`]. Velocity is in fractions of output
/// width per second, positive rightward, smoothed the same way
/// [`sc_input::Tracker`] smooths the in-app gestures.
#[derive(Clone, Copy, Debug)]
pub struct PageDrag {
    pub start_x: f32,
    last_x: f32,
    last_t: std::time::Instant,
    velocity: f32,
}

impl PageDrag {
    pub fn begin(start_x: f32) -> Self {
        Self {
            start_x,
            last_x: start_x,
            last_t: std::time::Instant::now(),
            velocity: 0.0,
        }
    }

    /// Fold a motion event at output-x `x` into the velocity estimate.
    pub fn update(&mut self, x: f32, width: f32) {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_t).as_secs_f32();
        if dt > 0.0 && width > 0.0 {
            let inst = ((x - self.last_x) / width) / dt;
            let a = sc_input::thresholds::VELOCITY_SMOOTHING;
            // Decay first: motion events stop arriving while the finger is held
            // still, so a drag-then-hold must not release as a flick.
            self.velocity = decay(self.velocity, dt);
            self.velocity = a * inst + (1.0 - a) * self.velocity;
        }
        self.last_x = x;
        self.last_t = now;
    }

    /// Velocity as of now, decayed over the time since the last motion event.
    pub fn velocity(&self) -> f32 {
        decay(self.velocity, self.last_t.elapsed().as_secs_f32().min(1.0))
    }
}

/// Exponential velocity decay, ~halving every 35ms — same curve as
/// [`sc_input::Tracker::decay`].
fn decay(v: f32, dt: f32) -> f32 {
    v * (-dt / 0.05).exp()
}

impl State {
    /// Seconds since the previous motion event of this gesture, for feeding the
    /// gesture tracker's velocity estimate. Consumes the timestamp, so call it
    /// once per motion event.
    ///
    /// This must be measured, not assumed: input arrives at whatever rate the
    /// device (or the synthetic-input socket) delivers it, which is nowhere near
    /// the frame rate on a slow output. Dividing a real position jump by an
    /// assumed 1/90s inflates the reported velocity by the ratio between the two
    /// — enough for a deliberate 800ms drag to release as a flick.
    ///
    /// Floored at 1ms so two events sharing a timestamp can't divide by ~0. No
    /// ceiling: a long stall genuinely means slow, and a finger held still is
    /// handled by [`sc_input::Tracker::decay`] in the frame loop.
    pub(crate) fn motion_dt(&mut self) -> f32 {
        let now = std::time::Instant::now();
        let dt = self
            .last_motion
            .map_or(1.0 / 90.0, |t| now.duration_since(t).as_secs_f32());
        self.last_motion = Some(now);
        dt.max(0.001)
    }
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
///
/// An ordered chain, but of two different kinds of step — the return type says
/// which is which:
///
/// - **Claimants** (`-> Stage`) can own the movement outright: an arrange drag,
///   a switcher-card drag, or a pull-down that has crossed into a search. Once
///   one returns [`Stage::Done`] nothing below it runs.
/// - **Unconditional steps** (`-> ()`) never consume anything; they just run if
///   the chain reaches them. Cancelling a pending launch past the tap slop,
///   tracking the page drag, and feeding the live grab are all in this group.
///
/// The mix is deliberate rather than an accident of ordering. A press on empty
/// Home space arms *both* a pull-down search and a page drag, and the finger
/// decides between them as it moves: until the drag turns dominantly downward,
/// `motion_pull_down_search` falls through and `motion_page_drag` still pages.
/// Likewise a movement that cancels a pending launch has, by definition, become
/// some other gesture — so the stages below it must still get to interpret it.
pub fn on_motion(state: &mut State, x: f32, y: f32) {
    state.last_pointer_pos = Some((x, y));
    if !state.pointer_down {
        return;
    }

    if motion_arrange_drag(state, x, y) == Stage::Done {
        return;
    }
    if motion_switcher_card(state, x, y) == Stage::Done {
        return;
    }
    motion_cancel_icon_press(state, x, y);
    if motion_pull_down_search(state, x, y) == Stage::Done {
        return;
    }
    motion_page_drag(state, x);
    motion_live_gesture(state, x, y);
}

/// Arrange-mode drag: track the finger directly, no launch/swipe logic. The
/// hover slot is the grid index the icon would drop into, computed against the
/// page with the dragged app removed so it maps to the hole-removed order.
fn motion_arrange_drag(state: &mut State, x: f32, y: f32) -> Stage {
    let Some(app) = state
        .arrange
        .as_ref()
        .and_then(|a| a.drag.as_ref())
        .map(|d| d.app_id.clone())
    else {
        return Stage::Fallthrough;
    };
    let (w, h) = state.output_size_f();
    let page = state.current_home_page();
    let layout = sc_layout::compute(w, h, page, &state.model);
    let hover = if layout.dock_zone.contains(x, y) {
        None
    } else {
        let live_len = state
            .model
            .pages
            .get(page)
            .map_or(0, |p| p.iter().filter(|a| **a != app).count());
        let idx = sc_layout::nearest_grid_index(w, h, x, y).min(live_len);
        Some((page, idx))
    };
    if let Some(drag) = state.arrange.as_mut().and_then(|a| a.drag.as_mut()) {
        drag.cur = (x, y);
        drag.hover = hover;
    }
    Stage::Done
}

/// Switcher card drag: a dominant upward drag tracks that card's close
/// progress, anything else carousel-pans the deck. Falls through when the UI
/// has already left the switcher under us.
fn motion_switcher_card(state: &mut State, x: f32, y: f32) -> Stage {
    let SwitcherDrag::OnCard {
        start_x,
        start_y,
        start_scroll,
        toplevel,
    } = state.switcher_drag
    else {
        return Stage::Fallthrough;
    };
    let (w, h) = state.output_size_f();
    match home::classify_card_drag(x - start_x, y - start_y, w, h, start_scroll) {
        home::CardDrag::Close { progress } => {
            if let UiState::Switcher { close, .. } = &mut state.ui {
                *close = Some(crate::ui_state::CardClose::dragging(toplevel, progress));
                return Stage::Done;
            }
        }
        home::CardDrag::Scroll { position } => {
            if let UiState::Switcher { scroll, close, .. } = &mut state.ui {
                *close = None;
                // Track the finger directly — no spring physics mid-drag.
                scroll.value = position;
                scroll.target = position;
                scroll.velocity = 0.0;
                return Stage::Done;
            }
        }
    }
    Stage::Fallthrough
}

/// Cancel a pending launch (and its press highlight) once the finger travels
/// past the tap slop — the gesture is a swipe, not a tap. Never consumes the
/// movement: the swipe it just became still needs the stages below.
fn motion_cancel_icon_press(state: &mut State, x: f32, y: f32) {
    let Some(p) = &state.pending_launch else {
        return;
    };
    if home::exceeds_icon_tap_slop(x - p.start_x, y - p.start_y) {
        state.pending_launch = None;
        // The gesture became a swipe — cancel the long-press hold too.
        state.icon_press = None;
    }
}

/// Pull-down to open search: a dominant downward drag on empty Home space
/// launches the search app. A sideways drag falls through to the page drag
/// (still pages); an upward drag does nothing.
fn motion_pull_down_search(state: &mut State, x: f32, y: f32) -> Stage {
    if !matches!(state.ui, UiState::Home { .. }) {
        return Stage::Fallthrough;
    }
    let Some((sx, sy)) = state.search_arm else {
        return Stage::Fallthrough;
    };
    let (_, h) = state.output_size_f();
    if home::is_pull_down_search(x - sx, y - sy, h) {
        state.open_search();
        return Stage::Done;
    }
    Stage::Fallthrough
}

/// Page drag: drive the page spring straight off the finger (no spring physics
/// while dragging), rubber-banding past either edge.
fn motion_page_drag(state: &mut State, x: f32) {
    let w = state.output_size.0 as f32;
    let Some(drag) = &mut state.page_drag else {
        return;
    };
    drag.update(x, w);
    let dx = x - drag.start_x;
    if let UiState::Home {
        page,
        page_spring,
        page_count,
        ..
    } = &mut state.ui
    {
        // Track the finger directly — no spring physics mid-drag. Note this
        // leaves `target == value`, so the spring reports settled: whoever
        // abandons the drag must retarget it (`State::cancel_page_drag`).
        let value = home::page_drag_value(dx, w, *page, *page_count);
        page_spring.value = value;
        page_spring.target = value;
        page_spring.velocity = 0.0;
    }
}

/// Feed the movement to the live in-app gesture, and handle crossing between
/// the two gestures a bar drag can become.
fn motion_live_gesture(state: &mut State, x: f32, y: f32) {
    let dt = state.motion_dt();
    if let Some(ev) = input_dispatch::on_move(&state.ui, x, y, dt, state.output_size) {
        transition(&mut state.ui, ev);
    }

    // A grab that turns clearly horizontal — while still low on the screen —
    // becomes the live quick-switch slide: the current app slides sideways
    // revealing the adjacent app. Once the finger has risen past the reveal
    // point it is a vertical gesture (fan), so don't hijack it into a slide.
    if let UiState::Grabbing { tracker, .. } = &state.ui {
        if tracker.up_progress() < sc_input::thresholds::SWITCHER_REVEAL_PROGRESS
            && sc_input::live_state(tracker) == sc_input::NavState::QuickSwitching
        {
            let (w, _) = state.output_size_f();
            let start_x = x - tracker.dx() * w; // screen-x where the drag began
            let origin = tracker.start;
            enter_quick_switch(state, start_x, origin);
        }
    }
    // Within a quick-switch, pulling up past the reveal point lifts back into
    // the vertical grab gesture (fan / switcher / home) — you can start a
    // left/right slide and then curve upward into the switcher. Otherwise keep
    // tracking the horizontal slide.
    if let UiState::QuickSwitch { origin, .. } = &state.ui {
        let (_, h) = state.output_size_f();
        let up = (origin.y - y / h).max(0.0);
        if up > sc_input::thresholds::SWITCHER_REVEAL_PROGRESS {
            revert_quick_switch(state, x, y);
        } else {
            update_quick_switch(state, x);
        }
    }
}

/// Resolve a released quick-switch: commit to the revealed neighbour if the
/// slide passed the threshold (advancing the MRU cursor without reordering),
/// else reject and spring back to the current app. Sets the offset spring
/// animating; `Tick` finishes it and swaps to `App`.
fn settle_quick_switch(state: &mut State) {
    let f = match &state.ui {
        UiState::QuickSwitch { offset, .. } => offset.value,
        _ => return,
    };
    let (commit, target, dir) = match &state.ui {
        UiState::QuickSwitch { prev, next, .. } => {
            match home::classify_quick_switch_release(f, prev.is_some(), next.is_some()) {
                home::QuickSwitchRelease::Commit { dir, target } => {
                    // `dir` picks the slot the slide revealed: rightward (+1)
                    // committed the `prev` slot, leftward (-1) the `next` one.
                    let app = if dir > 0 { prev.clone() } else { next.clone() };
                    (app, target, dir)
                }
                home::QuickSwitchRelease::Reject => (None, 0.0, 0),
            }
        }
        _ => return,
    };

    if dir != 0 {
        // Move the cursor onto the committed neighbour — browse, no reorder.
        state.history.quick_switch(dir);
    }
    if let UiState::QuickSwitch {
        offset,
        commit: c,
        releasing,
        ..
    } = &mut state.ui
    {
        *c = commit;
        *releasing = true;
        offset.stiffness = 280.0;
        offset.damping = 32.0;
        offset.retarget(target);
    }
}

/// Commit a released horizontal page drag: snap to the neighbouring page once
/// the finger has travelled far enough *or* let go fast enough, else settle back
/// to the current one. No-op outside `Home`. Shared by the normal release and
/// the arrange-mode release, which differ only in what gates the call.
///
/// `vx` is the release velocity in fractions of width per second, positive
/// rightward. It also hands the finger's momentum to the page spring: the drag
/// pins `velocity` to 0 every frame while tracking, so without this the flip
/// starts from a standstill and reads as stiff no matter how hard it was flicked.
fn commit_page_swipe(state: &mut State, dx: f32, vx: f32) {
    let w = state.output_size.0 as f32;
    if let UiState::Home {
        page,
        page_spring,
        page_count,
        ..
    } = &mut state.ui
    {
        let target_page = home::page_after_swipe(dx, vx, w, *page, *page_count);
        *page = target_page;
        // Page value grows as the finger moves left, hence the sign flip.
        // Clamped so a spike in the estimate can't overshoot past a whole page.
        page_spring.velocity = (-vx).clamp(-4.0, 4.0);
        page_spring.retarget(target_page as f32);
    }
}

/// App id for a live toplevel, if it exists.
fn app_id_of(state: &State, tid: ToplevelId) -> Option<(ToplevelId, String)> {
    state
        .toplevels
        .get(tid)
        .and_then(|slot| slot.as_ref())
        .map(|tl| (tid, tl.app_id.clone()))
}

/// Convert the current `Grabbing` (or `App`) state into a live quick-switch,
/// capturing the adjacent apps from the MRU cursor. `start_x` is the screen-x at
/// which `offset` is zero; `origin` is the normalized grab start. No-op if
/// already switching.
fn enter_quick_switch(state: &mut State, start_x: f32, origin: sc_input::Pt) {
    let (current, current_app) = match &state.ui {
        UiState::Grabbing {
            toplevel, app_id, ..
        }
        | UiState::App { toplevel, app_id } => (*toplevel, app_id.clone()),
        _ => return,
    };
    // Handedness matches the carousel (most-recent on the right): a rightward
    // slide (`offset > 0`, the `prev` slot) reveals the older/next app; a
    // leftward slide (the `next` slot) reveals the more-recent/previous app.
    let prev = state.history.peek(1).and_then(|t| app_id_of(state, t));
    let next = state.history.peek(-1).and_then(|t| app_id_of(state, t));
    state.ui = UiState::QuickSwitch {
        current,
        current_app,
        prev,
        next,
        offset: sc_anim::Spring::new(0.0),
        commit: None,
        releasing: false,
        start_x,
        origin,
    };
}

/// Lift a live quick-switch back into the vertical grab gesture. Reconstructs a
/// `Tracker` from the original `origin` to the current finger point so
/// `up_progress` stays continuous into the fan/home logic; the horizontal offset
/// is dropped (the finger is now driving a vertical gesture). Re-seeds the MRU
/// fan deck, same as `on_press`.
fn revert_quick_switch(state: &mut State, x: f32, y: f32) {
    let (current, app_id, origin) = match &state.ui {
        UiState::QuickSwitch {
            current,
            current_app,
            origin,
            ..
        } => (*current, current_app.clone(), *origin),
        _ => return,
    };
    let (w, h) = state.output_size_f();
    // Anchor the horizontal origin at the current x so `dx` starts at zero: the
    // finger is now driving a vertical gesture, and any earlier sideways travel
    // must not make the release classify as a quick-switch. Keep the original
    // start.y so `up_progress` continues smoothly into the fan.
    let mut tracker = sc_input::Tracker::begin(sc_input::Pt {
        x: x / w,
        y: origin.y,
    });
    tracker.current = sc_input::Pt { x: x / w, y: y / h };
    let cards = state.history.deck_order();
    state.ui = UiState::Grabbing {
        toplevel: current,
        app_id,
        tracker,
        cards,
    };
}

/// Track the finger during a live quick-switch. Positive travel (finger right of
/// `start_x`) reveals the previous app; negative reveals the next. Rubber-bands
/// when there is no app in that direction (matches the home-page edge).
fn update_quick_switch(state: &mut State, x: f32) {
    let (w, _) = state.output_size_f();
    if let UiState::QuickSwitch {
        prev,
        next,
        offset,
        releasing,
        start_x,
        ..
    } = &mut state.ui
    {
        if *releasing {
            return; // finger already lifted; let the spring finish
        }
        // Track the finger directly — no spring physics mid-slide.
        let f = home::quick_switch_offset(x - *start_x, w, prev.is_some(), next.is_some());
        offset.value = f;
        offset.target = f;
        offset.velocity = 0.0;
    }
}

/// Touch-down / button-press at the last known position.
///
/// Arrange mode and the switcher deck are modal — each owns every press while it
/// is up — so they come first and claim the event outright. Anything else falls
/// through to the normal Home/app hit-testing in [`input_dispatch::on_press`],
/// which only *arms* things: what the press eventually meant is decided by the
/// motion and release chains.
pub fn on_press(state: &mut State) {
    let Some((x, y)) = state.last_pointer_pos else {
        return;
    };
    state.pointer_down = true;
    // Time the first motion from the press, not from whatever stale instant the
    // previous gesture left behind.
    state.last_motion = Some(std::time::Instant::now());

    if press_arrange(state, x, y) == Stage::Done {
        return;
    }
    if press_switcher(state, x, y) == Stage::Done {
        return;
    }
    press_arm_gesture(state, x, y);
}

/// Arrange-mode press: remove badges, the Done button, and picking up a new drag
/// all take priority over normal Home hit-testing.
fn press_arrange(state: &mut State, x: f32, y: f32) -> Stage {
    if state.arrange.is_none() {
        return Stage::Fallthrough;
    }
    let (w, h) = state.output_size_f();
    let page = state.current_home_page();
    let mut layout = sc_layout::compute(w, h, page, &state.model);
    // Match the render path: the Done button is shifted below a top
    // exclusive-zone bar, so the tap target must move with it.
    layout.shift_done_below(state.layers.usable(state.dpi).y);

    match sc_layout::hit_test_arrange(&layout, x, y) {
        sc_layout::Hit::RemoveBadge { app_id } => {
            state.model.hide(&app_id);
            state.after_arrange_edit();
        }
        sc_layout::Hit::DoneButton | sc_layout::Hit::Bar => {
            state.arrange = None;
        }
        sc_layout::Hit::Miss => {
            // Empty-area press in arrange: arm a page drag. A swipe pages
            // (resolved in on_release); a still tap exits.
            state.page_drag = Some(PageDrag::begin(x));
        }
        sc_layout::Hit::GridIcon { app_id, .. } => {
            if let Some(a) = &mut state.arrange {
                a.drag = Some(lift(app_id, input_dispatch::IconSource::Grid, (x, y)));
            }
        }
        sc_layout::Hit::DockIcon { app_id, .. } => {
            if let Some(a) = &mut state.arrange {
                a.drag = Some(lift(app_id, input_dispatch::IconSource::Dock, (x, y)));
            }
        }
    }
    Stage::Done
}

/// Pick an icon up under the finger, ready to be dragged to a new slot.
fn lift(app_id: String, source: input_dispatch::IconSource, at: (f32, f32)) -> DragItem {
    DragItem {
        app_id,
        source,
        cur: at,
        hover: None,
        edge_since: None,
    }
}

/// Switcher deck press: start either a card drag (scroll / swipe-to-close) or an
/// empty-area press that may become a dismiss tap.
fn press_switcher(state: &mut State, x: f32, y: f32) -> Stage {
    if !matches!(state.ui, UiState::Switcher { .. }) {
        return Stage::Fallthrough;
    }
    match switcher::hit_test(&state.switcher_cards, x, y, state.output_size_f()) {
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
    Stage::Done
}

/// Normal press: hit-test Home/the app and arm whatever the gesture might turn
/// into. Nothing is committed here — the release decides.
fn press_arm_gesture(state: &mut State, x: f32, y: f32) {
    match input_dispatch::on_press(&state.ui, x, y, &state.model, state.output_size) {
        DownAction::Event(ev) => {
            transition(&mut state.ui, ev);
            // Seed the live switcher-preview fan with the MRU deck (front =
            // current app). Same source as Effect::EnterSwitcher.
            if matches!(state.ui, UiState::Grabbing { ref cards, .. } if cards.is_empty()) {
                let deck = state.history.deck_order();
                if let UiState::Grabbing { cards, .. } = &mut state.ui {
                    *cards = deck;
                }
            }
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
            state.page_drag = Some(PageDrag::begin(start_x));
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
            state.page_drag = Some(PageDrag::begin(start_x));
            // Arm a pull-down: a dominant downward drag from here opens search
            // (a sideways drag still pages — resolved in `on_motion`).
            state.search_arm = Some((x, y));
        }
        DownAction::StartBarDrag { start_x, start_y } => {
            state.bar_drag_start = Some((start_x, start_y));
        }
        DownAction::None => {}
    }
}

/// Whether a gesture stage consumed the event. Shared by the press, motion, and
/// release chains, which are all ordered sequences of these.
#[derive(Clone, Copy, PartialEq, Eq)]
#[must_use]
enum Stage {
    /// The event is fully handled: later stages (and, for a release, the
    /// trailing page-count refresh) are skipped.
    Done,
    /// Not this stage's gesture (or this stage only did part of the work);
    /// keep going down the chain.
    Fallthrough,
}

/// Touch-up / button-release at the last known position.
///
/// A release is resolved by walking the stages below **in order**, which is
/// load-bearing in both directions:
///
/// - Ordering: an armed icon tap outranks the page swipe armed from the same
///   press (`on_press` arms both and lets the release pick); the bar-drag
///   classification must run before the page swipe consumes `page_drag`.
/// - Termination: a stage returning [`Stage::Done`] skips everything after it,
///   *including* the trailing `page_count` refresh — which is why the quick-switch,
///   arrange, icon-tap, and switcher-tap paths don't touch it, while the bar-drag,
///   page-swipe, and grab paths (which can land on Home) do.
///
/// As in [`on_motion`], only the stages returning [`Stage`] can claim the event;
/// the ones returning `()` (bar drag, page swipe, grab release) always run if
/// the chain reaches them and always fall through.
pub fn on_release(state: &mut State) {
    let Some((x, y)) = state.last_pointer_pos else {
        return;
    };
    state.pointer_down = false;
    state.last_motion = None;
    // A completed (or abandoned) gesture disarms the pull-down.
    state.search_arm = None;

    if release_quick_switch(state) == Stage::Done {
        return;
    }
    if release_arrange(state, x) == Stage::Done {
        return;
    }
    if release_icon_tap(state) == Stage::Done {
        return;
    }
    release_bar_drag(state, x, y);
    release_page_swipe(state, x);
    if release_switcher(state, x, y) == Stage::Done {
        return;
    }
    release_grab(state);

    // Update page_count after returning home.
    if let UiState::Home { page_count, .. } = &mut state.ui {
        *page_count = state.model.pages.len().max(1);
    }
}

/// Live quick-switch release: commit to the revealed neighbour past the
/// threshold, otherwise spring back (reject). Every later stage is irrelevant in
/// this state.
fn release_quick_switch(state: &mut State) -> Stage {
    if !matches!(state.ui, UiState::QuickSwitch { .. }) {
        return Stage::Fallthrough;
    }
    state.bar_drag_start = None;
    settle_quick_switch(state);
    Stage::Done
}

/// Arrange-mode release: resolve the drag (if any) to pin/unpin/snap-back, then
/// stay in arrange mode — only Done or an empty-area tap exits it.
fn release_arrange(state: &mut State, x: f32) -> Stage {
    if state.arrange.is_none() {
        return Stage::Fallthrough;
    }
    // Take the drag out (if any) without holding a &mut borrow across the body.
    if let Some(drag) = state.arrange.as_mut().and_then(|a| a.drag.take()) {
        resolve_arrange_drop(state, drag);
        return Stage::Done;
    }
    // No icon drag: empty-area release. A swipe commits a page flip and stays in
    // arrange; a still tap exits.
    match state.page_drag.take() {
        Some(drag) => {
            let dx = x - drag.start_x;
            let w = state.output_size.0 as f32;
            if home::is_arrange_page_swipe(dx, w) {
                commit_page_swipe(state, dx, drag.velocity());
            } else {
                state.arrange = None; // still tap -> exit
            }
        }
        None => state.arrange = None,
    }
    Stage::Done
}

/// Apply a dropped arrange-mode icon: pin to the dock, reorder within the grid,
/// or snap back untouched.
fn resolve_arrange_drop(state: &mut State, drag: DragItem) {
    let (w, h) = state.output_size_f();
    let page = state.current_home_page();
    let page_len = state.model.pages.get(page).map_or(0, |p| p.len());
    let layout = sc_layout::compute(w, h, page, &state.model);
    let action = input_dispatch::resolve_drop(
        drag.cur.0,
        drag.cur.1,
        &layout,
        drag.source,
        page,
        page_len,
        w,
        h,
    );
    // Logged so the VM test can assert the drop resolved the way the gesture
    // intended, separately from whether the model edit then landed.
    debug!(
        target: "springchick::debug",
        "arrange drop app_id={} action={:?}", drag.app_id, action
    );
    let edited = match action {
        input_dispatch::DropAction::Pin => state.model.pin(&drag.app_id),
        input_dispatch::DropAction::Reorder { page, index } => {
            // `page` is the current Home page (edge-dwell flips update it); use
            // it, not `drag.hover.page` which the flip does not refresh.
            // `hover.index` is computed against the working (hole-removed)
            // order, so prefer it to avoid a slot skew.
            let ix = drag.hover.map_or(index, |h| h.1);
            state.model.move_to(&drag.app_id, page, ix);
            true
        }
        input_dispatch::DropAction::SnapBack => false,
    };
    if edited {
        state.after_arrange_edit();
    } else {
        // No model edit, but a drag may have created a trailing empty page via
        // edge-dwell flip — drop it (no save needed). Also re-seed the dock so a
        // snapped-back dock icon (dropped from dock_anim during the lift)
        // springs back into place.
        state.model.repack();
        state.reflow_grid();
        state.reflow_dock();
    }
}

/// Icon tap: the pending launch survived (the finger never passed the tap slop),
/// so this was a tap, not a swipe. Launch and drop the page drag armed from the
/// same press.
fn release_icon_tap(state: &mut State) -> Stage {
    if let Some(p) = state.pending_launch.take() {
        // The page drag armed by the same press is abandoned, not committed —
        // it has to be settled, or the grid stays parked up to a tap-slop's
        // worth off-page behind the opening app. See `State::cancel_page_drag`.
        state.cancel_page_drag();
        state.icon_press = None;
        state.launch_or_raise(&p.app_id, p.origin);
        return Stage::Done;
    }
    // Not a tap — the long-press hold is over either way.
    state.icon_press = None;
    Stage::Fallthrough
}

/// Bar drag from Home: swipe up into the switcher deck, or swipe right onto the
/// top card of the stack.
///
/// Both are release-classified (the live, finger-tracked versions are the in-app
/// grab gestures — see [`enter_quick_switch`]) and both bounce Home instead of
/// doing nothing when there is no app to reach.
fn release_bar_drag(state: &mut State, x: f32, y: f32) {
    let Some((start_x, start_y)) = state.bar_drag_start.take() else {
        return;
    };
    let dx = x - start_x;
    let dy = start_y - y; // positive = swiped up
    let (w, h) = state.output_size_f();

    let verdict = home::classify_bar_release(dx, dy, w, h);
    if matches!(verdict, home::BarRelease::None) {
        return;
    }
    // The deck the gesture is reaching for: MRU order, minus any toplevel that
    // has since gone away.
    let cards: Vec<_> = state
        .history
        .deck_order()
        .into_iter()
        .filter(|tid| matches!(state.toplevels.get(*tid), Some(Some(_))))
        .collect();
    debug!(target: "springchick::debug", "bar release {:?} cards={:?}", verdict, cards);

    match (verdict, cards.first().copied()) {
        // Nothing running: rubber-band Home so the gesture is still acknowledged.
        (_, None) => {
            transition(&mut state.ui, UiEvent::HomeBounce);
        }
        (home::BarRelease::OpenSwitcher, Some(_)) => {
            transition(&mut state.ui, UiEvent::OpenSwitcherFromHome { cards });
        }
        (home::BarRelease::SlideToTop, Some(tid)) => {
            state.slide_toplevel_from_home(tid);
        }
        (home::BarRelease::None, _) => unreachable!("returned above"),
    }
}

/// Page swipe: snap based on distance travelled or release speed.
fn release_page_swipe(state: &mut State, x: f32) {
    if let Some(drag) = state.page_drag.take() {
        commit_page_swipe(state, x - drag.start_x, drag.velocity());
    }
}

/// Switcher release: commit or cancel a card close, open a tapped card, dismiss
/// on an empty tap, or settle the carousel scroll.
fn release_switcher(state: &mut State, x: f32, y: f32) -> Stage {
    if !matches!(state.ui, UiState::Switcher { .. }) {
        return Stage::Fallthrough;
    }
    let tapped = |sx: f32, sy: f32| home::is_switcher_tap(x - sx, y - sy);
    match std::mem::replace(&mut state.switcher_drag, SwitcherDrag::None) {
        SwitcherDrag::OnCard {
            start_x, start_y, ..
        } => {
            // If this drag was closing a card, commit past the threshold or snap
            // it back — either way the release is spent.
            let closing = match &state.ui {
                UiState::Switcher { close, .. } => *close,
                _ => None,
            };
            if let Some(c) = closing {
                resolve_card_close(state, c.toplevel, c.progress.value);
                return Stage::Done;
            }

            if tapped(start_x, start_y) {
                return open_tapped_card(state, x, y);
            }
            if let UiState::Switcher { cards, scroll, .. } = &mut state.ui {
                // Carousel scroll released: settle to the nearest card.
                let max = cards.len().saturating_sub(1) as f32;
                let target = scroll.value.round().clamp(0.0, max);
                scroll.retarget(target);
            }
        }
        SwitcherDrag::InEmpty { start_x, start_y } => {
            if tapped(start_x, start_y) {
                transition(&mut state.ui, UiEvent::SwitcherDismiss);
                return Stage::Done;
            }
        }
        SwitcherDrag::None => {}
    }
    Stage::Fallthrough
}

/// Finish a card-close drag: past the commit threshold remove the card from
/// the deck and close the client, otherwise mark it releasing so `Tick` springs
/// it back.
fn resolve_card_close(state: &mut State, toplevel: ToplevelId, progress: f32) {
    if home::card_close_commits(progress) {
        let eff = transition(&mut state.ui, UiEvent::SwitcherCloseCard { toplevel });
        if let crate::ui_state::Effect::CloseToplevel { toplevel } = eff {
            state.detach_toplevel(toplevel);
        }
    } else if let UiState::Switcher { close, .. } = &mut state.ui {
        let mut c = crate::ui_state::CardClose::dragging(toplevel, progress);
        c.progress.retarget(0.0);
        c.releasing = true;
        *close = Some(c);
    }
}

/// Open the switcher card under `(x, y)`, if the tap actually hit one.
/// A tap that misses every card falls through to the remaining release stages.
fn open_tapped_card(state: &mut State, x: f32, y: f32) -> Stage {
    let switcher::CardHit::Card(idx) =
        switcher::hit_test(&state.switcher_cards, x, y, state.output_size_f())
    else {
        return Stage::Fallthrough;
    };
    // `idx` indexes the z-sorted render array; resolve it to the card's toplevel
    // id so ordering can't desync.
    let Some(card) = state.switcher_cards.get(idx).copied() else {
        return Stage::Fallthrough;
    };
    let origin = ZoomOrigin::card((card.center_x, card.center_y), card.scale);
    // Resolve the real app_id from the toplevel (the deck tracks only ids);
    // otherwise the App state carries a fabricated `app_{id}` placeholder.
    let app_id = state
        .toplevels
        .get(card.toplevel)
        .and_then(|t| t.as_ref())
        .map(|t| t.app_id.clone())
        .unwrap_or_default();
    // Selecting a card makes it the most-recent app, so the next switcher shows
    // it as the front card.
    state.history.push_foreground(card.toplevel);
    transition(
        &mut state.ui,
        UiEvent::SwitcherTapCard {
            toplevel: card.toplevel,
            app_id,
            origin,
        },
    );
    Stage::Done
}

/// Release an in-app grab: classify the gesture and either quick-switch to the
/// adjacent app or start the settle animation toward Home / the switcher.
fn release_grab(state: &mut State) {
    let UiState::Grabbing {
        tracker,
        toplevel,
        app_id,
        ..
    } = &state.ui
    else {
        return;
    };
    let (target, cur_tid, cur_app) = (
        sc_input::classify_release(tracker),
        *toplevel,
        app_id.clone(),
    );
    debug!(target: "springchick::debug", "on_release grab target={:?}", target);

    match target {
        sc_input::NavTarget::QuickSwitch(dir) => {
            // Grab-based quick-switch: raise the adjacent app directly, browsing
            // without reordering — same rule as the bar swipe. With no adjacent
            // app, snap back to the current one.
            let adj = state
                .history
                .quick_switch(dir)
                .filter(|tid| matches!(state.toplevels.get(*tid), Some(Some(_))));
            let (toplevel, app_id) = match adj {
                Some(tid) => (tid, state.toplevels[tid].as_ref().unwrap().app_id.clone()),
                None => (cur_tid, cur_app),
            };
            transition(&mut state.ui, UiEvent::RaiseApp { toplevel, app_id });
        }
        _ => {
            let last_origin = state.last_origin;
            let size = state.output_size_f();
            transition(&mut state.ui, UiEvent::GrabRelease);
            // Settling toward Home zooms back to the launcher icon; toward the
            // switcher it settles into the front-card slot so the shrink flows
            // straight into the fan (no shrink-to-point then pop).
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
