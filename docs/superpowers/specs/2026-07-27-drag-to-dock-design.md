# Drag-to-dock + arrange mode — design

Date: 2026-07-27
Status: Approved (design)

## Problem

The dock (`ShellModel.dock`, cap 4) is user-pinnable in the data model but there
is no UI to pin/unpin apps — the dock is empty and unreachable. The home grid is
frecency-derived and shows every catalog app, with no way to remove an app from
home. This spec adds a touch **arrange mode** for pinning apps to the dock,
unpinning them, and hiding apps from the grid.

Depends on the frecency-derived-grid work (already merged): grid `pages` are
recomputed from catalog minus `dock`, so pinning/hiding an app removes it from the
grid automatically via `recompute_pages`.

## Interaction model

- **Enter arrange mode:** long-press hold (~500 ms, finger still within slop) on
  any home icon. Before the timer fires, behavior is unchanged: early release =
  tap-launch, movement past slop = page swipe.
- **In arrange mode**, single-finger presses do:
  - **Drag an icon** → drop on the dock zone = pin; drag a dock icon onto the grid
    = unpin. Dropping on a full dock (4) snaps the icon back.
  - **Tap an icon's '-' badge** (top-left corner) → hide that app from the grid
    (adds to a persisted `hidden` set; app stays installed).
- **Exit arrange mode:** tap the **Done** button, tap empty space, or press Home.
  Exiting commits (all mutations already persisted on each action).

Out of scope (YAGNI / deferred): jiggle animation, dock↔dock reorder, and any
**unhide** UI. Hidden apps are recovered later via the planned swipe-down search
(a separate feature); this spec ships hide with no in-arrange unhide.

## Architecture

Arrange/edit state lives on the compositor `State`, **not** in `UiState`. `UiState`
stays `Home` throughout — mirroring how `pending_launch`, `page_drag_start`, and
`switcher_drag` already layer transient input state over a `UiState`. This avoids
threading an `edit` field through `UiState::Home`'s many constructors.

### 1. Model (`crates/sc-shell-model/src/lib.rs`)

- Add `#[serde(default)] pub hidden: Vec<AppId>` to `ShellModel`.
- `recompute_pages` filters out **dock ∪ hidden** (currently only dock).
- Helpers (each a pure mutation; caller runs `recompute_pages` + save afterward):
  - `pin(&mut self, app: &str) -> bool` — if `app` not already docked and
    `dock.len() < DOCK_CAP`, push to `dock`, return `true`; else `false`.
  - `unpin(&mut self, app: &str)` — remove from `dock`.
  - `hide(&mut self, app: &str)` — add to `hidden` if absent.
  - `unhide(&mut self, app: &str)` — remove from `hidden` (no UI yet; future
    search uses it. Kept so the data path exists).
- Persisted fields are now `dock`, `frecency`, `hidden`. `pages` remains
  `#[serde(skip)]`.

### 2. Layout (`crates/sc-layout/src/lib.rs`)

- `IconSlot` gains `badge_rect: Rect` — a small square at the top-left corner of
  `icon_rect` (the '-' remove target). Computed always; only rendered/hit in
  arrange mode.
- `Layout` gains:
  - `dock_zone: Rect` — the full dock band, used as the pin drop target.
  - `done_button: Rect` — Done affordance (e.g. top-right of the status area),
    shown only in arrange mode.
- New `Hit` variants: `RemoveBadge { app_id: String }` and `DoneButton`. Add a
  **separate `hit_test_arrange(layout, x, y) -> Hit`** (same signature style as
  `hit_test`) rather than a flag, so normal-mode `hit_test` stays untouched and
  keeps ignoring the badge/Done rects. `badge_rect` overlaps the top-left of
  `icon_rect`, so `hit_test_arrange` **must check `RemoveBadge` and `DoneButton`
  before** falling through to `GridIcon`/`DockIcon`, or the badge is unhittable.
- Drop-target resolution during a drag uses `dock_zone.contains(finger)` directly,
  not `Hit`.

### 3. Edit state (`crates/sc-compositor/src/`)

On `State`:

```rust
/// Set when an icon press is being held, to detect a long-press.
icon_press: Option<IconPress>,   // { app_id, source: IconSource, start: (f32,f32), at: Instant }
/// Some(_) => arrange mode is active.
arrange: Option<ArrangeState>,   // { drag: Option<DragItem> }
```

```rust
enum IconSource { Grid, Dock }
struct DragItem { app_id: String, source: IconSource, cur: (f32, f32) }
```

`HOLD_MS` (~500) and the badge/done geometry are constants.

### 4. Input (`input_common.rs`, `input_dispatch.rs`)

- **Press, not arranging:** on a grid/dock icon, in addition to arming
  `pending_launch` + `page_drag_start` (unchanged), record
  `icon_press = Some(IconPress { … , at: Instant::now() })`. `DownAction::PressIcon`
  currently does **not** distinguish grid vs dock (`input_dispatch.rs` emits the
  same variant for both `Hit::GridIcon`/`Hit::DockIcon`), so add a `source:
  IconSource` field to `PressIcon` (plan must touch the `DownAction` enum) and thread
  it into `IconPress`.
- **Frame tick / `advance_frame`:** if `icon_press` is held longer than `HOLD_MS`
  and the finger is still down within slop and `arrange.is_none()`, enter arrange
  mode: `arrange = Some(ArrangeState { drag: Some(DragItem { … , source }) })`,
  clear `pending_launch` and `page_drag_start` (so release won't launch/swipe).
  This needs a redraw; arrange mode keeps requesting frames while a drag is live.
- **Move:**
  - If `arrange` has a live `drag`, update `drag.cur = (x, y)` (follow finger); do
    **not** run the page-drag path.
  - If arming a hold (`icon_press` set, not yet arranged) and the finger passes
    slop, clear `icon_press` (the gesture is a page swipe — existing path already
    cancels `pending_launch`).
- **Press while arranging** (arrange active, no live drag): hit-test arrange:
  - `RemoveBadge { app_id }` → `model.hide(&app_id)`, recompute, save. Stay.
  - `DoneButton` → exit arrange (`arrange = None`).
  - `GridIcon`/`DockIcon` → begin a drag (`drag = Some(DragItem { source, … })`).
  - `Miss`/`Bar` → exit arrange.
- **Release:**
  - If a live `drag`: resolve the drop —
    - `dock_zone.contains(cur)` and `source == Grid` → `if model.pin(&app) { … }`
      else snap back. On success: recompute, save.
    - source `== Dock` and drop is on the grid area (not `dock_zone`) →
      `model.unpin(&app)`, recompute, save.
    - otherwise → snap back (no model change).
    - Clear `drag` (stay in arrange mode for further edits).
  - If arranging without a live drag: nothing extra (press already handled taps).
- A pure helper `resolve_drop(cur, layout, source) -> DropAction` (enum: `Pin`,
  `Unpin`, `SnapBack`) is extracted so the decision is unit-testable without a
  `State`.

### 5. Render (`crates/sc-compositor/src/skia_gl.rs`)

When `state.arrange.is_some()`:
- Draw a '-' badge at each grid + dock icon's `badge_rect`.
- Draw the Done button.
- Highlight `dock_zone` when a live drag's `cur` is over it.
- Draw the lifted/dragged icon at `drag.cur`, scaled up, above everything.

Normal-mode rendering is unchanged when `arrange.is_none()`.

### 6. Persistence

`config_state::save` after each committing action (pin / unpin / hide) — same
per-action save pattern already used for frecency launches. Atomic-write path
(added in the frecency work) covers durability.

## Testing

Pure, unit-testable pieces get real coverage; timer + render are manual-verified.

- **Model** (`sc-shell-model`): `pin` succeeds under cap and fails when full or
  already docked; `unpin` removes; `hide`/`unhide`; `recompute_pages` excludes both
  dock and hidden; round-trip persists `hidden` (extend `sc-config` tests).
- **Layout** (`sc-layout`): `badge_rect` sits at the icon's top-left and is inside
  `icon_rect`'s corner; `dock_zone` spans the dock band; `done_button` is non-empty
  and outside the grid; arrange hit-test returns `RemoveBadge`/`DoneButton` for
  those rects and normal `hit_test` still ignores them.
- **Drop logic**: `resolve_drop` → `Pin` when a grid drag ends over `dock_zone`,
  `Unpin` when a dock drag ends on the grid, `SnapBack` otherwise (and `SnapBack`
  is what the caller maps to "full dock" via `pin` returning false).
- **Manual (run-springchick)**: long-press lifts an icon; drag to dock pins it (and
  it leaves the grid); drag a dock icon out unpins; '-' hides an app; Done exits;
  full-dock drop snaps back; `state.toml` gains `dock`/`hidden` entries.

## Risks

- **No unhide UI**: a hidden app is unreachable until swipe-down search ships.
  Accepted; `unhide` exists in the model for that future path.
- **Long-press vs page-swipe timing**: the hold timer must not fire if the finger
  has begun swiping. Guard: only arm on an icon press, and cancel on slop before
  `HOLD_MS`. Covered by keeping the existing slop-cancel path.
- **Edit state on `State`, not `UiState`**: render and input must both check
  `state.arrange`; a missed check leaves a visual/logic desync. Mitigated by the
  single `arrange.is_some()` gate driving both.
