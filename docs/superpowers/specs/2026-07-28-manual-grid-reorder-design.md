# Manual Home-Grid Reorder (retire frecency ordering)

**Date:** 2026-07-28
**Status:** Design approved
**Supersedes ordering behavior from:** `2026-07-27-frecency-home-grid-design.md`

## Problem

The home grid is ordered by frecency (`ShellModel::recompute_pages`), recomputed
on every launch. In practice the icon you tap visibly moves during the launch
zoom because `record_launch` bumps its score and the grid re-sorts. This feels
bad.

We want:
1. Stop reordering the grid automatically. Icons stay where the user put them.
2. Keep frecency **data** (launch counts, decayed scores) for future
   search-result ordering — just stop using it to sort the grid.
3. Let the user manually reorder the home grid by dragging, aligned with the
   existing drag-to-dock interaction (lift the icon out, drop reinserts it).

The pre-frecency model already had persisted `pages` plus a `move_to` for
drag-rearrange (git `b4136d7`). This design restores persisted `pages` and adds
the live drag-reorder interaction on top of the existing arrange-mode drag
infrastructure.

## Data model (`sc-shell-model`)

- Remove `#[serde(skip)]` from `ShellModel::pages`; add `#[serde(default)]` so
  old config files (which never persisted pages) deserialize to an empty vec.
  `pages` becomes the persisted source of truth for grid order.
- Keep `FrecencyStore` and its methods (`record_launch`, `eff`, `seed`,
  `prune`). It is now **data only** — recorded on launch, never used to sort the
  grid. Reserved for future search ordering.
- Keep `hidden`.
- **Delete `recompute_pages`** (the frecency sort). Replace with `reconcile`.

### `reconcile(&mut self, catalog_ids: &[AppId])`

Keeps `pages` in sync with the installed catalog without ever reordering
existing slots:

- **Append** any catalog id not already present in `pages`, `dock`, or `hidden`
  via the existing `place()` (first page with room, else new page). Catalog ids
  arrive sorted, so a first-run empty config seeds alphabetically.
- **Prune** ids no longer in the catalog from `pages`, `dock`, and `hidden`
  (collapse emptied pages, as `delete` does). `frecency.prune(catalog_ids)`
  unchanged.

Called at startup and whenever the catalog changes. It only adds/removes; it
never moves an app that is already placed.

## Arrange-mode edits stop recomputing

`pin`/`unpin`/`hide`/`unhide` currently depend on `recompute_pages` to sync the
grid (docked/hidden apps were excluded by the re-sort). With the sort gone they
must mutate `pages` directly:

- `pin(app)` → push to dock **and** remove from `pages`.
- `unpin(app)` → remove from dock **and** `place()` back into `pages`.
- `hide(app)` → add to `hidden` **and** remove from `pages`.
- `unhide(app)` → remove from `hidden` **and** `place()` back into `pages`.

`after_arrange_edit` (compositor) drops its `recompute_pages` call: it now just
`save` + `reflow_grid` (retarget the existing grid springs to the mutated
layout).

## Drag-to-reorder interaction (arrange mode)

Reorder lives inside the existing arrange mode (long-press an icon to enter).
The drag state (`arrange.drag`, `ArrangeView.drag_app`/`drag_pos`) already
exists for drag-to-dock; we extend the drop resolution and add live gap-opening.

### Drop resolution (`input_dispatch::resolve_drop`)

Add a variant:

```rust
enum DropAction {
    Pin,
    Unpin,
    Reorder { page: usize, index: usize },
    SnapBack,
}
```

- source = Grid, over dock zone → `Pin` (unchanged).
- source = Dock, over grid → `Unpin` (unchanged).
- source = Grid, over grid (not dock) → `Reorder { page, index }`, where the
  target slot is the nearest grid slot to the drop point on the current page,
  clamped to the page's filled length. Replaces the old `SnapBack` for this
  case.
- otherwise → `SnapBack`.

On `Reorder`, the compositor calls the existing
`model.move_to(app, page, index)` then `after_arrange_edit`.

### Live gap-opening (the requested feel)

While a grid icon is being dragged:

- The dragged app is pulled out of a **working layout** used only for
  positioning the other icons, so remaining icons reflow-compact to close the
  gap (via the existing `grid_anim` springs).
- Each frame, the hovered target slot is computed from the current finger
  position (same nearest-slot logic as `resolve_drop`). The working layout
  inserts a hole at that slot so the surrounding icons spring aside to open a
  gap — iOS-style, the hole follows the finger.
- The dragged icon renders as a ghost at `drag_pos` (existing `ArrangeView`).
- On drop, `move_to` commits the app at the hovered slot; `reflow_grid` settles
  everything. On an invalid drop the drag snaps back (no model change).

This reuses the reflow spring infrastructure; the only new per-frame work is
tracking the hovered slot and retargeting springs to the holed working layout.

## Launch change (`launch_or_raise`)

- Keep `model.frecency.record_launch(app_id, now)` (data).
- Remove the `after_arrange_edit()` call — the grid no longer reorders on
  launch.
- Remove the `landed_origin` / page-follow block: since the icon does not move,
  the zoom origin is simply the tap point (`origin` already passed in). This
  simplifies the function.

## Migration

Existing config files carry `frecency` but no persisted `pages` (it was
`serde(skip)`). With `#[serde(default)]` they load `pages = []`; the startup
`reconcile` then seeds pages alphabetically from the catalog. Chosen over a
one-time frecency-based freeze for simplicity and determinism; the current
population is small, so a fresh alphabetical layout is acceptable.

## Testing

Model (`sc-shell-model`):
- `reconcile` appends catalog ids absent from pages/dock/hidden, in catalog
  order; prunes stale ids from all three; leaves already-placed apps in their
  existing slots.
- `pin`/`unpin`/`hide`/`unhide` mutate `pages` as specified (round-trip: pin
  then unpin restores presence; hide then unhide restores presence).
- `move_to` reorder within/across pages (existing test retained).
- `pages` round-trips through serde; old file without `pages` loads empty.

Dispatch (`input_dispatch`):
- `resolve_drop` grid-over-grid returns `Reorder` at the expected slot for a
  known layout/drop point.
- grid-over-dock still `Pin`; dock-over-grid still `Unpin`.

Compositor:
- Launch does not change page order (grid order stable across a launch).

## Out of scope

- Search UI / using frecency for search (data is preserved for it, not consumed
  here).
- Cross-page drag-drop while dragging (drop resolves on the current page; moving
  an icon to another page is a future addition if wanted).
