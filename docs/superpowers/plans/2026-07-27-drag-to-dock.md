# Drag-to-dock + Arrange Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A touch arrange mode (entered by long-press) to pin apps to the dock, unpin them, and hide apps from the home grid.

**Architecture:** Arrange/edit state lives on the compositor `State` (like `pending_launch`/`page_drag_start`), leaving `UiState` as `Home`. The model gains a persisted `hidden` set; `recompute_pages` filters dock ∪ hidden, so pin/hide drop an app from the grid automatically. Layout gains a remove-badge per icon plus a dock drop-zone and Done button, hit-tested by a new `hit_test_arrange`. A long-press timer in `advance_frame` lifts an icon into a drag; drop resolution (`resolve_drop`) pins/unpins/snaps-back. Render draws badges, Done, and the lifted icon when arrange is active.

**Tech Stack:** Rust, serde/toml, skia (GL), existing `sc-shell-model` / `sc-layout` / `sc-compositor` crates.

**Spec:** `docs/superpowers/specs/2026-07-27-drag-to-dock-design.md`

**Env:** bare `cargo` fails (no internet). Use `nix develop --command cargo …`. For manual verification use the `run-springchick` skill (`driver.sh build` first, then `up`).

---

## File Structure

- `crates/sc-shell-model/src/lib.rs` — `hidden` field; `pin`/`unpin`/`hide`/`unhide`; `recompute_pages` filters hidden.
- `crates/sc-config/src/state.rs` — round-trip test for `hidden`.
- `crates/sc-layout/src/lib.rs` — `IconSlot.badge_rect`; `Layout.dock_zone`/`done_button`; `Hit::RemoveBadge`/`DoneButton`; `hit_test_arrange`.
- `crates/sc-compositor/src/input_dispatch.rs` — `IconSource` enum; `DownAction::PressIcon.source`; pure `resolve_drop`.
- `crates/sc-compositor/src/main.rs` + `input_common.rs` — `icon_press`/`arrange` state; hold-timer in `advance_frame`; press/move/release wiring.
- `crates/sc-compositor/src/skia_gl.rs` + `render.rs` — arrange-mode drawing.

---

## Task 1: Model — hidden set + pin/unpin/hide

**Files:** Modify + Test: `crates/sc-shell-model/src/lib.rs`

- [ ] **Step 1: Write failing tests** (add to `tests` module):

```rust
#[test]
fn pin_adds_to_dock_under_cap() {
    let mut m = ShellModel::default();
    assert!(m.pin("a"));
    assert_eq!(m.dock, vec!["a"]);
}

#[test]
fn pin_fails_when_full_or_duplicate() {
    let mut m = ShellModel::default();
    for i in 0..DOCK_CAP { assert!(m.pin(&format!("d{i}"))); }
    assert!(!m.pin("overflow"));           // full
    assert_eq!(m.dock.len(), DOCK_CAP);
    let mut m2 = ShellModel::default();
    assert!(m2.pin("a"));
    assert!(!m2.pin("a"));                  // duplicate
    assert_eq!(m2.dock, vec!["a"]);
}

#[test]
fn unpin_removes_from_dock() {
    let mut m = ShellModel::default();
    m.pin("a");
    m.unpin("a");
    assert!(m.dock.is_empty());
}

#[test]
fn hide_unhide_toggle_hidden_set() {
    let mut m = ShellModel::default();
    m.hide("a");
    assert_eq!(m.hidden, vec!["a"]);
    m.hide("a");                            // idempotent
    assert_eq!(m.hidden, vec!["a"]);
    m.unhide("a");
    assert!(m.hidden.is_empty());
}

#[test]
fn recompute_pages_excludes_hidden_and_dock() {
    let mut m = ShellModel::default();
    m.pin("docked");
    m.hide("gone");
    let catalog = ["docked", "gone", "shown"].map(String::from).to_vec();
    m.recompute_pages(&catalog, 0);
    let flat: Vec<&String> = m.pages.iter().flatten().collect();
    assert_eq!(flat, vec!["shown"]);
}

#[test]
fn hidden_serialized_with_default() {
    let mut m = ShellModel::default();
    m.hide("a");
    let s = toml::to_string_pretty(&m).unwrap();
    let back: ShellModel = toml::from_str(&s).unwrap();
    assert_eq!(back.hidden, vec!["a"]);
    // legacy file without `hidden` still loads:
    let legacy: ShellModel = toml::from_str("dock = []\n").unwrap();
    assert!(legacy.hidden.is_empty());
}
```

- [ ] **Step 2:** `nix develop --command cargo test -p sc-shell-model` → FAIL (methods/field missing).

- [ ] **Step 3: Implement.** Add field to `ShellModel` (keep `pages` skip, `frecency` default):

```rust
    #[serde(default)]
    pub hidden: Vec<AppId>,
```

Add methods:

```rust
impl ShellModel {
    /// Pin `app` to the dock. Returns false if already docked or dock is full.
    pub fn pin(&mut self, app: &str) -> bool {
        if self.dock.iter().any(|a| a == app) || self.dock.len() >= DOCK_CAP {
            return false;
        }
        self.dock.push(app.to_owned());
        true
    }
    pub fn unpin(&mut self, app: &str) {
        self.dock.retain(|a| a != app);
    }
    pub fn hide(&mut self, app: &str) {
        if !self.hidden.iter().any(|a| a == app) {
            self.hidden.push(app.to_owned());
        }
    }
    pub fn unhide(&mut self, app: &str) {
        self.hidden.retain(|a| a != app);
    }
}
```

Update `recompute_pages`'s filter from `!self.dock.contains(id)` to also exclude hidden:

```rust
        .filter(|id| !self.dock.contains(id) && !self.hidden.contains(id))
```

- [ ] **Step 4:** `nix develop --command cargo test -p sc-shell-model` → PASS.

- [ ] **Step 5: Commit** — `git add crates/sc-shell-model/ && git commit -m "feat(shell-model): hidden set + pin/unpin/hide/unhide"`

---

## Task 2: Persistence — hidden round-trip

**Files:** Test: `crates/sc-config/src/state.rs`

- [ ] **Step 1: Add test** to the `tests` module:

```rust
#[test]
fn round_trips_hidden_and_dock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let mut m = ShellModel::default();
    m.pin("org.gnome.Console");
    m.hide("org.gnome.Maps");
    save(&m, &path).unwrap();
    let back = load(&path).unwrap();
    assert_eq!(m.dock, back.dock);
    assert_eq!(m.hidden, back.hidden);
}
```

- [ ] **Step 2:** `nix develop --command cargo test -p sc-config` → the new test passes (no production change expected; struct change flows through `toml`). If it fails, STOP and report.

- [ ] **Step 3: Commit** — `git add crates/sc-config/ && git commit -m "test(config): hidden round-trip"`

---

## Task 3: Layout — badge, dock zone, done button, arrange hit-test

**Files:** Modify + Test: `crates/sc-layout/src/lib.rs`

- [ ] **Step 1: Write failing tests:**

```rust
#[test]
fn badge_rect_at_icon_top_left() {
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    let s = &l.grid[0];
    // badge sits at the icon's top-left corner, within the icon bounds
    assert!((s.badge_rect.center_x() - s.icon_rect.x).abs() < s.icon_rect.w);
    assert!((s.badge_rect.center_y() - s.icon_rect.y).abs() < s.icon_rect.h);
    assert!(s.badge_rect.w > 0.0 && s.badge_rect.h > 0.0);
}

#[test]
fn dock_zone_spans_dock_band() {
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    // a dock icon's center is inside the dock zone
    let d = &l.dock[0];
    assert!(l.dock_zone.contains(d.icon_rect.center_x(), d.icon_rect.center_y()));
    // a top-of-screen grid point is not
    assert!(!l.dock_zone.contains(612.0, 100.0));
}

#[test]
fn done_button_nonempty_outside_grid() {
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    assert!(l.done_button.w > 0.0 && l.done_button.h > 0.0);
    for s in &l.grid {
        assert!(!l.done_button.contains(s.icon_rect.center_x(), s.icon_rect.center_y()));
    }
}

#[test]
fn arrange_hit_prefers_badge_then_done_then_icon() {
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    let s = &l.grid[0];
    // badge point resolves to RemoveBadge, not GridIcon (badge overlaps icon)
    let hit = hit_test_arrange(&l, s.badge_rect.center_x(), s.badge_rect.center_y());
    assert!(matches!(hit, Hit::RemoveBadge { .. }));
    // done point resolves to DoneButton
    let hit = hit_test_arrange(&l, l.done_button.center_x(), l.done_button.center_y());
    assert_eq!(hit, Hit::DoneButton);
    // an icon body point (away from its badge) resolves to GridIcon
    let far_x = s.icon_rect.x + s.icon_rect.w * 0.9;
    let far_y = s.icon_rect.y + s.icon_rect.h * 0.9;
    assert!(matches!(hit_test_arrange(&l, far_x, far_y), Hit::GridIcon { .. }));
}

#[test]
fn normal_hit_test_ignores_badge_and_done() {
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    // Done button point is a Miss under the normal hit-test
    assert_eq!(hit_test(&l, l.done_button.center_x(), l.done_button.center_y()), Hit::Miss);
}
```

- [ ] **Step 2:** `nix develop --command cargo test -p sc-layout` → FAIL.

- [ ] **Step 3: Implement.**
  - Add `pub badge_rect: Rect` to `IconSlot`. When building each grid and dock
    `IconSlot`, compute a badge square at the icon's top-left, e.g. side
    `icon_size * 0.34`, centered on `(icon_rect.x, icon_rect.y)`:
    ```rust
    let badge = |ir: Rect| {
        let s = ir.w * 0.34;
        Rect { x: ir.x - s / 2.0, y: ir.y - s / 2.0, w: s, h: s }
    };
    ```
    (Use `badge(icon_rect)` for both grid and dock slots.)
  - Add to `Layout`: `pub dock_zone: Rect` = the dock band
    `Rect { x: 0.0, y: dock_top, w: width, h: height * DOCK_HEIGHT }`, and
    `pub done_button: Rect` in the top status area, e.g. right-aligned:
    `Rect { x: width * (1.0 - H_MARGIN) - side, y: height * TOP_PAD, w: side, h: side }`
    with `side = width * 0.12`. Both computed in `compute` (they don't depend on
    arrange state — cheap, always present).
  - Add `Hit::RemoveBadge { app_id: String }` and `Hit::DoneButton`.
  - Add `hit_test_arrange`:
    ```rust
    pub fn hit_test_arrange(layout: &Layout, x: f32, y: f32) -> Hit {
        if layout.done_button.contains(x, y) { return Hit::DoneButton; }
        for s in layout.grid.iter().chain(layout.dock.iter()) {
            if s.badge_rect.contains(x, y) {
                return Hit::RemoveBadge { app_id: s.app_id.clone() };
            }
        }
        hit_test(layout, x, y) // fall through to normal icon/bar/miss
    }
    ```
  - Update every `IconSlot { … }` construction in tests/helpers to include
    `badge_rect` (the two in `compute`; any in existing tests use struct literals? they use `compute`, so fine).

- [ ] **Step 4:** `nix develop --command cargo test -p sc-layout` → PASS (new + existing).

- [ ] **Step 5: Commit** — `git add crates/sc-layout/ && git commit -m "feat(layout): badge, dock zone, done button + hit_test_arrange"`

---

## Task 4: Input primitives — IconSource + resolve_drop

**Files:** Modify + Test: `crates/sc-compositor/src/input_dispatch.rs`

- [ ] **Step 1: Write failing tests** (add to `input_dispatch` tests):

```rust
#[test]
fn resolve_drop_grid_over_dock_is_pin() {
    let m = { let mut m = ShellModel::default(); for i in 0..3 { m.place(format!("app{i}")); } m };
    let l = sc_layout::compute(1224.0, 2700.0, 0, &m);
    let (x, y) = (l.dock_zone.center_x(), l.dock_zone.center_y());
    assert_eq!(resolve_drop(x, y, &l, IconSource::Grid), DropAction::Pin);
}

#[test]
fn resolve_drop_dock_over_grid_is_unpin() {
    let m = { let mut m = ShellModel::default(); m.place("a".into()); m };
    let l = sc_layout::compute(1224.0, 2700.0, 0, &m);
    let (x, y) = (612.0, 300.0); // up in the grid, not the dock band
    assert_eq!(resolve_drop(x, y, &l, IconSource::Dock), DropAction::Unpin);
}

#[test]
fn resolve_drop_grid_over_grid_is_snapback() {
    let m = { let mut m = ShellModel::default(); m.place("a".into()); m };
    let l = sc_layout::compute(1224.0, 2700.0, 0, &m);
    assert_eq!(resolve_drop(612.0, 300.0, &l, IconSource::Grid), DropAction::SnapBack);
}
```

- [ ] **Step 2:** `nix develop --command cargo test -p sc-compositor` → FAIL.

- [ ] **Step 3: Implement** in `input_dispatch.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IconSource { Grid, Dock }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropAction { Pin, Unpin, SnapBack }

/// Decide what a drop at (x,y) means given where the dragged icon came from.
/// `pin` may still fail at the model (full dock) — the caller maps that to snap-back.
pub fn resolve_drop(x: f32, y: f32, layout: &sc_layout::Layout, source: IconSource) -> DropAction {
    let over_dock = layout.dock_zone.contains(x, y);
    match (source, over_dock) {
        (IconSource::Grid, true) => DropAction::Pin,
        (IconSource::Dock, false) => DropAction::Unpin,
        _ => DropAction::SnapBack,
    }
}
```

Add `source: IconSource` to `DownAction::PressIcon`, and set it in `on_press`:
`Hit::GridIcon { .. } => … PressIcon { source: IconSource::Grid, … }`,
`Hit::DockIcon { .. } => … PressIcon { source: IconSource::Dock, … }`.
Update the `input_common.rs` `on_press` match arm to destructure/carry `source`
(next task uses it) — for now bind it and pass into the pending state or ignore
with `source: _` to keep this task compiling. Update the existing
`press_on_icon_arms_pending_not_launch` test if it matches `PressIcon` fields.

- [ ] **Step 4:** `nix develop --command cargo test -p sc-compositor` → PASS. `nix develop --command cargo build -p sc-compositor` clean.

- [ ] **Step 5: Commit** — `git add crates/sc-compositor/ && git commit -m "feat(input): IconSource + resolve_drop drop-target logic"`

---

## Task 5: Arrange state machine — hold timer, drag, hide, exit

**Files:** Modify: `crates/sc-compositor/src/main.rs` (State fields, `advance_frame`), `crates/sc-compositor/src/input_common.rs` (press/move/release)

This task has little pure-unit surface (needs a live `State`); verify via build + the existing compositor test suite staying green, then the manual pass in Task 6. Keep changes small and self-contained.

- [ ] **Step 1: Add state + types.** In `main.rs`, near `pending_launch`/`switcher_drag`:

```rust
/// An icon press being held, to detect a long-press into arrange mode.
struct IconPress {
    app_id: String,
    source: input_dispatch::IconSource,
    start: (f32, f32),
    at: std::time::Instant,
}
/// Live drag inside arrange mode.
struct DragItem {
    app_id: String,
    source: input_dispatch::IconSource,
    cur: (f32, f32),
}
/// Arrange (edit) mode. Presence on State = active.
#[derive(Default)]
struct ArrangeState { drag: Option<DragItem> }
```

Add fields to `State`: `icon_press: Option<IconPress>` and `arrange: Option<ArrangeState>`, both initialized `None` in the constructor. Add const `const HOLD_MS: u128 = 500;` and reuse `ICON_TAP_SLOP` from `input_common` (make it `pub(crate)` or re-declare a slop const here).

- [ ] **Step 2: Arm the hold on icon press.** In `input_common::on_press`, in the
`DownAction::PressIcon` arm (which now carries `source`), also set:

```rust
state.icon_press = Some(IconPress {
    app_id: app_id.clone(), source, start: (start_x, start_y), at: std::time::Instant::now(),
});
```
(Keep the existing `pending_launch` + `page_drag_start` arming.)

- [ ] **Step 3: Fire the hold timer.** At the top of `advance_frame` (before the
`Tick` transition, or right after), if not already arranging:

```rust
if self.arrange.is_none() {
    if let Some(p) = &self.icon_press {
        if p.at.elapsed().as_millis() >= HOLD_MS {
            let drag = DragItem { app_id: p.app_id.clone(), source: p.source, cur: p.start };
            self.arrange = Some(ArrangeState { drag: Some(drag) });
            self.pending_launch = None;      // don't launch on release
            self.page_drag_start = None;     // don't page-swipe on release
            self.icon_press = None;
        }
    }
}
```

Arrange mode must keep the frame loop running while a drag is live so the lifted
icon tracks the finger — the winit/DRM loops already `advance_frame` continuously
(no redraw gating), so no extra scheduling is needed; confirm during Task 6.

- [ ] **Step 4: Move.** In `on_motion`, before the page-drag block:
  - If `state.arrange` has a live drag, set `drag.cur = (x, y)` and `return`
    (skip page-drag/pending-launch handling).
  - Cancel a pending hold if the finger leaves slop before the timer:
    in the existing pending-launch slop check, also `state.icon_press = None` when
    travel exceeds `ICON_TAP_SLOP`.

- [ ] **Step 5: Press while arranging.** At the very top of `on_press`, before the
switcher/dispatch logic, if `state.arrange.is_some()`:

```rust
let (x, y) = /* last_pointer_pos guard as existing */;
let layout = sc_layout::compute(w, h, page, &state.model); // page from UiState::Home
match sc_layout::hit_test_arrange(&layout, x, y) {
    Hit::RemoveBadge { app_id } => { state.model.hide(&app_id); state.after_arrange_edit(); }
    Hit::DoneButton | Hit::Miss | Hit::Bar => { state.arrange = None; }
    Hit::GridIcon { app_id, .. } => start_drag(state, app_id, IconSource::Grid, x, y),
    Hit::DockIcon { app_id, .. } => start_drag(state, app_id, IconSource::Dock, x, y),
}
state.pointer_down = true;
return;
```
where `after_arrange_edit()` is a small `State` helper: `recompute_pages(catalog, now)` + `config_state::save(...)` (factor the catalog-ids/now/save trio already used in `launch_or_raise` into one helper and call it from both). `start_drag` sets `arrange.drag = Some(DragItem { … })`.

- [ ] **Step 6: Release while arranging.** At the top of `on_release`, if
`state.arrange` has a live drag:

```rust
let drag = state.arrange.as_mut().unwrap().drag.take().unwrap();
let layout = sc_layout::compute(w, h, page, &state.model);
match resolve_drop(x, y, &layout, drag.source) {
    DropAction::Pin => { if state.model.pin(&drag.app_id) { state.after_arrange_edit(); } }
    DropAction::Unpin => { state.model.unpin(&drag.app_id); state.after_arrange_edit(); }
    DropAction::SnapBack => {}
}
state.pointer_down = false;
return; // stay in arrange mode
```

- [ ] **Step 7:** `nix develop --command cargo test -p sc-compositor` → PASS (no regressions). `nix develop --command cargo build -p sc-compositor` → clean.

- [ ] **Step 8: Commit** — `git add crates/sc-compositor/ && git commit -m "feat(compositor): arrange-mode state machine (hold, drag, hide, exit)"`

---

## Task 6: Render arrange mode

**Files:** Modify: `crates/sc-compositor/src/skia_gl.rs`, `crates/sc-compositor/src/render.rs` (thread arrange snapshot into `draw_home`)

- [ ] **Step 1: Thread arrange info into `draw_home`.** Add a parameter, e.g.
`arrange: Option<ArrangeView<'_>>` where `ArrangeView { drag_app: Option<&str>, drag_pos: Option<(f32,f32)>, over_dock: bool }` (a small render-only view built at the `render.rs` call site from `state.arrange`). When `None`, draw exactly as today.

- [ ] **Step 2: Draw badges + Done + lifted icon.** When `arrange` is `Some`:
  - For each grid and dock `IconSlot`, draw a filled circle with a '-' glyph at
    `slot.badge_rect`.
  - Draw the Done button (`layout.done_button`) — a rounded rect with "Done".
  - If `over_dock`, stroke/tint `layout.dock_zone` as a drop highlight.
  - If `drag_app`/`drag_pos` set, draw that app's icon centered at `drag_pos`,
    scaled ~1.2×, last (on top). Skip drawing it in its origin slot (or just let it
    draw underneath — simplest is to draw the lifted copy on top).
  - Use the same `layout = sc_layout::compute(...)` the draw already computes for
    grid/dock so badge/zone/done rects line up.

- [ ] **Step 3: Build + manual verify** (run-springchick skill):
  - `driver.sh build` (background / long timeout), then `up`, `client`, screenshot home.
  - `send` a long-press: `down X Y` on a grid icon, wait > 500 ms (`settle 700`),
    then `move` to the dock band, `up`. Screenshot: icon should be pinned in the
    dock and gone from the grid. Inspect `~/.config/springchick/state.toml` for the
    `dock` entry.
  - Repeat dragging a dock icon up to the grid → unpinned.
  - `down`/`up` on a grid icon's top-left badge (after entering arrange) → app
    hidden; verify `hidden` in `state.toml`.
  - Tap Done → arrange exits (badges gone).
  - **Back up `state.toml` first and restore afterward** (as done for the frecency
    verification) so the real user config isn't mutated.

- [ ] **Step 4: Commit** — `git add crates/sc-compositor/ && git commit -m "feat(compositor): render arrange mode (badges, done, lifted icon)"`

---

## Done criteria

- `cargo test` green across `sc-shell-model`, `sc-config`, `sc-layout`, `sc-compositor`.
- Long-press lifts an icon; drag to dock pins (and it leaves the grid); drag a dock
  icon to the grid unpins; '-' hides an app; Done/tap-empty/Home exits arrange.
- Full dock (4) drop snaps back.
- `state.toml` persists `dock` + `hidden`; grid reflects both via `recompute_pages`.
- No unhide UI (deferred to swipe-down search). Jiggle + dock↔dock reorder deferred.
