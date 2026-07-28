# Home Reorder Refinements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Polish the manual grid reorder — global hole-backfill via a `repack` model, a live visible drop-gap, dock drags symmetric with grid drags (animated dock), and page-switching while in edit mode.

**Architecture:** `pages` becomes a chunked flat order: one `repack` (flatten → re-chunk by `PAGE_CAP`) enforces the "only the last page is partial" invariant, giving overflow cascade *and* cross-page backfill for free. The live drag inserts a hole sentinel at the hovered global index so icons part to show the drop target. A parallel `dock_anim` spring set (mirroring `grid_anim`) makes the dock reflow. Edit-mode paging reuses the existing page-drag machinery, gated by restructured arrange-mode press/release.

**Tech Stack:** Rust workspace (`sc-shell-model`, `sc-compositor`), `sc-anim` springs, skia render. Build/test sc-compositor via `nix develop --command bash -c 'cargo …'` (plain/`+nightly` cargo fail in-sandbox). Interactive pieces via the `run-springchick` skill.

**Spec:** `docs/superpowers/specs/2026-07-28-reorder-refinements-design.md`

---

## File Structure

- `crates/sc-shell-model/src/lib.rs` — add `flat`, `repack`; rework `move_to` to global-index insert; `pin`/`hide` backfill via `repack`; delete `normalize_pages`; test cleanup.
- `crates/sc-compositor/src/main.rs` — `normalize_pages`→`repack` at `after_arrange_edit`; extract free `working_order` (hole sentinel); `reflow_grid` drops the hole; add `dock_anim` + `reflow_dock`; build `dock_positions` in `advance_frame`.
- `crates/sc-compositor/src/input_common.rs` — `normalize_pages`→`repack` fallback; unify Reorder drop for grid+dock source; split arrange `on_press` Miss arm; restructure arrange `on_release` to allow empty-area page-swipe/tap-exit.
- `crates/sc-compositor/src/input_dispatch.rs` — dock-over-grid → `DropAction::Reorder`; test replacement.
- `crates/sc-compositor/src/skia_gl.rs` — `draw_home` gains `dock_positions`; add `visible_dock_slots`.
- `crates/sc-compositor/src/render.rs` — thread `dock_positions` through `DrawCtx`/`draw_scene`.

> **Phase A→B build gap:** deleting `normalize_pages` in A leaves `sc-compositor` un-buildable (two callers) until B. Phase A runs only `cargo +nightly test -p sc-shell-model`; do not workspace-build between the A and B commits. Crate builds again at the end of B.

---

## Phase A — Model: repack + global-index move (`sc-shell-model`)

### Task A1: `flat` + `repack` replace `normalize_pages`

**Files:** Modify `crates/sc-shell-model/src/lib.rs`. TOOLCHAIN: `cargo +nightly test -p sc-shell-model`.

- [ ] **Step 1: Write failing tests** (delete the two `normalize_pages` tests `normalize_cascades_overflow_to_next_page`, `normalize_drops_empty_trailing_pages`)

```rust
#[test]
fn repack_backfills_interior_hole() {
    // 25 apps -> page0 full (24), page1 has 1. Remove one from page0's middle
    // by hand, then repack: page1's app must pull back so page0 stays full.
    let mut m = ShellModel {
        pages: vec![
            (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
            vec!["tail".into()],
        ],
        ..Default::default()
    };
    m.pages[0].remove(5); // interior hole -> page0 now 23
    m.repack();
    assert_eq!(m.pages[0].len(), PAGE_CAP); // backfilled from page1
    assert_eq!(m.pages[0][23], "tail");
    assert_eq!(m.pages.len(), 1); // page1 emptied + dropped
}

#[test]
fn repack_cascades_overflow_and_drops_empty_tail() {
    let mut m = ShellModel {
        pages: vec![(0..=PAGE_CAP).map(|i| format!("a{i}")).collect(), vec![], vec![]],
        ..Default::default()
    };
    m.repack();
    assert_eq!(m.pages[0].len(), PAGE_CAP);
    assert_eq!(m.pages[1], vec![format!("a{PAGE_CAP}")]);
    assert_eq!(m.pages.len(), 2); // empty tail pages gone
}
```

- [ ] **Step 2: Run** `cargo +nightly test -p sc-shell-model repack_` → FAIL (no `repack`).

- [ ] **Step 3: Implement** (add to `impl ShellModel`, delete `normalize_pages`)

```rust
    /// Flattened grid order (all pages concatenated).
    fn flat(&self) -> Vec<AppId> {
        self.pages.iter().flatten().cloned().collect()
    }

    /// Re-chunk the flattened order into PAGE_CAP-sized pages, dropping empty
    /// tail pages. The single packing invariant: every page but the last is
    /// full. A dense re-chunk can leave neither an interior hole nor an
    /// overflow, so this handles both backfill and overflow cascade.
    pub fn repack(&mut self) {
        let flat = self.flat();
        self.pages = flat.chunks(PAGE_CAP).map(|c| c.to_vec()).collect();
    }
```

- [ ] **Step 4: Run** `cargo +nightly test -p sc-shell-model` → all green.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): repack (chunked flat order) replaces normalize_pages"
```

### Task A2: `move_to` global-index insert

**Files:** Modify `crates/sc-shell-model/src/lib.rs`.

- [ ] **Step 1: Write failing test** (keep `move_to_reorders_within_page` — unaffected, repack is a no-op below PAGE_CAP)

```rust
#[test]
fn move_to_cross_page_lands_at_global_index() {
    let mut m = ShellModel {
        pages: vec![(0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(), vec!["x".into()]],
        ..Default::default()
    };
    // Move "x" to page 0, index 2 -> global index 2.
    m.move_to("x", 0, 2);
    assert_eq!(m.pages[0][2], "x");
    assert_eq!(m.pages[0].len(), PAGE_CAP); // repacked full
    assert_eq!(m.pages.len(), 1);
}

#[test]
fn move_to_from_dock_removes_from_dock() {
    let mut m = ShellModel::default();
    m.place("a".into());
    m.dock.push("d".into());
    m.move_to("d", 0, 0); // dock -> grid
    assert!(m.dock.is_empty());
    assert_eq!(m.pages[0], vec!["d", "a"]);
}
```

- [ ] **Step 2: Run** `cargo +nightly test -p sc-shell-model move_to_cross_page move_to_from_dock` → FAIL (old within-page semantics; dock not stripped).

- [ ] **Step 3: Implement** — replace `move_to` (and its `delete_keep_pages` helper is no longer needed by it; leave `delete_keep_pages` only if other callers use it — grep; if unused, delete it):

```rust
    /// Move `app` to the grid slot addressed by (page, index), treated as a
    /// global position `page*PAGE_CAP + index` in the flattened order. Removes
    /// `app` from pages/dock/hidden first, inserts, then repacks. Used by drag
    /// reorder (grid- and dock-sourced).
    pub fn move_to(&mut self, app: &str, page: usize, index: usize) {
        let mut flat: Vec<AppId> = self.flat().into_iter().filter(|a| a != app).collect();
        self.dock.retain(|a| a != app);
        self.hidden.retain(|a| a != app);
        let gi = page.saturating_mul(PAGE_CAP).saturating_add(index).min(flat.len());
        flat.insert(gi, app.to_string());
        self.pages = flat.chunks(PAGE_CAP).map(|c| c.to_vec()).collect();
    }
```

Run `grep -n delete_keep_pages crates/sc-shell-model/src/lib.rs` — if `move_to` was its only user, delete the helper.

- [ ] **Step 4: Run** `cargo +nightly test -p sc-shell-model` → green (incl. `move_to_reorders_within_page` unchanged).

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): move_to inserts at global index + repacks"
```

### Task A3: `pin`/`hide` backfill via `repack`

**Files:** Modify `crates/sc-shell-model/src/lib.rs`.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn pin_backfills_grid_across_pages() {
    // 25 grid apps -> page0 full, page1 has 1. Pin one from page0.
    let mut m = ShellModel {
        pages: vec![
            (0..PAGE_CAP).map(|i| format!("a{i:02}")).collect(),
            vec!["tail".into()],
        ],
        ..Default::default()
    };
    assert!(m.pin("a05"));
    assert!(m.dock.contains(&"a05".to_string()));
    assert_eq!(m.pages[0].len(), PAGE_CAP); // tail pulled back, no interior hole
    assert_eq!(m.pages.len(), 1);
}
```

- [ ] **Step 2: Run** → FAIL (current pin removes but doesn't backfill across pages).

- [ ] **Step 3: Implement** — after removing from pages, `repack`, in `pin` and `hide`:

```rust
    pub fn pin(&mut self, app: &str) -> bool {
        if self.dock.iter().any(|a| a == app) || self.dock.len() >= DOCK_CAP {
            return false;
        }
        self.remove_from_pages(app);
        self.repack();
        self.dock.push(app.to_owned());
        true
    }

    pub fn hide(&mut self, app: &str) {
        if !self.hidden.iter().any(|a| a == app) {
            self.remove_from_pages(app);
            self.repack();
            self.hidden.push(app.to_owned());
        }
    }
```

(`unpin`/`unhide` keep using `place`; the compositor `repack`s via `after_arrange_edit`.)

- [ ] **Step 4: Run** `cargo +nightly test -p sc-shell-model` → green.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): pin/hide repack to backfill grid globally"
```

---

## Phase B — Compositor call-site rename (restores build)

### Task B1: `normalize_pages` → `repack`

**Files:** Modify `crates/sc-compositor/src/main.rs`, `crates/sc-compositor/src/input_common.rs`. TOOLCHAIN: `nix develop --command bash -c 'cargo build -p sc-compositor'`.

- [ ] **Step 1: Rename both call sites**

`main.rs::after_arrange_edit`:
```rust
    fn after_arrange_edit(&mut self) {
        self.model.repack();
        if let Err(e) = config_state::save(&self.model, &config_path()) {
            warn!(%e, "failed to save shell model after arrange edit");
        }
        self.reflow_grid();
    }
```
`input_common.rs::on_release` no-edit fallback (`state.model.normalize_pages();` → `state.model.repack();`).

- [ ] **Step 2: Build** `nix develop --command bash -c 'cargo build -p sc-compositor'` → compiles (no more `normalize_pages`).

- [ ] **Step 3: Test** `nix develop --command bash -c 'cargo test -p sc-compositor'` → green.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/main.rs crates/sc-compositor/src/input_common.rs
git commit -m "feat(compositor): use repack in place of normalize_pages"
```

---

## Phase C — Live visible drop-gap

### Task C1: pure `working_order` with hole sentinel

**Files:** Modify `crates/sc-compositor/src/main.rs`. TOOLCHAIN: `nix develop --command bash -c 'cargo test -p sc-compositor'`.

- [ ] **Step 1: Write failing test** (add to main.rs test module; it uses `sc_shell_model::PAGE_CAP`)

```rust
    #[test]
    fn working_order_opens_hole_at_hover() {
        let pages = vec![vec!["a".to_string(), "b".into(), "c".into(), "d".into()]];
        // Drag "a", hover global index 2 -> order without "a" is [b,c,d];
        // hole at 2 -> [b, c, HOLE, d].
        let out = working_order(&pages, "a", Some((0, 2)));
        assert_eq!(out[0], vec!["b".to_string(), "c".into(), HOLE.to_string(), "d".into()]);
    }

    #[test]
    fn working_order_no_hole_when_hover_none() {
        let pages = vec![vec!["a".to_string(), "b".into(), "c".into()]];
        let out = working_order(&pages, "a", None);
        assert_eq!(out[0], vec!["b".to_string(), "c".into()]);
    }
```

- [ ] **Step 2: Run** → FAIL (no `working_order`/`HOLE`).

- [ ] **Step 3: Implement** — add a module const and a free fn near `reflow_targets`:

```rust
/// Sentinel occupying the gap slot in the drag working order. Never a real app
/// id (NUL-prefixed), so it can't collide; it is laid out for spacing but never
/// drawn (it is not in `model.pages`, and `reflow_grid` drops it from targets).
pub(crate) const HOLE: &str = "\u{0}hole";

/// The drag "working order": the flattened grid with `dragged` removed and, when
/// `hover` is Some, a HOLE sentinel inserted at the hovered global index so the
/// real icons part to show the drop target. Re-chunked into pages.
fn working_order(pages: &[Vec<String>], dragged: &str, hover: Option<(usize, usize)>) -> Vec<Vec<String>> {
    let mut flat: Vec<String> = pages.iter().flatten().filter(|a| *a != dragged).cloned().collect();
    if let Some((page, index)) = hover {
        let gi = (page.saturating_mul(sc_shell_model::PAGE_CAP).saturating_add(index)).min(flat.len());
        flat.insert(gi, HOLE.to_string());
    }
    flat.chunks(sc_shell_model::PAGE_CAP).map(|c| c.to_vec()).collect()
}
```

- [ ] **Step 4: Run** the two tests → PASS, then `nix develop --command bash -c 'cargo test -p sc-compositor'` → green.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): working_order opens a hole at the hovered slot"
```

### Task C2: `reflow_grid` uses `working_order`, drops HOLE

**Files:** Modify `crates/sc-compositor/src/main.rs`.

- [ ] **Step 1: Rewrite `working_pages` to delegate + `reflow_grid` to drop HOLE**

Replace `working_pages` body:
```rust
    fn working_pages(&self, dragged: &str, hover: Option<(usize, usize)>) -> Vec<Vec<String>> {
        working_order(&self.model.pages, dragged, hover)
    }
```
In `reflow_grid`, after building `targets` for the drag branch, remove the sentinel so no spring is created for it:
```rust
            Some(drag) => {
                let working = self.working_pages(&drag.app_id, drag.hover);
                let mut t = reflow_targets_for(&working, w, h);
                t.remove(HOLE);
                t
            }
```
(The real icons already carry their hole-shifted positions; `grid_anim.retain(|a,_| targets.contains_key(a))` then naturally never keeps HOLE.)

- [ ] **Step 2: Build + test** `nix develop --command bash -c 'cargo test -p sc-compositor'` → green.

- [ ] **Step 3: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): reflow the hole working order, drop the sentinel spring"
```

---

## Phase D — Dock↔grid drop symmetry (dispatch + release)

### Task D1: dock-over-grid → `Reorder`

**Files:** Modify `crates/sc-compositor/src/input_dispatch.rs`.

- [ ] **Step 1: Replace the test** `resolve_drop_dock_over_grid_is_unpin` with:

```rust
    #[test]
    fn resolve_drop_dock_over_grid_is_reorder() {
        let (w, h) = (1224.0, 2700.0);
        let l = sc_layout::compute(w, h, 0, &ShellModel::default());
        let p = sc_layout::global_slot_pos(0, 1, w, h);
        assert_eq!(
            resolve_drop(p.0, p.1, &l, IconSource::Dock, 0, 3, w, h),
            DropAction::Reorder { page: 0, index: 1 },
        );
    }
```

- [ ] **Step 2: Run** `nix develop --command bash -c 'cargo test -p sc-compositor resolve_drop'` → FAIL (dock-over-grid still returns `Unpin`).

- [ ] **Step 3: Implement** — in `resolve_drop`, make the grid target apply to Dock source too. Change the match so `(Grid|Dock, false)` over grid both build `Reorder`:

```rust
    let over_dock = layout.dock_zone.contains(x, y);
    match (source, over_dock) {
        (IconSource::Grid, true) => DropAction::Pin,
        (IconSource::Dock, true) => DropAction::SnapBack, // dock over dock
        (_, false) => {
            let idx = sc_layout::nearest_grid_index(w, h, x, y).min(page_len);
            DropAction::Reorder { page, index: idx }
        }
    }
```
`DropAction::Unpin` becomes unused — remove the variant and any remaining reference. (grep `Unpin`.)

- [ ] **Step 4: Run** `nix develop --command bash -c 'cargo test -p sc-compositor'` → green. (The on_release `Unpin` arm is removed in D2; if the crate doesn't build until then because of a stray `Unpin` match arm, do D2 in the same edit session before testing — see note.)

> **Note:** removing the `Unpin` variant breaks the `on_release` match arm that names it. Do D2's arm edit together with D1 so the crate compiles, then run tests.

- [ ] **Step 5: Commit** (with D2)

### Task D2: unify the Reorder drop for grid + dock source

**Files:** Modify `crates/sc-compositor/src/input_common.rs`.

- [ ] **Step 1: Rewrite the drop match** so `Reorder` handles both sources with a single `move_to` (which strips the app from the dock for a dock source) and drop the `Unpin` arm:

```rust
            let edited = match input_dispatch::resolve_drop(
                drag.cur.0, drag.cur.1, &layout, drag.source, page, page_len, w, h,
            ) {
                input_dispatch::DropAction::Pin => state.model.pin(&drag.app_id),
                input_dispatch::DropAction::Reorder { page, index } => {
                    let ix = drag.hover.map_or(index, |h| h.1);
                    // move_to removes the app from the dock too (dock->grid) and
                    // from its old grid slot (grid->grid), then repacks.
                    state.model.move_to(&drag.app_id, page, ix);
                    true
                }
                input_dispatch::DropAction::SnapBack => false,
            };
```

- [ ] **Step 2: Build + test** `nix develop --command bash -c 'cargo test -p sc-compositor'` → green.

- [ ] **Step 3: Commit**

```bash
git add crates/sc-compositor/src/input_dispatch.rs crates/sc-compositor/src/input_common.rs
git commit -m "feat(compositor): dock->grid drops at hovered slot (unified Reorder)"
```

---

## Phase E — Animated dock (`dock_anim`)

### Task E1: `dock_anim` state + `reflow_dock`

**Files:** Modify `crates/sc-compositor/src/main.rs`.

- [ ] **Step 1: Add the field + reflow** — mirror `grid_anim`. Add `dock_anim: std::collections::HashMap<String,(sc_anim::Spring, sc_anim::Spring)>` to `State` (init empty in `State::new`). Add:

```rust
    /// Retarget dock springs to the current dock layout, dropping a dock icon
    /// that is being dragged (it rides as the ghost). Mirror of `reflow_grid`.
    fn reflow_dock(&mut self) {
        let (w, h) = self.output_size_f();
        let dragged = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .filter(|d| d.source == input_dispatch::IconSource::Dock)
            .map(|d| d.app_id.clone());
        let layout = sc_layout::compute(w, h, 0, &self.model);
        let mut targets: std::collections::HashMap<String, (f32, f32)> = std::collections::HashMap::new();
        for slot in &layout.dock {
            if Some(&slot.app_id) == dragged.as_ref() {
                continue;
            }
            targets.insert(slot.app_id.clone(), (slot.icon_rect.center_x(), slot.icon_rect.center_y()));
        }
        for (app, (tx, ty)) in &targets {
            match self.dock_anim.get_mut(app) {
                Some((sx, sy)) => { sx.retarget(*tx); sy.retarget(*ty); }
                None => { self.dock_anim.insert(app.clone(), (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty))); }
            }
        }
        self.dock_anim.retain(|app, _| targets.contains_key(app));
    }
```
Call `self.reflow_dock()` wherever `self.reflow_grid()` is called (`after_arrange_edit`, the lazy seed, and the per-frame drag retarget in `advance_frame`), and step the dock springs alongside the grid springs in `advance_frame`:
```rust
        for (sx, sy) in self.dock_anim.values_mut() { sx.step(dt); sy.step(dt); }
```
Lazy-seed: `if self.dock_anim.is_empty() { self.reflow_dock(); }` next to the grid one.

- [ ] **Step 2: Build + test** → green (no behavior change yet; springs just track the static dock).

- [ ] **Step 3: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): dock_anim springs + reflow_dock"
```

### Task E2: render the dock from `dock_anim`

**Files:** Modify `crates/sc-compositor/src/main.rs`, `render.rs`, `skia_gl.rs`.

- [ ] **Step 1: Thread `dock_positions`** — in `advance_frame`, build it like `grid_positions`:
```rust
    let dock_positions: std::collections::HashMap<String, (f32, f32)> =
        state.dock_anim.iter().map(|(a, (sx, sy))| (a.clone(), (sx.value, sy.value))).collect();
```
Add `pub dock_positions: &'a HashMap<String,(f32,f32)>` to `DrawCtx` (render.rs), pass it in the ctx literal, and forward to `draw_home` (render.rs `ctx.dock_positions`).

- [ ] **Step 2: `draw_home` + `visible_dock_slots`** (skia_gl.rs) — add a `dock_positions: &HashMap<String,(f32,f32)>` param to `draw_home`; replace the `for slot in &current_layout.dock` draw loop's source with animated slots:
```rust
        let dock_slots = visible_dock_slots(model, dock_positions, width as f32, height as f32, page);
        for slot in &dock_slots { draw_icon_slot(...); }
```
where `visible_dock_slots` builds an `IconSlot` per dock app from its `dock_positions` center (fall back to the static `layout.dock` center if absent, so a not-yet-seeded app still draws). Model the helper on `visible_grid_slots` + `slot_at_center`. Also update the arrange-mode badge loop (`anim_slots.iter().chain(current_layout.dock.iter())`) to chain `dock_slots` instead of `current_layout.dock`.

- [ ] **Step 3: Build + interactive check** — `nix develop --command bash -c 'cargo build -p sc-compositor'`; then via run-springchick confirm the dock still renders in place, and lifting a dock icon in arrange mode makes the remaining dock icons slide to re-center.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/main.rs crates/sc-compositor/src/render.rs crates/sc-compositor/src/skia_gl.rs
git commit -m "feat(compositor): render dock from dock_anim (reflows on lift)"
```

---

## Phase F — Edit-mode paging

### Task F1: swipe pages / tap exits in arrange

**Files:** Modify `crates/sc-compositor/src/input_common.rs`.

- [ ] **Step 1: Split the `on_press` arrange Miss arm** — replace
```rust
            sc_layout::Hit::DoneButton | sc_layout::Hit::Miss | sc_layout::Hit::Bar => {
                state.arrange = None;
            }
```
with
```rust
            sc_layout::Hit::DoneButton | sc_layout::Hit::Bar => {
                state.arrange = None;
            }
            sc_layout::Hit::Miss => {
                // Empty-area press in arrange: arm a page drag; a swipe pages,
                // a still tap (resolved in on_release) exits.
                state.page_drag_start = Some(x);
            }
```
(`on_motion` already advances the page spring from `page_drag_start` when there's no active icon `drag`, so paging follows the finger without further change.)

- [ ] **Step 2: Restructure `on_release` arrange block** so it does NOT swallow the empty-area gesture. Change:
```rust
    if let Some(arrange) = &mut state.arrange {
        if let Some(drag) = arrange.drag.take() {
            ... // drop resolution
        }
        return;
    }
```
to only early-return on the drag path; on the no-drag path, resolve page-swipe vs tap-exit and then return:
```rust
    if state.arrange.is_some() {
        if let Some(drag) = state.arrange.as_mut().and_then(|a| a.drag.take()) {
            ... // existing drop resolution (unchanged)
            return;
        }
        // Empty-area release in arrange: a swipe commits a page flip (stay in
        // arrange); a still tap exits.
        if let Some(start_x) = state.page_drag_start.take() {
            let dx = x - start_x;
            let w = state.output_size.0 as f32;
            if dx.abs() > w * 0.15 {
                let page_delta = -dx / w;
                if let UiState::Home { page, page_spring, page_count, .. } = &mut state.ui {
                    let target = if page_delta > 0.3 && *page + 1 < *page_count { *page + 1 }
                        else if page_delta < -0.3 && *page > 0 { *page - 1 }
                        else { *page };
                    *page = target;
                    page_spring.retarget(target as f32);
                }
            } else {
                state.arrange = None; // still tap -> exit
            }
        } else {
            state.arrange = None;
        }
        return;
    }
```
(The `w*0.15` swipe/tap split matches the bar-drag threshold used elsewhere; the `0.3` page-commit threshold matches the existing page-swipe commit.)

- [ ] **Step 3: Build + interactive check** — via run-springchick: in arrange mode, swipe left/right on empty area changes page and stays in arrange; a tap on empty exits; Done exits.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/input_common.rs
git commit -m "feat(compositor): swipe pages in arrange mode, tap exits"
```

---

## Phase G — Verification

### Task G1: workspace green + interactive pass

- [ ] **Step 1** `nix develop --command bash -c 'cargo test --workspace'` → all green. Fix any config/round-trip test that assumed old packing.
- [ ] **Step 2** `nix develop --command bash -c 'cargo clippy --workspace --all-targets'` → no new warnings (allow/adjust as in the prior branch).
- [ ] **Step 3 — interactive (run-springchick), confirm each refinement:**
  - Drag a grid icon over others → a **gap opens** at the hovered slot (not just compaction); drop lands there.
  - Pin an interior grid app to the dock → grid **backfills** (next app pulls up, no lingering hole).
  - Lift an app **out of the dock** → dock reflows; drag over grid shows the gap; drop lands at the hovered slot.
  - In arrange mode, **swipe** changes page (stays in arrange); **tap** empty exits.
- [ ] **Step 4** If a non-obvious fix was needed (borrow dance, hole/spring interaction, dock fallback), note it in project memory.
- [ ] **Step 5** superpowers:finishing-a-development-branch.

---

## Notes / gotchas

- HOLE sentinel must never be persisted or drawn: it only lives in the transient `working_order` and is dropped from `reflow_grid` targets. `model.pages` never contains it.
- `move_to`'s global index tolerates `page` past the last page (edge-dwell new page): `page*PAGE_CAP` clamps to `flat.len()` → append. Keep the edge-dwell empty-page push (page nav depends on it).
- Dock has ≤4 icons; `dock_anim` is tiny. `visible_dock_slots` must fall back to the static layout center for an unseeded app so the first frame isn't blank.
- `repack` is the single packing authority — every model mutation ends by repacking (directly or via `after_arrange_edit`).
