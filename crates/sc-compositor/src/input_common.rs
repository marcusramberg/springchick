//! Backend-agnostic pointer/touch handling. Measures the finger, asks
//! [`sc_input::home`] / [`sc_input::nav`] for a verdict, and applies it.
//! Keys go through `keybinds`.

use crate::arrange::BgPress;
use crate::input_dispatch::{self, DownAction};
use crate::switcher;
use crate::ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};
use crate::{DragItem, IconPress, State};
use sc_input::{home, Pt, Tracker};
use tracing::debug;

/// Cleared once movement exceeds the tap slop.
#[derive(Clone, Debug)]
pub struct PendingLaunch {
    pub app_id: String,
    pub origin: ZoomOrigin,
    pub start_x: f32,
    pub start_y: f32,
}

/// A shell drag (page swipe, card close). Unlike the grab, these are only
/// touched on input, so they time themselves and decay lazily; otherwise
/// drag-hold-release reads as a flick. Normalized, y positive downward.
#[derive(Clone, Copy, Debug)]
pub struct FingerDrag {
    tracker: Tracker,
    last_t: std::time::Instant,
}

impl FingerDrag {
    pub fn begin(p: Pt) -> Self {
        Self {
            tracker: Tracker::begin(p),
            last_t: std::time::Instant::now(),
        }
    }

    pub fn update(&mut self, p: Pt) {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_t).as_secs_f32();
        self.tracker.decay(dt);
        self.tracker.update(p, dt);
        self.last_t = now;
    }

    pub fn start(&self) -> Pt {
        self.tracker.start
    }

    /// Decayed over the time since the last motion event.
    pub fn velocity(&self) -> Pt {
        let mut t = self.tracker;
        t.decay(self.last_t.elapsed().as_secs_f32().min(1.0));
        t.velocity
    }
}

fn norm(state: &State, x: f32, y: f32) -> Pt {
    let (w, h) = state.output_size_f();
    Pt { x: x / w, y: y / h }
}

impl State {
    /// Seconds since the previous motion event; consumes the timestamp. Must be
    /// measured: dividing by an assumed 1/90s turns a slow drag on a slow output
    /// into a flick. Floored at 1ms.
    pub(crate) fn motion_dt(&mut self) -> f32 {
        let now = std::time::Instant::now();
        let dt = self
            .last_motion
            .map_or(1.0 / 90.0, |t| now.duration_since(t).as_secs_f32());
        self.last_motion = Some(now);
        dt.max(0.001)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SwitcherDrag {
    /// A dominant vertical drag rides `toplevel` on the close axis; otherwise
    /// it scrolls. `vertical` tracks the y velocity for spotting a close flick.
    OnCard {
        start_x: f32,
        vertical: FingerDrag,
        start_scroll: f32,
        toplevel: ToplevelId,
    },
    InEmpty {
        start_x: f32,
        start_y: f32,
    },
    None,
}

/// Ordered chain. `-> Stage` steps can claim the movement; `-> ()` steps
/// always run if reached. An empty-Home press arms both pull-down and page
/// drag, and the pull-down falls through until the drag turns downward.
pub fn on_motion(state: &mut State, x: f32, y: f32) {
    state.last_pointer_pos = Some((x, y));
    if !state.pointer_down {
        return;
    }

    if motion_icon_menu(state, x, y) == Stage::Done {
        return;
    }
    if state.folder_motion(x, y) {
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
    motion_page_drag(state, x, y);
    motion_live_gesture(state, x, y);
}

/// Only claims once a row is pressed: the drift after the opening long press
/// must not pick a row. Sliding off a row disarms it.
fn motion_icon_menu(state: &mut State, x: f32, y: f32) -> Stage {
    let Some(menu) = &state.icon_menu else {
        return Stage::Fallthrough;
    };
    if menu.pressed.is_none() {
        return Stage::Done;
    }
    let (w, h) = state.output_size_f();
    let hit = sc_layout::menu::hit_test(&menu.layout(w, h), x, y);
    if let Some(m) = &mut state.icon_menu {
        if m.pressed != hit {
            m.pressed = hit;
            state.needs_render = true;
        }
    }
    Stage::Done
}

/// Hover is computed against the page with the dragged app removed.
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

/// Falls through if the UI already left the switcher.
fn motion_switcher_card(state: &mut State, x: f32, y: f32) -> Stage {
    let SwitcherDrag::OnCard {
        start_x,
        mut vertical,
        start_scroll,
        toplevel,
    } = state.switcher_drag
    else {
        return Stage::Fallthrough;
    };
    let (w, h) = state.output_size_f();
    // Feed the y velocity even while horizontal; the gesture can turn vertical.
    vertical.update(Pt { x: x / w, y: y / h });
    let start_y = vertical.start().y * h;
    state.switcher_drag = SwitcherDrag::OnCard {
        start_x,
        vertical,
        start_scroll,
        toplevel,
    };
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
                scroll.value = position;
                scroll.target = position;
                scroll.velocity = 0.0;
                return Stage::Done;
            }
        }
    }
    Stage::Fallthrough
}

/// Never consumes: the swipe it became still needs later stages.
fn motion_cancel_icon_press(state: &mut State, x: f32, y: f32) {
    if let Some(p) = &state.bg_press {
        if home::exceeds_icon_hold_slop(x - p.start.0, y - p.start.1) {
            state.bg_press = None;
        }
    }
    if let Some(p) = &state.icon_press {
        // Looser than the launch slop: a held finger drifts more than a tap.
        if home::exceeds_icon_hold_slop(x - p.start.0, y - p.start.1) {
            state.icon_press = None;
        }
    }
    if let Some((_, start)) = state.pending_folder {
        if home::exceeds_icon_tap_slop(x - start.0, y - start.1) {
            state.pending_folder = None;
        }
    }
    let Some(p) = &state.pending_launch else {
        return;
    };
    if home::exceeds_icon_tap_slop(x - p.start_x, y - p.start_y) {
        state.pending_launch = None;
    }
}

/// A sideways drag falls through to paging.
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

fn motion_page_drag(state: &mut State, x: f32, y: f32) {
    let (w, h) = state.output_size_f();
    let Some(drag) = &mut state.page_drag else {
        return;
    };
    drag.update(Pt { x: x / w, y: y / h });
    let dx = x - drag.start().x * w;
    if let UiState::Home {
        page,
        page_spring,
        page_count,
        ..
    } = &mut state.ui
    {
        // Leaves `target == value`, so the spring reads settled: whoever abandons
        // the drag must retarget it (`State::cancel_page_drag`).
        let value = home::page_drag_value(dx, w, *page, *page_count);
        page_spring.value = value;
        page_spring.target = value;
        page_spring.velocity = 0.0;
    }
}

fn motion_live_gesture(state: &mut State, x: f32, y: f32) {
    let dt = state.motion_dt();
    if let Some(ev) = input_dispatch::on_move(&state.ui, x, y, dt, state.output_size()) {
        transition(&mut state.ui, ev);
    }

    // A grab turning horizontal while still low becomes a quick-switch slide.
    // Past the reveal point it stays vertical.
    if let UiState::Grabbing { tracker, .. } = &state.ui {
        if tracker.up_progress() < sc_input::thresholds::SWITCHER_REVEAL_PROGRESS
            && sc_input::live_state(tracker) == sc_input::NavState::QuickSwitching
        {
            let (w, _) = state.output_size_f();
            let start_x = x - tracker.dx() * w;
            let origin = tracker.start;
            enter_quick_switch(state, start_x, origin);
        }
    }
    // Pulling up past the reveal point turns a quick-switch back into the grab.
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

/// Commit past the threshold (moving the MRU cursor, no reorder), else
/// spring back. `Tick` finishes it.
fn settle_quick_switch(state: &mut State) {
    let f = match &state.ui {
        UiState::QuickSwitch { offset, .. } => offset.value,
        _ => return,
    };
    let (commit, target, dir) = match &state.ui {
        UiState::QuickSwitch { prev, next, .. } => {
            match home::classify_quick_switch_release(f, prev.is_some(), next.is_some()) {
                home::QuickSwitchRelease::Commit { dir, target } => {
                    let app = if dir > 0 { prev.clone() } else { next.clone() };
                    (app, target, dir)
                }
                home::QuickSwitchRelease::Reject => (None, 0.0, 0),
            }
        }
        _ => return,
    };

    if dir != 0 {
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

/// Snap to the neighbour on distance or speed. `vx` (widths/s, positive
/// right) also seeds the page spring; the drag pins velocity to 0, so the
/// flip would otherwise start from standstill.
fn commit_page_swipe(state: &mut State, dx: f32, vx: f32) {
    let w = state.output_size().0 as f32;
    if let UiState::Home {
        page,
        page_spring,
        page_count,
        ..
    } = &mut state.ui
    {
        let target_page = home::page_after_swipe(dx, vx, w, *page, *page_count);
        *page = target_page;
        // Clamped so an estimate spike can't overshoot a page.
        page_spring.velocity = (-vx).clamp(-4.0, 4.0);
        page_spring.retarget(target_page as f32);
    }
}

fn app_id_of(state: &State, tid: ToplevelId) -> Option<(ToplevelId, String)> {
    state
        .toplevels
        .get(tid)
        .and_then(|slot| slot.as_ref())
        .map(|tl| (tid, tl.app_id.clone()))
}

/// `start_x` is where `offset` is zero; `origin` is the normalized grab
/// start. No-op if already switching.
fn enter_quick_switch(state: &mut State, start_x: f32, origin: Pt) {
    let (current, current_app) = match &state.ui {
        UiState::Grabbing {
            toplevel, app_id, ..
        }
        | UiState::App { toplevel, app_id } => (*toplevel, app_id.clone()),
        _ => return,
    };
    // Rightward (`prev`) reveals the older app, leftward the more recent one.
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

/// Rebuilds a `Tracker` from `origin` so `up_progress` stays continuous.
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
    // Anchor x at the finger so earlier sideways travel can't classify as a
    // quick-switch; keep start.y for continuity.
    let mut tracker = Tracker::begin(Pt {
        x: x / w,
        y: origin.y,
    });
    tracker.current = Pt { x: x / w, y: y / h };
    let cards = state.history.deck_order();
    state.ui = UiState::Grabbing {
        toplevel: current,
        app_id,
        tracker,
        cards,
    };
}

/// Rubber-bands when there is no app in that direction.
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
            return;
        }
        let f = home::quick_switch_offset(x - *start_x, w, prev.is_some(), next.is_some());
        offset.value = f;
        offset.target = f;
        offset.velocity = 0.0;
    }
}

/// Arrange and the switcher are modal and claim every press. Otherwise the
/// press only arms things; motion and release decide.
pub fn on_press(state: &mut State) {
    let Some((x, y)) = state.last_pointer_pos else {
        return;
    };
    state.pointer_down = true;
    // Time the first motion from the press, not a stale instant.
    state.last_motion = Some(std::time::Instant::now());

    if press_icon_menu(state, x, y) == Stage::Done {
        return;
    }
    if state.folder_press(x, y) {
        return;
    }
    state.library_press(x, y);
    if press_arrange(state, x, y) == Stage::Done {
        return;
    }
    if press_switcher(state, x, y) == Stage::Done {
        return;
    }
    press_arm_gesture(state, x, y);
}

/// While open the menu owns every press; outside dismisses without
/// falling through.
fn press_icon_menu(state: &mut State, x: f32, y: f32) -> Stage {
    let Some(menu) = &state.icon_menu else {
        return Stage::Fallthrough;
    };
    let (w, h) = state.output_size_f();
    let layout = menu.layout(w, h);
    match sc_layout::menu::hit_test(&layout, x, y) {
        Some(i) => {
            if let Some(m) = &mut state.icon_menu {
                m.pressed = Some(i);
            }
        }
        None if layout.panel.contains(x, y) => {
            if let Some(m) = &mut state.icon_menu {
                m.pressed = None;
            }
        }
        None => state.icon_menu = None,
    }
    state.needs_render = true;
    Stage::Done
}

fn press_arrange(state: &mut State, x: f32, y: f32) -> Stage {
    if state.arrange.is_none() {
        return Stage::Fallthrough;
    }
    let (w, h) = state.output_size_f();
    let page = state.current_home_page();
    let mut layout = sc_layout::compute(w, h, page, &state.model);
    // The Done button is drawn shifted below a top bar.
    layout.shift_done_below(state.layers.usable(state.dpi).y);

    match sc_layout::hit_test_arrange(&layout, x, y) {
        sc_layout::Hit::RemoveBadge { app_id } => {
            state.model.delete(&app_id);
            state.after_arrange_edit();
        }
        sc_layout::Hit::DoneButton | sc_layout::Hit::Bar => {
            state.arrange = None;
        }
        sc_layout::Hit::Miss => {
            // A swipe pages (in on_release); a still tap exits.
            state.page_drag = Some(FingerDrag::begin(norm(state, x, y)));
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

fn lift(app_id: String, source: input_dispatch::IconSource, at: (f32, f32)) -> DragItem {
    DragItem {
        app_id,
        source,
        cur: at,
        hover: None,
        edge_since: None,
    }
}

fn press_switcher(state: &mut State, x: f32, y: f32) -> Stage {
    if !matches!(state.ui, UiState::Switcher { .. }) {
        return Stage::Fallthrough;
    }
    let norm_p = norm(state, x, y);
    match switcher::hit_test(&state.switcher_cards, x, y, state.output_size_f()) {
        switcher::CardHit::Card(idx) => {
            let toplevel = state.switcher_cards.get(idx).map(|c| c.toplevel);
            if let (UiState::Switcher { scroll, .. }, Some(toplevel)) = (&state.ui, toplevel) {
                state.switcher_drag = SwitcherDrag::OnCard {
                    start_x: x,
                    vertical: FingerDrag::begin(norm_p),
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

/// Arms whatever the gesture might become; the release decides.
fn press_arm_gesture(state: &mut State, x: f32, y: f32) {
    match input_dispatch::on_press(&state.ui, x, y, &state.model, state.output_size()) {
        DownAction::Event(ev) => {
            transition(&mut state.ui, ev);
            // Seed the live fan with the MRU deck.
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
            // Arm a launch and a page drag from the same point; the release picks.
            state.pending_launch = Some(PendingLaunch {
                app_id: app_id.clone(),
                origin,
                start_x,
                start_y,
            });
            state.page_drag = Some(FingerDrag::begin(norm(state, start_x, start_y)));
            state.icon_press = Some(IconPress {
                app_id,
                source,
                start: (start_x, start_y),
                at: std::time::Instant::now(),
            });
        }
        DownAction::StartPageDrag { start_x } => {
            state.page_drag = Some(FingerDrag::begin(norm(state, start_x, y)));
            // A dominant downward drag opens search.
            state.search_arm = Some((x, y));
            // Not on the library page: nothing to rearrange there.
            if !state.on_library_page() {
                state.bg_press = Some(BgPress {
                    start: (x, y),
                    at: std::time::Instant::now(),
                });
            }
        }
        DownAction::StartBarDrag { start_x, start_y } => {
            state.bar_drag_start = Some((start_x, start_y));
            // Touching the bar brings a faded pill back.
            state.bar_hint.touched(std::time::Instant::now());
        }
        DownAction::None => {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[must_use]
enum Stage {
    /// Skips later stages, including the release's page-count refresh.
    Done,
    Fallthrough,
}

/// Stages run in order. An armed icon tap outranks the page swipe from the
/// same press, and the bar drag must classify before the page swipe takes
/// `page_drag`. `Stage::Done` also skips the trailing `page_count` refresh,
/// which only paths that can land on Home need.
pub fn on_release(state: &mut State) {
    let Some((x, y)) = state.last_pointer_pos else {
        return;
    };
    state.pointer_down = false;
    state.last_motion = None;
    state.search_arm = None;

    state.bg_press = None;

    if release_icon_menu(state) == Stage::Done {
        return;
    }
    if state.folder_release() {
        return;
    }
    if release_folder_tap(state) == Stage::Done {
        return;
    }
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

    let pages = state.home_page_count();
    if let UiState::Home { page_count, .. } = &mut state.ui {
        *page_count = pages;
    }
}

/// With no row armed the menu stays open: the opening long press ends in a
/// release over the icon.
fn release_icon_menu(state: &mut State) -> Stage {
    let Some(menu) = &state.icon_menu else {
        return Stage::Fallthrough;
    };
    let Some(action) = menu
        .pressed
        .and_then(|i| menu.items.get(i))
        .map(|i| i.action)
    else {
        return Stage::Done;
    };
    let Some(menu) = state.icon_menu.take() else {
        return Stage::Fallthrough;
    };
    state.run_menu_action(&menu, action);
    state.needs_render = true;
    Stage::Done
}

fn release_quick_switch(state: &mut State) -> Stage {
    if !matches!(state.ui, UiState::QuickSwitch { .. }) {
        return Stage::Fallthrough;
    }
    state.bar_drag_start = None;
    settle_quick_switch(state);
    Stage::Done
}

/// Stays in arrange; only Done or an empty-area tap exits.
fn release_arrange(state: &mut State, x: f32) -> Stage {
    if state.arrange.is_none() {
        return Stage::Fallthrough;
    }
    // The release of the engaging long press is not an exit tap.
    if let Some(a) = state.arrange.as_mut() {
        if a.just_engaged {
            a.just_engaged = false;
            state.page_drag = None;
            return Stage::Done;
        }
    }
    if let Some(drag) = state.arrange.as_mut().and_then(|a| a.drag.take()) {
        resolve_arrange_drop(state, drag);
        return Stage::Done;
    }
    match state.page_drag.take() {
        Some(drag) => {
            let w = state.output_size().0 as f32;
            let dx = x - drag.start().x * w;
            if home::is_arrange_page_swipe(dx, w) {
                commit_page_swipe(state, dx, drag.velocity().x);
            } else {
                state.arrange = None; // still tap -> exit
            }
        }
        None => state.arrange = None,
    }
    Stage::Done
}

fn resolve_arrange_drop(state: &mut State, drag: DragItem) {
    let (w, h) = state.output_size_f();
    let page = state.current_home_page();
    let page_len = state.model.pages.get(page).map_or(0, |p| p.len());
    let layout = sc_layout::compute(w, h, page, &state.model);
    // The library page is where unplaced apps live, so dropping there removes
    // it. The dock, drawn over it, still wins.
    if page == state.library_page() && !layout.dock_zone.contains(drag.cur.0, drag.cur.1) {
        debug!(
            target: "springchick::debug",
            "arrange drop app_id={} action=RemoveToLibrary", drag.app_id
        );
        if drag.source != input_dispatch::IconSource::Library {
            state.model.delete(&drag.app_id);
            state.after_arrange_edit();
        } else {
            state.model.repack();
            state.reflow_grid();
            state.reflow_dock();
        }
        return;
    }
    let action =
        input_dispatch::resolve_drop(drag.cur, &layout, drag.source, page, page_len, (w, h));
    // The VM test asserts this.
    debug!(
        target: "springchick::debug",
        "arrange drop app_id={} action={:?}", drag.app_id, action
    );
    let edited = match action {
        input_dispatch::DropAction::Pin => state.model.pin(&drag.app_id),
        input_dispatch::DropAction::Reorder { page, index } => {
            // `page`, not `drag.hover.page`: edge-dwell flips don't refresh the hover.
            // `hover.index` is against the hole-removed order.
            let ix = drag.hover.map_or(index, |h| h.1);
            state.model.move_to(&drag.app_id, page, ix);
            true
        }
        input_dispatch::DropAction::SnapBack => false,
    };
    if edited {
        state.after_arrange_edit();
    } else {
        // Drop any trailing page an edge-dwell flip added, and re-seed the dock so
        // a snapped-back dock icon returns.
        state.model.repack();
        state.reflow_grid();
        state.reflow_dock();
    }
}

/// Armed by `State::library_press`; the slop check drops it so a swipe
/// starting on a tile pages instead.
fn release_folder_tap(state: &mut State) -> Stage {
    let Some((index, _)) = state.pending_folder.take() else {
        return Stage::Fallthrough;
    };
    state.cancel_page_drag();
    // The VM test greps this; Home stays Home, so no state-change line fires.
    debug!(
        target: "springchick::debug",
        "folder opened index={index} name={} members={}",
        state.folders.get(index).map_or("?", |f| f.name),
        state.folders.get(index).map_or(0, |f| f.apps.len()),
    );
    state.open_folder(index);
    state.needs_render = true;
    Stage::Done
}

/// The pending launch survived the tap slop, so this was a tap: launch.
fn release_icon_tap(state: &mut State) -> Stage {
    if let Some(p) = state.pending_launch.take() {
        // Settle the page drag armed by the same press, or the grid stays parked
        // off-page behind the opening app.
        state.cancel_page_drag();
        state.icon_press = None;
        state.launch_or_raise(&p.app_id, p.origin);
        return Stage::Done;
    }
    state.icon_press = None;
    Stage::Fallthrough
}

/// Release-classified: up opens the switcher, right slides onto the top
/// card. Bounces Home when there is no app.
fn release_bar_drag(state: &mut State, x: f32, y: f32) {
    let Some((start_x, start_y)) = state.bar_drag_start.take() else {
        return;
    };
    let dx = x - start_x;
    let dy = start_y - y; // positive = up
    let (w, h) = state.output_size_f();

    let verdict = home::classify_bar_release(dx, dy, w, h);
    if matches!(verdict, home::BarRelease::None) {
        return;
    }
    let cards: Vec<_> = state
        .history
        .deck_order()
        .into_iter()
        .filter(|tid| matches!(state.toplevels.get(*tid), Some(Some(_))))
        .collect();
    debug!(target: "springchick::debug", "bar release {:?} cards={:?}", verdict, cards);

    match (verdict, cards.first().copied()) {
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

fn release_page_swipe(state: &mut State, x: f32) {
    if let Some(drag) = state.page_drag.take() {
        let w = state.output_size().0 as f32;
        commit_page_swipe(state, x - drag.start().x * w, drag.velocity().x);
    }
}

fn release_switcher(state: &mut State, x: f32, y: f32) -> Stage {
    if !matches!(state.ui, UiState::Switcher { .. }) {
        return Stage::Fallthrough;
    }
    let tapped = |sx: f32, sy: f32| home::is_switcher_tap(x - sx, y - sy);
    match std::mem::replace(&mut state.switcher_drag, SwitcherDrag::None) {
        SwitcherDrag::OnCard {
            start_x, vertical, ..
        } => {
            let closing = match &state.ui {
                UiState::Switcher { close, .. } => *close,
                _ => None,
            };
            if let Some(c) = closing {
                resolve_card_close(state, c, vertical.velocity().y);
                return Stage::Done;
            }

            let (_, h) = state.output_size_f();
            if tapped(start_x, vertical.start().y * h) {
                return open_tapped_card(state, x, y);
            }
            if let UiState::Switcher { cards, scroll, .. } = &mut state.ui {
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

/// `vy` in screen heights/s, negative upward.
fn resolve_card_close(state: &mut State, mut closing: crate::ui_state::CardClose, vy: f32) {
    if home::card_close_commits(closing.progress.value, vy) {
        let toplevel = closing.toplevel;
        let eff = transition(&mut state.ui, UiEvent::SwitcherCloseCard { toplevel });
        if let crate::ui_state::Effect::CloseToplevel { toplevel } = eff {
            state.detach_toplevel(toplevel);
        }
    } else if let UiState::Switcher { close, .. } = &mut state.ui {
        closing.release(vy);
        *close = Some(closing);
    }
}

fn open_tapped_card(state: &mut State, x: f32, y: f32) -> Stage {
    let switcher::CardHit::Card(idx) =
        switcher::hit_test(&state.switcher_cards, x, y, state.output_size_f())
    else {
        return Stage::Fallthrough;
    };
    // `idx` is into the z-sorted array; resolve the toplevel so order can't
    // desync.
    let Some(card) = state.switcher_cards.get(idx).copied() else {
        return Stage::Fallthrough;
    };
    let origin = ZoomOrigin::card((card.center_x, card.center_y), card.scale);
    let app_id = state
        .toplevels
        .get(card.toplevel)
        .and_then(|t| t.as_ref())
        .map(|t| t.app_id.clone())
        .unwrap_or_default();
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
            // Browse without reordering; snap back with no neighbour.
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
            let home_origin = state.home_origin();
            let size = state.output_size_f();
            transition(&mut state.ui, UiEvent::GrabRelease);
            // Toward the switcher, settle into the front slot so the shrink flows into
            // the fan.
            if let UiState::Settling { origin, target, .. } = &mut state.ui {
                *origin = if matches!(target, sc_input::NavTarget::Switcher) {
                    let (cx, cy, s) = switcher::front_slot(size);
                    ZoomOrigin::card((cx, cy), s)
                } else {
                    home_origin
                };
            }
        }
    }
}
