# Manual Home-Grid Reorder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace frecency-driven auto-ordering of the home grid with a persisted, manually-reorderable grid — drag an icon (in arrange mode) to lift it out, reflow the rest, and drop to reinsert, including across pages.

**Architecture:** `pages` becomes persisted source of truth (restores pre-frecency `b4136d7` shape); `FrecencyStore` is kept as recorded-but-unused data for future search. Reorder rides on the existing arrange-mode drag: a new `DropAction::Reorder` plus a live "working order" (dragged app removed, hole at the hovered slot) fed into the existing `grid_anim` reflow springs. Cross-page drag flips pages on edge-dwell; `normalize_pages` keeps persisted pages within `PAGE_CAP`.

**Tech Stack:** Rust workspace (`sc-shell-model`, `sc-layout`, `sc-compositor`), `sc-anim` springs, skia render, serde/TOML config. Interactive pieces verified via the `run-springchick` skill.

**Spec:** `docs/superpowers/specs/2026-07-28-manual-grid-reorder-design.md`

---

## File Structure

- `crates/sc-shell-model/src/lib.rs` — model: un-skip `pages`, add `reconcile` + `normalize_pages`, make `pin/unpin/hide/unhide` mutate `pages`, delete `recompute_pages`; test cleanup.
- `crates/sc-layout/src/lib.rs` — add `nearest_grid_index`.
- `crates/sc-compositor/src/input_dispatch.rs` — `DropAction::Reorder`, extend `resolve_drop` signature.
- `crates/sc-compositor/src/main.rs` — startup uses `reconcile`; `launch_or_raise` saves + drops page-follow; delete `landed_origin`; `after_arrange_edit` drops recompute + calls `normalize_pages`; `reflow_targets` gains working-order variant; per-frame drag reflow; `DragItem.hover`.
- `crates/sc-compositor/src/input_common.rs` — `on_motion` updates `hover`; `on_release` resolves grid→grid to `move_to`; edge-dwell page flip.

---

## Phase A — Model layer (`sc-shell-model`)

Pure, fully unit-testable. Do this first.

### Task A1: Persist `pages`

**Files:**
- Modify: `crates/sc-shell-model/src/lib.rs` (the `ShellModel` struct, ~63-74)
- Test: same file `#[cfg(test)]`

- [ ] **Step 1: Write the failing test** (replaces `pages_not_serialized_frecency_is`, which asserts the OLD behavior — delete that test)

```rust
#[test]
fn pages_round_trip_through_serde() {
    let mut m = ShellModel::default();
    m.place("a".into());
    m.place("b".into());
    let s = toml::to_string(&m).unwrap();
    let back: ShellModel = toml::from_str(&s).unwrap();
    assert_eq!(back.pages, m.pages);
}

#[test]
fn old_file_without_pages_loads_empty() {
    // A config written before pages were persisted: only dock + frecency.
    let s = "dock = []\n[frecency]\n";
    let m: ShellModel = toml::from_str(s).unwrap();
    assert!(m.pages.is_empty());
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-shell-model pages_round_trip_through_serde old_file_without_pages_loads_empty`
Expected: FAIL — `pages` is `#[serde(skip)]`, so it round-trips empty and the first test fails.

- [ ] **Step 3: Change the field attribute**

In `ShellModel`, replace:
```rust
    #[serde(skip)]
    pub pages: Vec<Vec<AppId>>,
```
with:
```rust
    #[serde(default)]
    pub pages: Vec<Vec<AppId>>,
```
Delete the now-stale `pages_not_serialized_frecency_is` test.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p sc-shell-model pages_round_trip_through_serde old_file_without_pages_loads_empty`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): persist pages (manual grid order)"
```

### Task A2: `reconcile` replaces `recompute_pages`

**Files:**
- Modify: `crates/sc-shell-model/src/lib.rs` (delete `recompute_pages` ~79-91; add `reconcile`)
- Test: same file

- [ ] **Step 1: Write the failing tests** (also delete the four `recompute_pages_*` tests and `launch_promotes_app_to_front_of_grid`)

```rust
#[test]
fn reconcile_appends_new_catalog_ids_in_order() {
    let mut m = ShellModel::default();
    m.place("b".into());
    // catalog gained "a" and "c"; "b" already placed must stay put.
    m.reconcile(&["a".into(), "b".into(), "c".into()], 0, false);
    assert_eq!(m.pages[0], vec!["b", "a", "c"]);
}

#[test]
fn reconcile_prunes_uninstalled_from_pages_dock_hidden() {
    let mut m = ShellModel::default();
    m.place("gone".into());
    m.place("keep".into());
    m.dock.push("dgone".into());
    m.hidden.push("hgone".into());
    m.reconcile(&["keep".into()], 0, false);
    assert_eq!(m.pages, vec![vec!["keep".to_string()]]);
    assert!(m.dock.is_empty());
    assert!(m.hidden.is_empty());
}

#[test]
fn reconcile_seeds_frecency_for_new_apps() {
    let mut m = ShellModel::default();
    // Later install (store non-empty at bootstrap) seeds score 1.0.
    m.frecency.apps.insert("existing".into(), AppStat { score: 5.0, last_launch: 0 });
    m.reconcile(&["existing".into(), "new".into()], 100, false);
    assert_eq!(m.frecency.apps["new"].score, 1.0);
    assert_eq!(m.frecency.apps["new"].last_launch, 100);
}

#[test]
fn reconcile_does_not_move_already_placed() {
    let mut m = ShellModel::default();
    for n in ["a", "b", "c"] { m.place(n.into()); }
    m.move_to("c", 0, 0); // user order: c, a, b
    m.reconcile(&["a".into(), "b".into(), "c".into()], 0, false);
    assert_eq!(m.pages[0], vec!["c", "a", "b"]);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-shell-model reconcile_`
Expected: FAIL — `reconcile` does not exist.

- [ ] **Step 3: Implement `reconcile`, delete `recompute_pages`**

Delete `recompute_pages`. Add to `impl ShellModel`:
```rust
    /// Keep `pages` in sync with the installed catalog without reordering
    /// existing slots. Appends catalog ids not yet in pages/dock/hidden (via
    /// `place`), seeds their frecency, and prunes ids no longer installed.
    /// `now`/`first_run` mirror the old startup seed loop (score 0 on a
    /// first-run empty store, 1.0 for a later install).
    pub fn reconcile(&mut self, catalog_ids: &[AppId], now: u64, first_run: bool) {
        // Prune uninstalled from every surface.
        self.pages.iter_mut().for_each(|p| p.retain(|a| catalog_ids.contains(a)));
        self.pages.retain(|p| !p.is_empty());
        self.dock.retain(|a| catalog_ids.contains(a));
        self.hidden.retain(|a| catalog_ids.contains(a));
        self.frecency.prune(catalog_ids);
        // Append + seed new ones, in catalog (sorted) order.
        for id in catalog_ids {
            let known = self.pages.iter().any(|p| p.contains(id))
                || self.dock.contains(id)
                || self.hidden.contains(id);
            if !known {
                self.place(id.clone());
            }
            self.frecency.seed(id, now, first_run);
        }
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p sc-shell-model reconcile_`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): reconcile replaces frecency recompute"
```

### Task A3: `pin/unpin/hide/unhide` mutate `pages`

**Files:**
- Modify: `crates/sc-shell-model/src/lib.rs` (~130-154)
- Test: same file

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn pin_removes_from_pages_unpin_restores() {
    let mut m = ShellModel::default();
    m.place("a".into());
    assert!(m.pin("a"));
    assert!(!m.pages.iter().any(|p| p.contains(&"a".to_string())));
    assert!(m.dock.contains(&"a".to_string()));
    m.unpin("a");
    assert!(!m.dock.contains(&"a".to_string()));
    assert!(m.pages.iter().any(|p| p.contains(&"a".to_string())));
}

#[test]
fn hide_removes_from_pages_unhide_restores() {
    let mut m = ShellModel::default();
    m.place("a".into());
    m.hide("a");
    assert!(!m.pages.iter().any(|p| p.contains(&"a".to_string())));
    assert!(m.hidden.contains(&"a".to_string()));
    m.unhide("a");
    assert!(!m.hidden.contains(&"a".to_string()));
    assert!(m.pages.iter().any(|p| p.contains(&"a".to_string())));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-shell-model pin_removes hide_removes`
Expected: FAIL — pin/hide don't touch `pages` (previously `recompute_pages` did the sync).

- [ ] **Step 3: Implement**

```rust
    pub fn pin(&mut self, app: &str) -> bool {
        if self.dock.iter().any(|a| a == app) || self.dock.len() >= DOCK_CAP {
            return false;
        }
        self.remove_from_pages(app);
        self.dock.push(app.to_owned());
        true
    }

    pub fn unpin(&mut self, app: &str) {
        if self.dock.iter().any(|a| a == app) {
            self.dock.retain(|a| a != app);
            self.place(app.to_owned());
        }
    }

    pub fn hide(&mut self, app: &str) {
        if !self.hidden.iter().any(|a| a == app) {
            self.remove_from_pages(app);
            self.hidden.push(app.to_owned());
        }
    }

    pub fn unhide(&mut self, app: &str) {
        if self.hidden.iter().any(|a| a == app) {
            self.hidden.retain(|a| a != app);
            self.place(app.to_owned());
        }
    }

    fn remove_from_pages(&mut self, app: &str) {
        for page in &mut self.pages { page.retain(|a| a != app); }
        self.pages.retain(|p| !p.is_empty());
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p sc-shell-model`
Expected: PASS (full model suite green)

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): pin/unpin/hide/unhide mutate pages directly"
```

### Task A4: `normalize_pages`

**Files:**
- Modify: `crates/sc-shell-model/src/lib.rs`
- Test: same file

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn normalize_cascades_overflow_to_next_page() {
    let mut m = ShellModel::default();
    m.pages = vec![(0..=PAGE_CAP).map(|i| format!("a{i}")).collect()]; // PAGE_CAP+1 on one page
    m.normalize_pages();
    assert_eq!(m.pages[0].len(), PAGE_CAP);
    assert_eq!(m.pages[1], vec![format!("a{PAGE_CAP}")]);
}

#[test]
fn normalize_drops_empty_trailing_pages() {
    let mut m = ShellModel::default();
    m.pages = vec![vec!["a".into()], vec![], vec![]];
    m.normalize_pages();
    assert_eq!(m.pages, vec![vec!["a".to_string()]]);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-shell-model normalize_`
Expected: FAIL — no `normalize_pages`.

- [ ] **Step 3: Implement**

```rust
    /// Keep every page within PAGE_CAP by cascading overflow onto the next page
    /// (creating one if needed), and drop empty pages. Called after any reorder.
    pub fn normalize_pages(&mut self) {
        let mut i = 0;
        while i < self.pages.len() {
            if self.pages[i].len() > PAGE_CAP {
                let overflow: Vec<AppId> = self.pages[i].split_off(PAGE_CAP);
                if i + 1 == self.pages.len() {
                    self.pages.push(Vec::new());
                }
                // prepend overflow to the front of the next page
                let next = &mut self.pages[i + 1];
                for (k, app) in overflow.into_iter().enumerate() {
                    next.insert(k, app);
                }
            }
            i += 1;
        }
        self.pages.retain(|p| !p.is_empty());
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p sc-shell-model`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/src/lib.rs
git commit -m "feat(shell-model): normalize_pages overflow cascade"
```

---

## Phase B — Layout helper (`sc-layout`)

### Task B1: `nearest_grid_index`

**Files:**
- Modify: `crates/sc-layout/src/lib.rs` (near `global_slot_pos`)
- Test: same file

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn nearest_grid_index_maps_and_clamps() {
    let (w, h) = (1224.0, 2700.0);
    // A point inside the first cell -> index 0.
    let p0 = global_slot_pos(0, 0, w, h);
    assert_eq!(nearest_grid_index(w, h, p0.0, p0.1), 0);
    // Second column, first row -> index 1.
    let p1 = global_slot_pos(0, 1, w, h);
    assert_eq!(nearest_grid_index(w, h, p1.0, p1.1), 1);
    // Far off-screen clamps into range.
    assert!(nearest_grid_index(w, h, -9999.0, -9999.0) < PAGE_CAP);
    assert!(nearest_grid_index(w, h, 9e9, 9e9) < PAGE_CAP);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-layout nearest_grid_index_maps_and_clamps`
Expected: FAIL — function undefined.

- [ ] **Step 3: Implement** (note: `global_slot_pos` args are page-global; pass a **screen-space** point, i.e. current page, since `nearest_grid_index` treats x within `[0,width)`)

```rust
/// Slot index (0..PAGE_CAP) whose cell is nearest the on-screen point (x, y)
/// for the currently visible page. Clamps to the grid; callers further clamp
/// to the page's fill length. `x` is screen-space (0..width), not page-global.
pub fn nearest_grid_index(width: f32, height: f32, x: f32, y: f32) -> usize {
    let gm = grid_metrics(width, height);
    let col = (((x - gm.grid_left) / gm.cell_w).floor() as isize)
        .clamp(0, COLS as isize - 1) as usize;
    let row = (((y - gm.grid_top) / gm.cell_h).floor() as isize)
        .clamp(0, ROWS as isize - 1) as usize;
    row * COLS + col
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p sc-layout nearest_grid_index_maps_and_clamps`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/sc-layout/src/lib.rs
git commit -m "feat(layout): nearest_grid_index for drop-slot resolution"
```

---

## Phase C — Drop resolution (`input_dispatch`)

### Task C1: `DropAction::Reorder` + extended `resolve_drop`

**Files:**
- Modify: `crates/sc-compositor/src/input_dispatch.rs` (~26-42)
- Modify: `crates/sc-compositor/src/input_common.rs` (~279 — the only production caller; must be updated in THIS task or the bin won't compile for the test run)
- Test: same file (replace `resolve_drop_grid_over_grid_is_snapback`)

> **Note:** Phase A already left `sc-compositor` un-buildable (recompute_pages). If executing C1 before Phase D, its `cargo test -p sc-compositor` runs will still fail to compile for that reason — run C1's tests *after* D2, or temporarily land D1+D2 first. The point of this task's caller update is that it does not add a *new* break.

- [ ] **Step 1: Write the failing tests** (delete `resolve_drop_grid_over_grid_is_snapback`)

```rust
#[test]
fn resolve_drop_grid_over_grid_is_reorder() {
    let (w, h) = (1224.0, 2700.0);
    let l = sc_layout::compute(w, h, 0, &ShellModel::default());
    // A point over the grid (not the dock zone), page 0, page_len 5.
    let p = sc_layout::global_slot_pos(0, 2, w, h);
    assert_eq!(
        resolve_drop(p.0, p.1, &l, IconSource::Grid, 0, 5, w, h),
        DropAction::Reorder { page: 0, index: 2 },
    );
}

#[test]
fn resolve_drop_reorder_clamps_to_page_len() {
    let (w, h) = (1224.0, 2700.0);
    let l = sc_layout::compute(w, h, 0, &ShellModel::default());
    let p = sc_layout::global_slot_pos(0, 10, w, h); // slot 10
    // Only 3 icons on the page -> append at index 3.
    assert_eq!(
        resolve_drop(p.0, p.1, &l, IconSource::Grid, 0, 3, w, h),
        DropAction::Reorder { page: 0, index: 3 },
    );
}
```

Keep `resolve_drop_grid_over_dock_is_pin` / `resolve_drop_dock_over_grid_is_unpin`, updating their calls with the two new args (`, 0, 0`).

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p sc-compositor --bin sc-compositor resolve_drop_`
Expected: FAIL — signature/variant mismatch.

- [ ] **Step 3: Implement**

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropAction {
    Pin,
    Unpin,
    Reorder { page: usize, index: usize },
    SnapBack,
}

/// Decide what a drop at (x,y) means. `page` is the visible Home page and
/// `page_len` its filled icon count; `(w, h)` is the output size (Layout has
/// no size fields, so pass them through for nearest-slot mapping).
pub fn resolve_drop(
    x: f32,
    y: f32,
    layout: &sc_layout::Layout,
    source: IconSource,
    page: usize,
    page_len: usize,
    w: f32,
    h: f32,
) -> DropAction {
    let over_dock = layout.dock_zone.contains(x, y);
    match (source, over_dock) {
        (IconSource::Grid, true) => DropAction::Pin,
        (IconSource::Dock, false) => DropAction::Unpin,
        (IconSource::Grid, false) => {
            let idx = sc_layout::nearest_grid_index(w, h, x, y).min(page_len);
            DropAction::Reorder { page, index: idx }
        }
        _ => DropAction::SnapBack,
    }
}
```

`Layout` has no `width`/`height` fields (verified), so `(w, h)` are explicit args. Update the two Pin/Unpin tests' calls to pass `, 0, 0, w, h` (add `let (w, h) = (1224.0, 2700.0);` bindings — those tests inline the literals in `compute(...)` and have no `w`/`h` locals) and the Reorder tests as shown.

- [ ] **Step 3b: Update the production caller (same task, keeps the bin compiling)**

`resolve_drop`'s only non-test caller is `input_common.rs:279` (arrange release), still passing 4 args. Thread the new args and add a **stub** `Reorder` arm (real `move_to` wiring lands in E1):
```rust
            let page = if let UiState::Home { page, .. } = &state.ui { *page } else { 0 };
            let page_len = state.model.pages.get(page).map_or(0, |p| p.len());
            let layout = sc_layout::compute(w, h, page, &state.model);
            match input_dispatch::resolve_drop(drag.cur.0, drag.cur.1, &layout, drag.source, page, page_len, w, h) {
                input_dispatch::DropAction::Pin => {
                    if state.model.pin(&drag.app_id) { state.after_arrange_edit(); }
                }
                input_dispatch::DropAction::Unpin => {
                    state.model.unpin(&drag.app_id);
                    state.after_arrange_edit();
                }
                input_dispatch::DropAction::Reorder { .. } => {} // wired in E1
                input_dispatch::DropAction::SnapBack => {}
            }
```
(E1 replaces the stub arm; it does not re-thread the args.)

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p sc-compositor --bin sc-compositor resolve_drop_`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/input_dispatch.rs
git commit -m "feat(input): DropAction::Reorder + page-aware resolve_drop"
```

---

## Phase D — Startup + launch wiring (`main.rs`)

Not unit-testable in isolation; verified by compile + existing tests + a `run-springchick` smoke test at the end of the phase.

> **Phase A→D build gap:** deleting `recompute_pages` in A2 leaves `sc-compositor` un-buildable (it still calls it at startup, in `after_arrange_edit`, and in a test) until D1+D2 land. Phase A only runs `cargo test -p sc-shell-model`, so this is expected — do NOT run a workspace build between the A and D commits. The crate builds green again at the end of D2.

### Task D1: Startup uses `reconcile` + fix stale compositor test

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (~481-491 startup; ~1658 test `reflow_targets_maps_pages_and_excludes_dock`)

- [ ] **Step 0: Rewrite the stale test that calls `recompute_pages`**

`reflow_targets_maps_pages_and_excludes_dock` (main.rs:1658) calls `m.recompute_pages(&catalog, 0)`, deleted in A2. The test asserts a page-1 app has `x > 1224.0`, so it needs > `PAGE_CAP` apps across two pages. Replace the frecency-record loop + `recompute_pages` with direct `place` calls:
```rust
    fn reflow_targets_maps_pages_and_excludes_dock() {
        let mut m = ShellModel::default();
        for i in 0..25 { m.place(format!("app{i:02}")); } // 24 on page 0, 1 on page 1
        let t = reflow_targets(&m, 1224.0, 2700.0);
        assert_eq!(t.len(), 25);
        let page1_app = &m.pages[1][0];
        assert!(t[page1_app].0 > 1224.0);
        let page0_app = &m.pages[0][0];
        assert!(t[page0_app].0 < 1224.0);
    }
```
(The old test's "excludes dock" name is now covered by `reflow_targets` naturally skipping docked apps — they were never in `pages`; keep or trim the name as preferred.)

- [ ] **Step 1: Replace the seed/prune/recompute block**

Replace:
```rust
        let mut model = model;
        let now = unix_now();
        let mut catalog_ids: Vec<String> = app_catalog.keys().cloned().collect();
        catalog_ids.sort();
        let first_run = model.frecency.apps.is_empty();
        for id in &catalog_ids {
            model.frecency.seed(id, now, first_run);
        }
        model.frecency.prune(&catalog_ids);
        model.recompute_pages(&catalog_ids, now);
```
with:
```rust
        let mut model = model;
        let now = unix_now();
        let mut catalog_ids: Vec<String> = app_catalog.keys().cloned().collect();
        catalog_ids.sort(); // deterministic seeding + first-run alpha order
        let first_run = model.frecency.apps.is_empty();
        model.reconcile(&catalog_ids, now, first_run);
```

- [ ] **Step 2: Build**

Run: `cargo build -p sc-compositor`
Expected: compiles (no more `recompute_pages`).

- [ ] **Step 3: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): reconcile model at startup"
```

### Task D2: `after_arrange_edit` drops recompute, adds normalize

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (~593-602)

- [ ] **Step 1: Rewrite the body**

Replace:
```rust
    fn after_arrange_edit(&mut self) {
        let now = unix_now();
        let mut catalog_ids: Vec<String> = self.app_catalog.keys().cloned().collect();
        catalog_ids.sort();
        self.model.recompute_pages(&catalog_ids, now);
        if let Err(e) = config_state::save(&self.model, &config_path()) {
            warn!(%e, "failed to save shell model after arrange edit");
        }
        self.reflow_grid();
    }
```
with:
```rust
    /// Persist + reflow after a manual grid/dock edit (pin/unpin/hide/reorder).
    /// No frecency recompute — grid order is now manual.
    fn after_arrange_edit(&mut self) {
        self.model.normalize_pages();
        if let Err(e) = config_state::save(&self.model, &config_path()) {
            warn!(%e, "failed to save shell model after arrange edit");
        }
        self.reflow_grid();
    }
```

- [ ] **Step 2: Build**

Run: `cargo build -p sc-compositor`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): after_arrange_edit normalizes instead of recomputing"
```

### Task D3: `launch_or_raise` saves + drops page-follow; delete `landed_origin`

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (`launch_or_raise` ~625-662, `landed_origin` ~388-401)
- Test: delete `landed_origin_some_for_grid_none_for_absent` (~1675)

- [ ] **Step 1: Rewrite launch prologue**

Replace the block that starts at `// Record usage for frecency...` through the end of the `if !already_running { ... }` page-follow section with:
```rust
        // Record usage for frecency (data for future search only — the grid no
        // longer reorders on launch). Persist directly: after_arrange_edit was
        // previously the only launch-time save, and a phone shell is usually
        // killed, not cleanly exited.
        let now = unix_now();
        self.model.frecency.record_launch(app_id, now);
        if let Err(e) = config_state::save(&self.model, &config_path()) {
            warn!(%e, "failed to save shell model after launch");
        }
```
Delete the `let already_running = ...`, `let (w, h) = self.output_size_f();`, and the whole `if !already_running { if let Some(o) = landed_origin(...) { ... } }` block. The `origin` passed into `launch_or_raise` is now the zoom origin unchanged.

- [ ] **Step 2: Delete `landed_origin`**

Remove the `fn landed_origin(...)` (~388-401) and its test `landed_origin_some_for_grid_none_for_absent` (~1675).

- [ ] **Step 3: Build + test**

Run: `cargo build -p sc-compositor && cargo test -p sc-compositor`
Expected: compiles; suite green (dead-code warning for `landed_origin` gone).

- [ ] **Step 4: Smoke test the grid no longer jumps on launch**

Use the `run-springchick` skill: launch the compositor, tap an app on Home, confirm the tapped icon does NOT change slots after the launch/close cycle. Screenshot before/after Home to compare order.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): launch records frecency + saves, no grid reorder"
```

---

## Phase E — Static drag-to-reorder (drop commits, no live gap yet)

Gets a working reorder before the live-animation polish, so the model path is proven independently.

### Task E1: `on_release` resolves grid→grid to `move_to`

**Files:**
- Modify: `crates/sc-compositor/src/input_common.rs` (~270-292 arrange release)

- [ ] **Step 1: Fill the stubbed `Reorder` arm**

C1 Step 3b already threaded `page`/`page_len`/`w`/`h` into the `resolve_drop` call and left `Reorder { .. } => {}`. Replace only that arm:
```rust
                input_dispatch::DropAction::Reorder { page, index } => {
                    state.model.move_to(&drag.app_id, page, index);
                    state.after_arrange_edit();
                }
```

- [ ] **Step 2: Build**

Run: `cargo build -p sc-compositor`
Expected: compiles.

- [ ] **Step 3: Smoke test reorder**

`run-springchick`: long-press an icon to enter arrange mode, drag it onto another grid slot, release. Confirm it lands at the new slot and the order persists (re-enter, check). Screenshot.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/input_common.rs
git commit -m "feat(compositor): drop grid icon on grid reorders it"
```

---

## Phase F — Live gap-opening

### Task F1: `DragItem.hover` + per-frame update

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (`DragItem` ~211-215)
- Modify: `crates/sc-compositor/src/input_common.rs` (`on_motion` ~57-63)

- [ ] **Step 1: Add the field**

```rust
struct DragItem {
    app_id: String,
    source: input_dispatch::IconSource,
    cur: (f32, f32),
    /// (page, index) hole the grid opens under the finger; None until first
    /// motion or when the finger is over the dock zone.
    hover: Option<(usize, usize)>,
}
```
Update **all three** `DragItem { ... }` constructions to add `hover: None`: `input_common.rs:170`, `input_common.rs:179`, and `main.rs:960` (the long-press auto-pickup in `advance_frame`). Missing any one is a compile error.

- [ ] **Step 2: Update `hover` in `on_motion`**

Replace the arrange-drag branch:
```rust
        if let Some(arrange) = &mut state.arrange {
            if let Some(drag) = &mut arrange.drag {
                drag.cur = (x, y);
                return;
            }
        }
```
with a version that computes the hovered slot against the **working order** (dragged app removed) on the current page:
```rust
        if state.arrange.as_ref().and_then(|a| a.drag.as_ref()).is_some() {
            let (w, h) = state.output_size_f();
            let page = if let UiState::Home { page, .. } = &state.ui { *page } else { 0 };
            let layout = sc_layout::compute(w, h, page, &state.model);
            let over_dock = layout.dock_zone.contains(x, y);
            let hover = if over_dock {
                None
            } else {
                // Fill count on this page with the dragged app removed, so the
                // nearest index maps against the hole-removed order (avoids the
                // one-slot same-page skew).
                let app = state.arrange.as_ref().unwrap().drag.as_ref().unwrap().app_id.clone();
                let live_len = state.model.pages.get(page)
                    .map_or(0, |p| p.iter().filter(|a| **a != app).count());
                let idx = sc_layout::nearest_grid_index(w, h, x, y).min(live_len);
                Some((page, idx))
            };
            if let Some(drag) = state.arrange.as_mut().unwrap().drag.as_mut() {
                drag.cur = (x, y);
                drag.hover = hover;
            }
            return;
        }
```

- [ ] **Step 3: Build**

Run: `cargo build -p sc-compositor`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/main.rs crates/sc-compositor/src/input_common.rs
git commit -m "feat(compositor): track hovered reorder slot during drag"
```

### Task F2: Working-order reflow while dragging

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (`reflow_targets` ~377, `reflow_grid` ~607, `advance_frame` grid-spring section ~974)

- [ ] **Step 1: Add a working-order variant of `reflow_targets`**

```rust
/// Reflow targets over an explicit page list (used for the live drag "working
/// order": dragged app removed, a hole opened at the hovered slot).
fn reflow_targets_for(pages: &[Vec<String>], width: f32, height: f32)
    -> std::collections::HashMap<String, (f32, f32)>
{
    let mut out = std::collections::HashMap::new();
    for (page, apps) in pages.iter().enumerate() {
        for (index, app) in apps.iter().enumerate() {
            out.insert(app.clone(), sc_layout::global_slot_pos(page, index, width, height));
        }
    }
    out
}
```
Refactor `reflow_targets` to delegate: `reflow_targets_for(&model.pages, width, height)`.

- [ ] **Step 2: Make `reflow_grid` honor an active drag**

```rust
    fn reflow_grid(&mut self) {
        let (w, h) = self.output_size_f();
        let targets = match self.arrange.as_ref().and_then(|a| a.drag.as_ref()) {
            Some(drag) => {
                let working = self.working_pages(&drag.app_id, drag.hover);
                reflow_targets_for(&working, w, h)
            }
            None => reflow_targets(&self.model, w, h),
        };
        for (app, (tx, ty)) in &targets {
            match self.grid_anim.get_mut(app) {
                Some((sx, sy)) => { sx.retarget(*tx); sy.retarget(*ty); }
                None => {
                    self.grid_anim.insert(app.clone(),
                        (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty)));
                }
            }
        }
        self.grid_anim.retain(|app, _| targets.contains_key(app));
    }

    /// The current pages with `dragged` removed and, if `hover` is set, a hole
    /// opened at that slot (the dragged app is NOT reinserted — it renders as a
    /// ghost following the finger).
    fn working_pages(&self, dragged: &str, hover: Option<(usize, usize)>)
        -> Vec<Vec<String>>
    {
        let mut pages: Vec<Vec<String>> = self.model.pages.clone();
        for p in &mut pages { p.retain(|a| a != dragged); }
        // No reinsertion: reflow_targets_for lays icons at 0..len, so the slots
        // at/after `hover.index` already sit one cell further than raw order —
        // that IS the open gap.
        let _ = hover; // hole is implicit in index-based slot layout
        pages
    }
```

> **Note for implementer:** the gap is implicit — because targets are assigned by list index, simply removing the dragged app makes everything after its old position shift up by one, and the hovered index determines where `move_to` will land on drop. If a *visible* hole that tracks the finger (rather than a compacted list) is desired, insert a placeholder marker at `hover.index` and skip it when assigning positions. Start with the compacted version; evaluate feel in the smoke test and only add the placeholder if the compaction reads worse than the spec's "hole follows finger" intent.

- [ ] **Step 3: Retarget every frame during a drag**

In `advance_frame`, where the grid springs are stepped (~974), before the step loop add:
```rust
        // Live reorder: retarget springs to the working order each frame while
        // an icon is being dragged over the grid.
        if self.arrange.as_ref().and_then(|a| a.drag.as_ref())
            .map_or(false, |d| d.hover.is_some())
        {
            self.reflow_grid();
        }
```

- [ ] **Step 4: Build + smoke test**

Run: `cargo build -p sc-compositor`
`run-springchick`: drag an icon slowly across others; confirm the rest reflow to open a gap that follows the finger, and the dragged icon rides as a ghost. Screenshot mid-drag. Decide compacted vs placeholder per the note.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): live reflow working-order gap during drag"
```

### Task F3: Drop uses live `hover`; ghost not double-drawn

**Files:**
- Modify: `crates/sc-compositor/src/input_common.rs` (arrange release)
- Verify (no change): `crates/sc-compositor/src/skia_gl.rs` `visible_grid_slots`/`draw_home` skip apps absent from `grid_positions`

- [ ] **Step 1: Prefer `hover` on drop**

In the `Reorder` path from Task E1, use the live hover when present (computed against working order, avoiding skew):
```rust
                input_dispatch::DropAction::Reorder { page, index } => {
                    let (pg, ix) = drag.hover.unwrap_or((page, index));
                    state.model.move_to(&drag.app_id, pg, ix);
                    state.after_arrange_edit();
                }
```

- [ ] **Step 2: Ghost is not double-drawn (already satisfied — verify only)**

`skia_gl::visible_grid_slots` builds grid slots by looking each app up in `grid_positions` and skipping misses. During a drag the dragged app is dropped from `grid_anim` (working-order `retain` in Task F2), so it is absent from `grid_positions` and never drawn in a slot — only the `ArrangeView` ghost draws it. No code change needed; just confirm at runtime.

- [ ] **Step 3: Build + smoke test**

`run-springchick`: drag within a page and across the gap; confirm drop lands exactly where the gap was (no one-slot skew), no duplicate icon during drag.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/input_common.rs crates/sc-compositor/src/render.rs
git commit -m "feat(compositor): commit reorder at live hover slot"
```

---

## Phase G — Cross-page drag

### Task G1: Edge-dwell page flip (+ create new trailing page)

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (`DragItem`, `advance_frame`)
- Modify: `crates/sc-compositor/src/input_common.rs` (`on_motion` hover already sets page)

- [ ] **Step 1: Add edge-dwell tracking to `DragItem`**

```rust
    /// When the finger entered the current edge zone (for dwell-to-flip).
    /// None when not in an edge zone.
    edge_since: Option<(std::time::Instant, EdgeSide)>,
```
with `enum EdgeSide { Left, Right }`. Initialize `edge_since: None` in **all three** `DragItem` constructors (`input_common.rs:170`, `input_common.rs:179`, `main.rs:960`).

- [ ] **Step 2: In `advance_frame`, flip on dwell**

Constants: `const EDGE_FRAC: f32 = 0.06;` (edge band width as fraction of screen) and `const EDGE_DWELL_MS: u128 = 400;`. Logic (run while a drag is active):
```rust
        if let Some(drag) = self.arrange.as_ref().and_then(|a| a.drag.as_ref()) {
            let (w, _h) = self.output_size_f();
            let x = drag.cur.0;
            let side = if x < w * EDGE_FRAC { Some(EdgeSide::Left) }
                       else if x > w * (1.0 - EDGE_FRAC) { Some(EdgeSide::Right) }
                       else { None };
            // ... compare/update edge_since; if dwell exceeded, flip page:
            // Left -> page = page.saturating_sub(1)
            // Right -> if page+1 == pages.len() push an empty page as a target,
            //          then page += 1
            // Reset edge_since to now (auto-repeat while held). Retarget the
            // Home page_spring to the new page. hover.page updates next motion.
        }
```
Implement the borrow-safe version (read the needed values, then mutate `self.arrange`, `self.model.pages`, and the `UiState::Home { page, page_spring, page_count, .. }` fields). Creating the trailing empty page lets a drag build a new page; `normalize_pages` on drop drops it if unused.

- [ ] **Step 3: Build + smoke test**

`run-springchick` (needs ≥2 pages, or drag to the right edge to create one): hold a dragged icon at the right edge ~0.4s, confirm the page flips and the icon can be dropped on the next page. Drop on a fresh right-edge page and confirm a new page is created; drop back and confirm the empty page collapses.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/main.rs crates/sc-compositor/src/input_common.rs
git commit -m "feat(compositor): edge-dwell page flip during reorder drag"
```

---

## Phase H — Full verification

### Task H1: Workspace green + manual pass

- [ ] **Step 1: Full test + lint**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: all green, no warnings (in particular, no dead `recompute_pages`/`landed_origin`/frecency-sort leftovers).

- [ ] **Step 2: Migration check**

With an existing pre-change config (has `frecency`, no `pages`): launch via `run-springchick`, confirm Home seeds alphabetically and persists after a reorder + restart.

- [ ] **Step 3: Regression sweep**

Confirm unchanged behaviors still work: grid→dock pin, dock→grid unpin, remove-badge hide, Done exits arrange. Launch does not move icons.

- [ ] **Step 4: Update memory if any gotcha surfaced**

If the borrow-checker dance in Task G2 or the draw_home ghost guard needed a non-obvious fix, note it in project memory.

- [ ] **Step 5: Final commit / branch wrap**

Use superpowers:finishing-a-development-branch to decide merge/PR.

---

## Notes / gotchas

- `resolve_drop` reads `layout.width`/`layout.height` in the plan; if `Layout` lacks those, thread `(w, h)` as explicit args (Task C1 Step 3).
- The live "gap" may be implicit (compaction) rather than a tracked placeholder hole — pick per feel in Task F2. Spec intent is "hole follows finger"; compaction is the cheap first cut.
- Frecency is now write-only. Do NOT delete `FrecencyStore`/`record_launch`/`eff` — future search consumes them.
