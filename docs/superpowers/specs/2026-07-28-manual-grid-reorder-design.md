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
- **Seed frecency** for newly-appended apps, preserving the existing startup
  behavior (`frecency.seed(id, now, first_run)` — score 0 on a first-run empty
  store, 1.0 for a later install so search can surface it). `reconcile` takes
  `now` and a `first_run` flag (store was empty at bootstrap), replacing the
  startup seed loop that today lives beside `recompute_pages` in `main.rs`.

Called at startup and whenever the catalog changes. It only adds/removes/seeds;
it never moves an app that is already placed. Signature:
`reconcile(&mut self, catalog_ids: &[AppId], now: u64, first_run: bool)`.

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

`resolve_drop`'s signature must grow to build `Reorder`: it currently gets
`(x, y, layout, source)` and has no page number and no per-page fill count
(`layout.grid` is a fixed `COLS*ROWS` slot array, not the number of filled
slots). Extend to
`resolve_drop(x, y, layout, source, page: usize, page_len: usize)`, where `page`
is the currently-visible Home page and `page_len` is that page's filled icon
count (`model.pages[page].len()`), passed by the caller.

- source = Grid, over dock zone → `Pin` (unchanged).
- source = Dock, over grid → `Unpin` (unchanged).
- source = Grid, over grid (not dock) → `Reorder { page, index }`. `page` is the
  current page; `index` is the nearest grid slot to the drop point, clamped to
  `page_len` (so a drop past the last icon appends). Replaces the old `SnapBack`
  for this case.
- otherwise → `SnapBack`.

On `Reorder`, the compositor calls the existing
`model.move_to(app, page, index)` then `after_arrange_edit`.

### Cross-page drag

Dragging near a horizontal edge flips pages so an icon can move to another page:

- While a drag is active and the finger dwells in a left/right **edge zone**
  (narrow band at each screen side) past a dwell threshold (~400 ms), flip to
  the previous/next page. A held edge auto-repeats (flip, keep dwelling, flip
  again). The dragged app keeps following the finger across the flip; the
  working layout recomputes on the now-current page.
- Flipping right past the **last** page appends a trailing empty page as a drop
  target, so a drag can create a new page. If nothing is dropped there, the
  empty trailing page is dropped by `normalize_pages` on settle.
- Drop resolves on whatever page is current when the finger lifts:
  `move_to(app, current_page, slot)` (already supports an arbitrary page index).

### Overflow cascade (`normalize_pages`)

Inserting into a full page can push a page over `PAGE_CAP`. Add
`ShellModel::normalize_pages` that, after any reorder, cascades icons beyond
`PAGE_CAP` onto the following page (creating one if needed) and collapses empty
pages. `after_arrange_edit` calls it before `save`. This keeps every persisted
page within `PAGE_CAP` while letting a mid-drag insert temporarily overflow.

### Live gap-opening (the requested feel)

While a grid icon is being dragged:

- The **working layout** is a transient, drag-only copy of the current page's
  app list with the dragged app removed and a hole inserted at the hovered slot.
  It is *not* a new persistent structure: it is derived each frame from
  `model.pages[page]` plus the drag's `app_id` and hovered index, stored on the
  active drag (extend `arrange.drag` with a `hover: Option<(usize, usize)>`
  page/slot). The model's `pages` is not mutated until drop.
- Each frame, the hovered target slot is computed from the current finger
  position (the same nearest-slot logic as `resolve_drop`). `reflow_grid` is
  fed this working order instead of raw `model.pages`, so the surrounding icons
  spring aside to open a gap — iOS-style, the hole follows the finger. Concretely
  `reflow_targets` / `reflow_grid` gain an optional working-order override for
  the dragged page; absent a drag they use `model.pages` as today.
- The dragged icon renders as a ghost at `drag_pos` (existing `ArrangeView`);
  its origin slot is the hole, so it is not double-drawn.
- On drop, `move_to(app, page, hover_index)` commits, then `normalize_pages`,
  `save`, `reflow_grid` (via `after_arrange_edit`). On an invalid drop the drag
  snaps back — the working layout is discarded, no model change.

This reuses the reflow spring infrastructure; the only new per-frame work is
tracking the hovered slot and feeding the holed working order into `reflow_grid`.

**Index-mapping caveat:** the dragged app stays in `model.pages[page]` until
drop, so `page_len = model.pages[page].len()` counts the dragged icon itself
while the working layout shows `len - 1` real icons plus a hole. Clamp-to-
`page_len` still gives correct append behavior, but the nearest-slot index must
be computed against the **working (hole-removed) order**, not the raw page, or a
same-page reorder skews by one slot.

## Launch change (`launch_or_raise`)

- Keep `model.frecency.record_launch(app_id, now)` (data).
- Remove the `after_arrange_edit()` call — the grid no longer reorders on
  launch.
- **Still persist.** `after_arrange_edit` was the only immediate
  `config_state::save` on launch; the sole other save is graceful shutdown. A
  phone shell is routinely killed, not cleanly exited, so without a save the
  freshly-recorded frecency (the data we are keeping expressly for search) is
  lost on kill. After `record_launch`, call `config_state::save(&self.model,
  &config_path())` directly (log-and-continue on error, as `after_arrange_edit`
  does). Grid order is unchanged so no reflow is needed.
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
- `normalize_pages` cascades a >`PAGE_CAP` page onto the next, creates a page
  when the last overflows, and drops empty trailing pages.
- `pages` round-trips through serde; old file without `pages` loads empty.

Dispatch (`input_dispatch`):
- `resolve_drop` grid-over-grid returns `Reorder` at the expected slot for a
  known layout/drop point.
- grid-over-dock still `Pin`; dock-over-grid still `Unpin`.

Compositor:
- Launch does not change page order (grid order stable across a launch).

### Tests to remove/replace

The ordering change invalidates existing tests that must be deleted or rewritten
(not left to rot):
- `sc-shell-model`: the four `recompute_pages_*` tests, `launch_promotes_app_to_front_of_grid`,
  and `pages_not_serialized_frecency_is` (pages ARE now serialized — replace
  with a round-trip test).
- `input_dispatch`: `resolve_drop_grid_over_grid_is_snapback` (now `Reorder`).
- `main.rs`: `landed_origin_some_for_grid_none_for_absent` (page-follow removed;
  `landed_origin` itself goes away).

## Out of scope

- Search UI / using frecency for search (data is preserved for it, not consumed
  here).
