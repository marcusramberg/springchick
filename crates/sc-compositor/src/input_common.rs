//! Backend-agnostic input handling.
//!
//! winit and libinput both decode their native events into these calls, so the
//! gesture behavior is identical across backends. Keys take a different route:
//! `keybinds` handles them for both backends via the seat keyboard on `State`.

use crate::input_dispatch::{self, DownAction};
use crate::switcher;
use crate::ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use crate::{DragItem, IconPress, State};
use tracing::debug;

/// Upward travel (fraction of screen height) that drives close_progress from 0
/// to 1. A release past `CLOSE_COMMIT_PROGRESS` closes the card.
const CLOSE_FULL_RISE: f32 = 0.25;
const CLOSE_COMMIT_PROGRESS: f32 = 0.4;
/// Finger travel to advance the carousel one card, as a fraction of output
/// width (≈ front card width * slide distance).
const FRONT_SCALE_PX_FRAC: f32 = 0.42;
/// Pixels a finger may travel from an icon press and still count as a tap
/// (launch). Past this the press is cancelled and the gesture becomes a page
/// swipe.
const ICON_TAP_SLOP: f32 = 12.0;
/// Fraction of screen width the quick-switch slide must reach at release to
/// commit to the neighbour app; short of it the swipe is rejected (springs back).
/// Kept low so a short, deliberate swipe commits instead of bouncing.
const BAR_QS_COMMIT_FRAC: f32 = 0.2;

/// A finger held on an app icon, waiting to see if it becomes a tap (launch)
/// or a page swipe. Cleared once movement exceeds `ICON_TAP_SLOP`.
#[derive(Clone, Debug)]
pub struct PendingLaunch {
    pub app_id: String,
    pub origin: ZoomOrigin,
    pub start_x: f32,
    pub start_y: f32,
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
    let dx = x - start_x;
    let dy = y - start_y;
    let h = state.output_size.1 as f32;

    if dy < 0.0 && dy.abs() > dx.abs() {
        let progress = ((-dy) / (h * CLOSE_FULL_RISE)).clamp(0.0, 1.0);
        if let UiState::Switcher { close, .. } = &mut state.ui {
            *close = Some((toplevel, progress, false));
            return Stage::Done;
        }
    } else if let UiState::Switcher { scroll, close, .. } = &mut state.ui {
        // Dragging right advances the focus (active card slides off right, next
        // comes forward). One card advances per ~front-card width of travel.
        *close = None;
        let per_index = state.output_size.0 as f32 * FRONT_SCALE_PX_FRAC;
        scroll.value = start_scroll + dx / per_index;
        scroll.target = scroll.value;
        scroll.velocity = 0.0;
        return Stage::Done;
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
    let dx = x - p.start_x;
    let dy = y - p.start_y;
    if (dx * dx + dy * dy).sqrt() > ICON_TAP_SLOP {
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
    let dy = y - sy;
    let dx = (x - sx).abs();
    if dy > h * 0.08 && dy > dx {
        state.open_search();
        return Stage::Done;
    }
    Stage::Fallthrough
}

/// Page drag: drive the page spring straight off the finger (no spring physics
/// while dragging), rubber-banding past either edge.
fn motion_page_drag(state: &mut State, x: f32) {
    let Some(start_x) = state.page_drag_start else {
        return;
    };
    let dx = x - start_x;
    let w = state.output_size.0 as f32;
    if let UiState::Home {
        page,
        page_spring,
        page_count,
        ..
    } = &mut state.ui
    {
        let raw_target = *page as f32 - dx / w;
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

/// Feed the movement to the live in-app gesture, and handle crossing between
/// the two gestures a bar drag can become.
fn motion_live_gesture(state: &mut State, x: f32, y: f32) {
    let dt = 1.0 / 90.0;
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
    // The `prev` slot (rightward slide, offset > 0) now holds the older/next
    // app, so committing it walks the cursor forward (+1); the `next` slot
    // (leftward slide) holds the more-recent/previous app (-1). `target` follows
    // the slide direction, not the app.
    let (commit, target, dir) = match &state.ui {
        UiState::QuickSwitch { prev, next, .. } => {
            if f >= BAR_QS_COMMIT_FRAC && prev.is_some() {
                (prev.clone(), 1.0f32, 1)
            } else if f <= -BAR_QS_COMMIT_FRAC && next.is_some() {
                (next.clone(), -1.0f32, -1)
            } else {
                (None, 0.0, 0)
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
/// the finger has travelled past 30% of the output width, else settle back to
/// the current one. No-op outside `Home`. Shared by the normal release and the
/// arrange-mode release, which differ only in what gates the call.
fn commit_page_swipe(state: &mut State, dx: f32) {
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
        let mut f = (x - *start_x) / w;
        let at_end = (f > 0.0 && prev.is_none()) || (f < 0.0 && next.is_none());
        if at_end {
            f *= 0.3;
        }
        f = f.clamp(-1.0, 1.0);
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
            state.page_drag_start = Some(x);
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
            state.page_drag_start = Some(start_x);
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
            state.page_drag_start = Some(start_x);
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

/// Finger travel (output px) a switcher press may drift and still count as a tap
/// rather than a scroll.
const SWITCHER_TAP_SLOP: f32 = 15.0;

/// Touch-up / button-release at the last known position.
///
/// A release is resolved by walking the stages below **in order**, which is
/// load-bearing in both directions:
///
/// - Ordering: an armed icon tap outranks the page swipe armed from the same
///   press (`on_press` arms both and lets the release pick); the bar-drag
///   classification must run before the page swipe consumes `page_drag_start`.
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
    match state.page_drag_start.take() {
        Some(start_x) => {
            let dx = x - start_x;
            let w = state.output_size.0 as f32;
            if dx.abs() > w * 0.15 {
                commit_page_swipe(state, dx);
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

/// Bar drag from Home: classify the swipe direction and raise an app.
///
/// From the Home bar there is no foreground app to slide, so the horizontal
/// switch stays a release-classified instant raise. (The live slide is the
/// in-app grab gesture — see [`enter_quick_switch`].)
fn release_bar_drag(state: &mut State, x: f32, y: f32) {
    let Some((start_x, start_y)) = state.bar_drag_start.take() else {
        return;
    };
    let dx = x - start_x;
    let dy = start_y - y; // positive = swiped up
    let (w, h) = state.output_size_f();

    if dy > h * 0.08 {
        // Swiped up from bar → raise most recent app (deliberate jump).
        if let Some(tid) = state.history.previous() {
            state.raise_toplevel_centered(tid, true);
        }
    } else if dx.abs() > w * 0.15 {
        let dir = if dx < 0.0 { 1 } else { -1 };
        if let Some(tid) = state.history.quick_switch(dir) {
            state.raise_toplevel_centered(tid, false);
        }
    }
}

/// Page swipe: snap based on the 30% threshold.
fn release_page_swipe(state: &mut State, x: f32) {
    if let Some(start_x) = state.page_drag_start.take() {
        commit_page_swipe(state, x - start_x);
    }
}

/// Switcher release: commit or cancel a card close, open a tapped card, dismiss
/// on an empty tap, or settle the carousel scroll.
fn release_switcher(state: &mut State, x: f32, y: f32) -> Stage {
    if !matches!(state.ui, UiState::Switcher { .. }) {
        return Stage::Fallthrough;
    }
    let tapped =
        |sx: f32, sy: f32| (x - sx).abs() < SWITCHER_TAP_SLOP && (y - sy).abs() < SWITCHER_TAP_SLOP;
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
            if let Some((ct, progress, _)) = closing {
                resolve_card_close(state, ct, progress);
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

/// Finish a card-close drag: past [`CLOSE_COMMIT_PROGRESS`] remove the card from
/// the deck and close the client, otherwise mark it releasing so `Tick` springs
/// it back.
fn resolve_card_close(state: &mut State, toplevel: ToplevelId, progress: f32) {
    if progress >= CLOSE_COMMIT_PROGRESS {
        let eff = transition(&mut state.ui, UiEvent::SwitcherCloseCard { toplevel });
        if let crate::ui_state::Effect::CloseToplevel { toplevel } = eff {
            state.detach_toplevel(toplevel);
        }
    } else if let UiState::Switcher { close, .. } = &mut state.ui {
        *close = Some((toplevel, progress, true));
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
