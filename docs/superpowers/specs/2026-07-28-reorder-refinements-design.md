# Home Reorder Refinements

**Date:** 2026-07-28
**Status:** Design approved
**Builds on:** `2026-07-28-manual-grid-reorder-design.md` (manual persisted grid order + arrange-mode drag reorder + cross-page edge-dwell flip)

## Problem

The manual grid reorder shipped and works, but device testing surfaced four rough edges:

1. **Dock drags feel different from grid drags.** Pulling an app out of the dock is an instant unpin+append with no lift ghost, no live gap, and the remaining dock icons don't reflow. It should use the same motion as a grid drag.
2. **Can't change pages while editing.** In arrange mode, page-drag isn't wired, and tapping empty space exits. You can't browse to another page to drop an icon there (except via the mid-drag edge-dwell flip).
3. **Removing an app leaves a permanent hole.** Pinning to the dock (or hiding) removes an app from its grid slot, but the grid only compacts within the page and drops empty pages — it never pulls apps back from later pages, so a gap can persist mid-grid.
4. **No visible drop target.** The live drag reflows by compaction only (dragged app removed, rest slide up); there's no gap that opens under the finger to show where the drop will land.

## Core model change: `repack` (pages become a chunked flat order)

The "global repack" decision (only the last page may be partial) means `pages` is equivalent to **one linear ordered list chunked by `PAGE_CAP`**. Adopt that as the invariant:

- **`ShellModel::repack(&mut self)`** replaces `normalize_pages`: flatten every page into one `Vec<AppId>`, then re-chunk into `PAGE_CAP`-sized pages, dropping any empty tail. A dense re-chunk cannot leave a hole or an overflow, so this single operation handles **both** overflow cascade *and* cross-page hole backfill (problem 3).
- **`move_to(app, page, index)`** becomes: remove `app` everywhere, compute the global insertion index, insert into the flattened order, then `repack`. Global index = `page.min(last_page) * PAGE_CAP + index`, clamped to the flattened length. Cross-page drops need no special case.
- Removal paths (`pin`, `hide`, and their `remove_from_pages` helper) call `repack` after removing, so the grid backfills globally.
- Persistence is unchanged on disk: `pages` still serializes as `Vec<Vec<AppId>>` (always in repacked form after any edit). No migration needed.

`repack` is the one place the packing invariant lives; every mutation ends by calling it (directly or via `after_arrange_edit`).

### Model API after this change

- `repack(&mut self)` — new; delete `normalize_pages`.
- `move_to(app, page, index)` — reworked to global-index insert + repack.
- `pin` / `hide` — after removing from pages, `repack` (unpin/unhide already re-`place` then callers repack via `after_arrange_edit`).
- `flat(&self) -> Vec<AppId>` — private helper (concat pages) used by `repack`, `move_to`, and the compositor's working-order builder if useful. If exposed, keep it `pub(crate)`-ish minimal.

## Live visible gap (problem 4)

Replaces the compaction-only working order.

- **Working order:** flatten `model.pages`, remove the dragged app, and when the finger is over the grid, **insert a hole placeholder at the hovered global index**; re-chunk into pages. Icons part around the hole, so the gap tracks the finger and shows the drop location.
- The hole is a sentinel (e.g. a reserved app-id string that never collides, or an `Option<AppId>` working representation). `reflow_targets_for` lays out real entries by their post-hole index; the sentinel occupies a slot but is not drawn.
- **Over the dock zone** (drop would pin, not reorder) the working order has **no** hole — just the dragged app removed and everything compacted — so the grid doesn't misleadingly promise a grid slot.
- On drop over the grid, commit `move_to(app, hovered_page, hovered_index)` where the hovered index is the slot the hole occupied.
- The dragged app remains absent from `grid_anim` (rendered as the ghost), as today.

## Dock symmetry (problem 1)

Make a dock-sourced drag behave like a grid-sourced drag, and animate the dock.

- **`dock_anim`**: a parallel spring map for dock icon positions (mirrors `grid_anim`; ≤ `DOCK_CAP` = 4 entries). The dock is currently drawn from static `layout.dock` centers; route dock icon draw positions through `dock_anim` so they can animate. A `reflow_dock` retargets the springs to the current dock layout, called wherever `reflow_grid` is (and each frame during a drag).
- **Lifting from the dock:** the dragged dock app is removed from the dock working-set, so the remaining dock icons reflow (re-center) via `dock_anim`. The lifted app rides as the ghost — same as a grid lift.
- **Dock → grid drop:** honors the hovered grid slot (the answer to the drop-location question). Resolve to unpin **and** place at the hovered global index: `unpin` (drops from dock) then `move_to(app, hovered_page, hovered_index)`. The live grid gap opens under the finger during the drag exactly as for a grid-sourced reorder.
- **Grid → dock drop (pin):** unchanged intent — append to the dock — but the grid now backfills globally via `repack` (problem 3, "hole in the card"). During the drag the grid shows the app lifted out (compacted, no hole, since drop is a pin).
- `resolve_drop` gains the dock-source-over-grid → a reorder-like action carrying the hovered `(page, index)`; the caller performs unpin+move_to. Grid-source-over-dock stays Pin; dock-over-dock stays SnapBack.

## Edit-mode paging (problem 2)

- Wire page-drag in arrange mode: a horizontal swipe starting on empty grid area (a `Hit::Miss` with movement past the swipe threshold) scrolls/flips pages, using the same page-drag machinery Home uses outside arrange.
- A **still tap** on empty area (no movement past the tap slop) still **exits** arrange (as today). The Done button still exits.
- Distinguish the two by the existing tap-vs-swipe classification (movement threshold) already used elsewhere: arm both a potential page-drag and a potential exit on empty-area press; whichever the release resolves to wins.
- Mid-drag paging via edge-dwell flip is unchanged.

## Interaction summary

| Gesture | Result |
|---|---|
| Drag grid icon over grid | live gap at hovered slot; drop reorders (global) |
| Drag grid icon over dock | pin (append dock); grid repacks to backfill |
| Drag dock icon over grid | dock reflows on lift; live gap; drop unpins + inserts at hovered slot |
| Drag dock icon over dock | snap back |
| Swipe empty area (arrange) | change page, stay in arrange |
| Tap empty area (arrange) | exit arrange |
| Done button | exit arrange |
| Edge-dwell during drag | flip page (unchanged) |

## Testing

Model (`sc-shell-model`):
- `repack`: flatten+re-chunk fills every page but the last; drops empty tail; a mid-list hole is backfilled from later pages; an over-`PAGE_CAP` page cascades forward. (Replaces the `normalize_pages` tests.)
- `move_to`: global-index insert across pages (e.g. move page-0 item to a page-1 index lands at the right global position and repacks); within-page reorder still holds. Update `move_to_reorders_within_page` expectation to the repacked result.
- `pin`/`hide` backfill: removing a mid-grid app pulls the next app back so no interior hole remains.

Dispatch (`input_dispatch`):
- Dock-source-over-grid resolves to the unpin+reorder action with the hovered `(page, index)`; grid-over-dock still Pin; dock-over-dock still SnapBack.

Compositor (unit where possible; interactive smoke for the rest):
- Working-order builder inserts a hole at the hovered index over the grid, and no hole over the dock zone.
- Interactive (run-springchick): dock lift reflows the dock; dock→grid drops at the hovered slot with a visible gap; grid→dock leaves no interior hole; swipe changes page in arrange while tap exits.

## Out of scope

- Search UI / consuming frecency (still write-only data).
- Reordering within the dock (dock is append-only on pin; dock→dock is snap-back).
