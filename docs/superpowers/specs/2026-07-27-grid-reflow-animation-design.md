# Grid reflow animation — design

Date: 2026-07-27
Status: Approved (design)

## Problem

The home grid is frecency-ordered and recomputed on every launch (and on
arrange-mode pin/unpin/hide). The reorder is applied instantly: because apps take
time to spawn on device, Home stays visible during the wait, and the launched
icon (plus everything it displaces) **teleports** to a new slot — jarring. We want
the grid to animate: icons slide from old positions to new, and for a launch the
view follows the app to its landing page, which then becomes the zoom-open origin.

Builds on: frecency-derived grid (`recompute_pages`) and arrange mode (both merged).

## Decisions

- **Full reflow**: every icon that shifts animates, not just the launched one.
- **Cross-page**: icons animate across page boundaries (unified coordinate space).
- **View follows** the launched app to its new page; the app then zoom-opens from
  its landed slot.
- **Arrange edits animate too**: pin/unpin/hide reorder the visible grid and reuse
  the same reflow (page-follow is launch-only).

## Architecture

### Global grid coordinate space

Today `draw_home` loops per page, calling `sc_layout::compute` and drawing static
slots. Replace the grid portion with **per-app animated positions in one global
space** where pages lie side-by-side: an app in `(page, slot)` has a global center
`(gx, gy)` with `gx` including `page * width`. Screen position is
`(gx - page_scroll * width, gy)`, where `page_scroll` is the existing fractional
`page_spring` value (so page-swipe and reflow are the same mechanism — moving
things in global space / moving the viewport).

### 1. `sc-layout` (pure)

Add:

```rust
/// Global-space center of the icon for a flattened (page, index_in_page) slot.
/// x includes page * width so pages lie side-by-side in one coordinate space.
pub fn global_slot_pos(page: usize, index: usize, width: f32, height: f32) -> (f32, f32);
```

Computed from the same cell geometry `compute` uses (COLS/ROWS/margins/TOP_PAD),
just offset by `page * width` in x. Dock/dots/bar are unaffected (they don't
scroll or reflow).

Also add, so the renderer can build a slot at an animated center with the same
icon/label/badge geometry `compute` produces (single-sourced):

```rust
/// An IconSlot whose icon/label/badge rects are centered at (cx, cy), using the
/// same sizes `compute` derives for grid slots at the given output size.
pub fn slot_at_center(app_id: String, cx: f32, cy: f32, width: f32, height: f32) -> IconSlot;
```

### 2. `sc-compositor` state

On `State`:

```rust
/// Per-app animated grid position (global space). Absent = not yet shown.
grid_anim: HashMap<AppId, (sc_anim::Spring /* x */, sc_anim::Spring /* y */)>,
```

Pure helper (module-level, testable):

```rust
/// Global target center for every grid app in the current model order.
fn reflow_targets(model: &ShellModel, width: f32, height: f32) -> HashMap<AppId, (f32, f32)>;
```
It walks `model.pages` (`page`, `index`) → `global_slot_pos`.

Controller method on `State`:

```rust
/// Retarget grid springs to the current model order. Seeds a missing app's
/// springs at its current target (so a first appearance snaps, not flies in).
/// Drops springs for apps no longer on the grid (hidden/pinned).
fn reflow_grid(&mut self);
```
- For each `(app, target)` in `reflow_targets`: if a spring pair exists, `retarget`
  x and y (preserves velocity → interruptible); else insert `Spring::new(target)`
  (snapped).
- Remove `grid_anim` entries whose app is not in the new targets.

### 3. Launch flow (`launch_or_raise`) and arrange edits

Centralize: **any code that calls `recompute_pages` then calls `self.reflow_grid()`.**
- `after_arrange_edit` (pin/unpin/hide): `recompute_pages` + `reflow_grid` + save.
- `launch_or_raise`: after `record_launch` + `recompute_pages`:
  1. `self.reflow_grid()` — retarget all grid springs from their current positions.
  2. **Follow (launch only):** find the launched app's new `(page, _)` in
     `model.pages`; `page_spring.retarget(page as f32)` and set the integer `page`
     (via the `UiState::Home { page, .. }`) so page state stays consistent.
  3. **Zoom origin:** capture `ZoomOrigin::icon` from the launched app's **new**
     global slot converted to screen space at the *target* `page_scroll` (i.e. its
     on-screen center once the view has followed). This makes the app zoom-open
     from where it lands. If the app isn't on the grid (e.g. it's docked), fall
     back to today's tapped-icon origin.

The springs + page scroll then animate on the visible Home surface while the app
spawns; when it maps (`AppMapped`), the existing zoom-open runs from the captured
origin.

### 4. Render (`skia_gl` / `render.rs`)

Replace the per-page grid loop in `draw_home` with a per-app pass:
- For each app in the current grid (or each `grid_anim` entry), read its animated
  global `(gx, gy)`, convert to screen `(gx - page_scroll * width, gy)`, cull if
  outside the viewport (with a margin), and draw it there.
- **Build a synthetic `IconSlot` at the animated center** (icon_rect, label_rect,
  and badge_rect all offset to `(gx - page_scroll*width, gy)`, using
  `sc_layout` for the icon/label/badge *sizes*). The grid icon, its label, **and
  its arrange-mode remove-badge** are all drawn from this one animated slot — so a
  badge never detaches from a sliding icon during an arrange reflow. Provide a
  small `sc_layout` helper (e.g. `slot_at_center(cx, cy, width, height) ->
  IconSlot`) so the rect geometry stays single-sourced with `compute`.
- The renderer needs the animated positions: thread a
  `grid_positions: &HashMap<AppId,(f32,f32)>` (screen-space, prebuilt at the
  `DrawCtx` construction site from `state.grid_anim` + `page_scroll`) into
  `draw_home`, mirroring how `pressed_app`/`arrange` are threaded.
- **Dock** icons + their arrange badges: **unchanged/static** (the dock does not
  reflow). Dots, bar, Done button, drag-ghost: unchanged.
- Fallback: if an app has no `grid_anim` entry yet (should not happen after
  `reflow_grid` seeds on first frame), draw at its static layout position.

### 5. Lifecycle

- **Advance:** step all `grid_anim` springs in `advance_frame`; keep requesting
  frames while any is unsettled (same pattern as page_spring / arrange).
- **Init:** on first home render (or `State` construction), seed `grid_anim` from
  the current order so nothing animates on cold start — `reflow_grid` seeds-snapped
  for missing apps, so calling it once at startup suffices.
- **Interruptible:** page-swipe moves `page_scroll` only; a second launch or an
  arrange edit retargets mid-flight (velocity preserved).
- **Removed apps:** hidden/pinned apps are dropped from `grid_anim` (snap out, no
  fade in v1).

## Testing

Pure/unit:
- `global_slot_pos`: slot `(0,0)` matches `compute`'s first grid center; `(1,0)`
  is exactly `width` to the right of `(0,0)`; index advances by cell within a page.
- `reflow_targets`: every grid app maps to its `(page,slot)` global pos; dock and
  hidden apps are absent; a cross-page app (page 1) has `x > width`.
- `reflow_grid` (as a pure-ish unit if factored to take model+size, else via a
  thin harness): retargeting a moved app changes its spring target but not its
  current value; a new app is seeded snapped (value == target); a removed app's
  entry is dropped.
- Spring behavior: already covered in `sc-anim`.

Manual (run-springchick): launch an app from page 2 → the grid reflows, the view
follows to page 1, the app lands in slot 0 and zoom-opens from there; pin/unpin/hide
in arrange mode slide the grid; rapid double-launch interrupts smoothly; page-swipe
during a reflow doesn't fight it.

## Risks / notes

- **Render rework**: the grid draw changes from per-page-static to per-app-animated.
  Contained to `draw_home`'s grid section + the `DrawCtx` plumbing; dock/dots/bar
  and arrange overlays are untouched.
- **Origin timing**: the zoom origin is captured at launch from the *target*
  on-screen position (post-follow), not after the spring settles — so a very fast
  reopen still has a sensible origin. Acceptable; the spring is usually near-settled
  by the time the app maps.
- **page_scroll source of truth**: reflow reads `page_spring.value`; the integer
  `page` must be kept in sync when following (set it alongside the retarget) so
  page-count/dot logic stays correct.
- **Cross-page culling**: draw only apps within the viewport (± one icon margin) so
  a full-catalog grid doesn't pay to draw every off-screen page each frame.
