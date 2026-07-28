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

**Keep the edge-dwell empty-page push.** The cross-page flip in `advance_frame` still does `self.model.pages.push(Vec::new())` when flipping right past the last page — this is **not** dead under repack. It gives `page_count`/paging a real page to navigate to mid-drag; the working order (flatten+hole+re-chunk) tolerates the empty page (it contributes nothing to the flat list). On drop, `move_to`'s hovered global index (`new_page * PAGE_CAP + idx`, clamped to flat length) lands the app at the end and `repack` keeps or drops the page as appropriate. Do not remove this push.

### Model API after this change

- `repack(&mut self)` — new; delete `normalize_pages`.
- `move_to(app, page, index)` — reworked to global-index insert + repack.
- `pin` / `hide` — after removing from pages, `repack` (unpin/unhide already re-`place` then callers repack via `after_arrange_edit`).
- `flat(&self) -> Vec<AppId>` — private helper (concat pages) used by `repack`, `move_to`, and the compositor's working-order builder if useful. If exposed, keep it `pub(crate)`-ish minimal.

### Compositor call sites that must follow the rename

Deleting `normalize_pages` breaks two live callers that must switch to `repack()`:
- `main.rs::after_arrange_edit` (`self.model.normalize_pages()`).
- `input_common.rs::on_release` no-edit fallback (`state.model.normalize_pages()`).

## Live visible gap (problem 4)

Replaces the compaction-only working order.

- **Working order:** flatten `model.pages`, remove the dragged app, and when the finger is over the grid, **insert a hole placeholder at the hovered global index**; re-chunk into pages. Icons part around the hole, so the gap tracks the finger and shows the drop location.
- The hole is a sentinel (e.g. a reserved app-id string that never collides, or an `Option<AppId>` working representation). `reflow_targets_for` lays out real entries by their post-hole index; the sentinel occupies a slot but is not drawn.
- **Over the dock zone** (drop would pin, not reorder) the working order has **no** hole — just the dragged app removed and everything compacted — so the grid doesn't misleadingly promise a grid slot.
- On drop over the grid, commit `move_to(app, hovered_page, hovered_index)` where the hovered index is the slot the hole occupied.
- The dragged app remains absent from `grid_anim` (rendered as the ghost), as today.

## Dock symmetry (problem 1)

Make a dock-sourced drag behave like a grid-sourced drag, and animate the dock.

- **`dock_anim`**: a parallel spring map for dock icon positions (mirrors `grid_anim`; ≤ `DOCK_CAP` = 4 entries). This is **net-new plumbing** — unlike the grid, the dock has no position-map path today. Required pieces:
  - `state.dock_anim: HashMap<String,(Spring,Spring)>` and a `reflow_dock` that retargets it to the current dock layout (called wherever `reflow_grid` is, and each frame during a drag).
  - `draw_home` gains a `dock_positions: &HashMap<String,(f32,f32)>` param (mirroring the existing `grid_positions`), plus a `visible_dock_slots`-style helper that builds dock `IconSlot`s from those animated centers instead of static `current_layout.dock`.
  - The render call site in `main.rs` (`advance_frame`, alongside where `grid_positions` is built from `grid_anim`) builds `dock_positions` from `dock_anim` and passes it in.
  - The dragged dock app is dropped from `dock_anim` during the drag (rendered as ghost), same pattern as `grid_anim`.
- **Lifting from the dock:** the dragged dock app is removed from the dock working-set, so the remaining dock icons reflow (re-center) via `dock_anim`. The lifted app rides as the ghost — same as a grid lift.
- **Dock → grid drop:** honors the hovered grid slot (the answer to the drop-location question). Just `move_to(app, hovered_page, hovered_index)` — its `delete_keep_pages` already strips the app from `dock`, so no separate `unpin` is needed (that would do a throwaway `place` `move_to` immediately undoes). The live grid gap opens under the finger during the drag exactly as for a grid-sourced reorder.
- **Grid → dock drop (pin):** unchanged intent — append to the dock — but the grid now backfills globally via `repack` (problem 3, "hole in the card"). During the drag the grid shows the app lifted out (compacted, no hole, since drop is a pin).
- **`resolve_drop` shape:** dock-source-over-grid returns the **existing** `DropAction::Reorder { page, index }` (no new variant) with the hovered `(page, index)`. The `on_release` caller handles both the same way — `move_to(app, page, index)` (which removes the app from `dock` via `delete_keep_pages` for a dock source, and from its old grid slot for a grid source); no source-specific branch needed. Grid-source-over-dock stays `Pin`; dock-over-dock stays `SnapBack`. The existing test `resolve_drop_dock_over_grid_is_unpin` (which asserts plain `Unpin`) is **replaced** by one asserting `Reorder { page, index }` for dock-over-grid.

## Edit-mode paging (problem 2)

This needs three coordinated changes to the arrange-mode input path; the naïve "arm page_drag_start" alone will not work because of the existing early-returns.

- **`on_press` — split the exit arm.** Today `Hit::DoneButton | Hit::Miss | Hit::Bar => state.arrange = None` (one arm). Split it: `DoneButton` and `Bar` still exit immediately; `Hit::Miss` no longer exits at press — instead it arms a pending page-drag (`page_drag_start = Some(x)`) *and* a pending "exit if this stays a tap" marker, to be resolved at release.
- **`on_release` — don't swallow the empty-area gesture.** The arrange block currently returns unconditionally after handling a taken `drag`. When there is **no** `drag` (finger was on empty area, not an icon), it must instead fall through to / invoke the shared page-swipe-commit logic (`page_drag_start.take()` → classify): a swipe past threshold commits a page flip and stays in arrange; a still tap (under the tap slop) sets `state.arrange = None` (exit). Restructure so the `return` only happens on the drag path, not the empty-area path.
- **`on_motion`** already updates `page_drag`-style deltas outside arrange; ensure the arrange path (when there is no active icon `drag`) feeds the same page-drag delta so the page follows the finger.
- A **still tap** on empty area (no movement past the tap slop) still **exits** arrange (as today). The Done button still exits.
- Mid-drag (icon-drag) paging via edge-dwell flip is unchanged.

## Interaction summary

| Gesture | Result |
|---|---|
| Drag grid icon over grid | live gap at hovered slot; drop reorders (global) |
| Drag grid icon over dock | pin (append dock); grid repacks to backfill |
| Drag dock icon over grid | dock reflows on lift; live gap; drop inserts at hovered slot (move_to also removes it from the dock) |
| Drag dock icon over dock | snap back |
| Swipe empty area (arrange) | change page, stay in arrange |
| Tap empty area (arrange) | exit arrange |
| Done button | exit arrange |
| Edge-dwell during drag | flip page (unchanged) |

## Testing

Model (`sc-shell-model`):
- `repack`: flatten+re-chunk fills every page but the last; drops empty tail; a mid-list hole is backfilled from later pages; an over-`PAGE_CAP` page cascades forward. (Replaces the `normalize_pages` tests.)
- `move_to`: global-index insert across pages (e.g. move page-0 item to a page-1 index lands at the right global position and repacks). `move_to_reorders_within_page` (3 apps, one page) is **unaffected** — repack is a no-op below `PAGE_CAP`, so its `vec!["c","a","b"]` assertion still holds; keep it. Add a new cross-page move test.
- Delete the `normalize_pages` tests (`normalize_cascades_overflow_to_next_page`, `normalize_drops_empty_trailing_pages`); re-express their intent as `repack` tests.
- `pin`/`hide` backfill: removing a mid-grid app pulls the next app back so no interior hole remains.

Dispatch (`input_dispatch`):
- Dock-source-over-grid resolves to `DropAction::Reorder { page, index }` with the hovered slot (caller unpins); grid-over-dock still `Pin`; dock-over-dock still `SnapBack`. Replaces `resolve_drop_dock_over_grid_is_unpin`.

Compositor (unit where possible; interactive smoke for the rest):
- Working-order builder inserts a hole at the hovered index over the grid, and no hole over the dock zone.
- Interactive (run-springchick): dock lift reflows the dock; dock→grid drops at the hovered slot with a visible gap; grid→dock leaves no interior hole; swipe changes page in arrange while tap exits.

## Out of scope

- Search UI / consuming frecency (still write-only data).
- Reordering within the dock (dock is append-only on pin; dock→dock is snap-back).
