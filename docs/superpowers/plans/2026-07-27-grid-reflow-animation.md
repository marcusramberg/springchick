# Grid Reflow Animation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Animate the home grid so icons slide (across pages) to their new frecency/arrange positions instead of teleporting; on launch the view follows the app to its landing page and the app zoom-opens from there.

**Architecture:** Each grid app owns a 2D spring position in a global coordinate space (pages side-by-side; screen = `global - page_scroll*width`). Any `recompute_pages` is followed by `State::reflow_grid()` which retargets those springs. Render draws each app from a synthetic `IconSlot` at its animated center (icon + label + remove-badge together). Launch additionally retargets `page_spring` to the app's new page and captures the zoom origin from the landed slot.

**Tech Stack:** Rust, `sc-anim::Spring`, `sc-layout`, `sc-compositor` (skia GL).

**Spec:** `docs/superpowers/specs/2026-07-27-grid-reflow-animation-design.md`

**Env:** bare `cargo` fails (no internet). Use `nix develop --command cargo …`. Manual verify via `run-springchick` (`driver.sh build` first, then `up`).

---

## File Structure

- `crates/sc-layout/src/lib.rs` — `global_slot_pos`, `slot_at_center`.
- `crates/sc-compositor/src/main.rs` — `grid_anim` state; `reflow_targets` (pure); `State::reflow_grid`; step springs in `advance_frame`; page-follow + zoom-origin in `launch_or_raise`; call `reflow_grid` from `after_arrange_edit`; build `grid_positions` at both `DrawCtx` sites.
- `crates/sc-compositor/src/render.rs` — `grid_positions` field on `DrawCtx`, pass into `draw_home`.
- `crates/sc-compositor/src/skia_gl.rs` — per-app animated grid pass (icons + arrange badges from animated slots).

---

## Task 1: Layout — global_slot_pos + slot_at_center

**Files:** Modify + Test: `crates/sc-layout/src/lib.rs`

- [ ] **Step 1: Write failing tests:**

```rust
#[test]
fn global_slot_pos_matches_compute_first_slot() {
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    let (gx, gy) = global_slot_pos(0, 0, 1224.0, 2700.0);
    assert!((gx - l.grid[0].icon_rect.center_x()).abs() < 0.01);
    assert!((gy - l.grid[0].icon_rect.center_y()).abs() < 0.01);
}

#[test]
fn global_slot_pos_page1_offset_by_width() {
    let (x0, y0) = global_slot_pos(0, 0, 1224.0, 2700.0);
    let (x1, y1) = global_slot_pos(1, 0, 1224.0, 2700.0);
    assert!((x1 - (x0 + 1224.0)).abs() < 0.01);
    assert!((y1 - y0).abs() < 0.01);
}

#[test]
fn global_slot_pos_advances_by_cell_within_page() {
    let (x0, _) = global_slot_pos(0, 0, 1224.0, 2700.0);
    let (x1, _) = global_slot_pos(0, 1, 1224.0, 2700.0); // next column
    assert!(x1 > x0);
}

#[test]
fn slot_at_center_places_icon_and_badge() {
    let s = slot_at_center("x".into(), 500.0, 600.0, 1224.0, 2700.0);
    assert_eq!(s.app_id, "x");
    assert!((s.icon_rect.center_x() - 500.0).abs() < 0.01);
    assert!((s.icon_rect.center_y() - 600.0).abs() < 0.01);
    // same icon size compute uses:
    let m = sample_model();
    let l = compute(1224.0, 2700.0, 0, &m);
    assert!((s.icon_rect.w - l.grid[0].icon_rect.w).abs() < 0.01);
    // badge sits at the icon's top-left (same as compute's badge)
    assert!(s.badge_rect.w > 0.0);
    assert!((s.badge_rect.center_x() - s.icon_rect.x).abs() < s.icon_rect.w);
}
```

- [ ] **Step 2:** `nix develop --command cargo test -p sc-layout` → FAIL.

- [ ] **Step 3: Implement.** Refactor the per-slot geometry in `compute` so the icon/label/badge rects for a grid cell are produced by a shared helper, then express both `compute` and the new functions in terms of it. Concretely:
  - Extract the grid cell metrics (`grid_left`, `grid_top`, `cell_w`, `cell_h`, `icon_size`, `label_h`, and the badge closure) into a small private helper or free function that, given `(width, height)`, returns those metrics. `compute` already computes all of these — factor them so they aren't duplicated.
  - `global_slot_pos(page, index, width, height)`: `col = index % COLS`, `row = index / COLS`; compute the same `icon_x/icon_y` as `compute` does, then return the icon-rect **center**, adding `page as f32 * width` to x.
  - `slot_at_center(app_id, cx, cy, width, height)`: build an `IconSlot` whose `icon_rect` is centered at `(cx, cy)` with the grid `icon_size`; `label_rect` below it (width = `cell_w`, height = `label_h`, positioned as in `compute` relative to the icon); `badge_rect` via the same `badge(icon_rect)` closure used in the arrange feature.
  - Keep `compute`'s output byte-identical (existing layout tests must still pass) — this is a refactor-to-share, not a behavior change.

- [ ] **Step 4:** `nix develop --command cargo test -p sc-layout` → PASS (new + all existing). `nix develop --command cargo build -p sc-compositor` (downstream still compiles).

- [ ] **Step 5: Commit** — `git add crates/sc-layout/ && git commit -m "feat(layout): global_slot_pos + slot_at_center for reflow"`

---

## Task 2: Reflow controller + launch follow

**Files:** Modify: `crates/sc-compositor/src/main.rs`. Test: same file (`reflow_targets` unit test).

- [ ] **Step 1: Write a failing test** for the pure target mapping (add near other main.rs tests, or create a `#[cfg(test)] mod` if none):

```rust
#[test]
fn reflow_targets_maps_pages_and_excludes_dock() {
    let mut m = ShellModel::default();
    // 25 apps -> page 0 full (24) + page 1 (1)
    for i in 0..25 { m.frecency.record_launch(&format!("app{i:02}"), 0); }
    let catalog: Vec<String> = (0..25).map(|i| format!("app{i:02}")).collect();
    m.recompute_pages(&catalog, 0);
    let t = reflow_targets(&m, 1224.0, 2700.0);
    assert_eq!(t.len(), 25);
    // the app on page 1 has x > width
    let page1_app = &m.pages[1][0];
    assert!(t[page1_app].0 > 1224.0);
    // a page-0 app has x < width
    let page0_app = &m.pages[0][0];
    assert!(t[page0_app].0 < 1224.0);
}
```

- [ ] **Step 2:** `nix develop --command cargo test -p sc-compositor` → FAIL (`reflow_targets` missing).

- [ ] **Step 3: Implement.**
  - Add field to `State`: `grid_anim: std::collections::HashMap<String, (sc_anim::Spring, sc_anim::Spring)>`, initialized empty (`HashMap::new()`) in the constructor.
  - Pure fn (module level):
    ```rust
    fn reflow_targets(model: &ShellModel, width: f32, height: f32) -> std::collections::HashMap<String, (f32, f32)> {
        let mut out = std::collections::HashMap::new();
        for (page, apps) in model.pages.iter().enumerate() {
            for (index, app) in apps.iter().enumerate() {
                out.insert(app.clone(), sc_layout::global_slot_pos(page, index, width, height));
            }
        }
        out
    }
    ```
  - `State::reflow_grid`:
    ```rust
    fn reflow_grid(&mut self) {
        let (w, h) = self.output_size_f();
        let targets = reflow_targets(&self.model, w, h);
        // retarget existing, seed missing (snapped), drop removed
        for (app, (tx, ty)) in &targets {
            match self.grid_anim.get_mut(app) {
                Some((sx, sy)) => { sx.retarget(*tx); sy.retarget(*ty); }
                None => { self.grid_anim.insert(app.clone(), (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty))); }
            }
        }
        self.grid_anim.retain(|app, _| targets.contains_key(app));
    }
    ```
  - **Step springs in `advance_frame`**: after the existing spring/tick work, step every `grid_anim` spring by `dt`:
    ```rust
    for (sx, sy) in self.grid_anim.values_mut() { sx.step(dt); sy.step(dt); }
    ```
    Also lazily seed on first use: if `self.grid_anim.is_empty()`, call `self.reflow_grid()` before stepping (seeds snapped from the current order). The dev/DRM loops render continuously, so unsettled springs animate without extra scheduling.
  - **Wire arrange edits**: in `after_arrange_edit`, after `recompute_pages`, call `self.reflow_grid();` (before/after save, doesn't matter).
  - **Launch follow + origin** in `launch_or_raise`: after `record_launch` + `recompute_pages`:
    ```rust
    self.reflow_grid();
    // Follow: scroll to the launched app's new page, and set the zoom origin to its landed slot.
    let (w, h) = self.output_size_f();
    let mut landed: Option<(usize, usize)> = None; // (page, index)
    for (pg, apps) in self.model.pages.iter().enumerate() {
        if let Some(ix) = apps.iter().position(|a| a == app_id) { landed = Some((pg, ix)); break; }
    }
    if let Some((pg, ix)) = landed {
        if let UiState::Home { page, page_spring, page_count, .. } = &mut self.ui {
            *page = pg;
            *page_count = self.model.pages.len().max(1);
            page_spring.retarget(pg as f32);
        }
        // origin = landed slot's on-screen center on its own page
        let l = sc_layout::compute(w, h, pg, &self.model);
        if let Some(slot) = l.grid.get(ix) {
            origin = ZoomOrigin::icon((slot.icon_rect.center_x(), slot.icon_rect.center_y()));
        }
    }
    ```
    Note: `origin` is the local variable already passed to the zoom transition; adjust to the file's actual flow (the parameter is `origin: ZoomOrigin` — rebind it as `let mut origin = origin;` at the top of the fn, or set `self.last_origin` which the launch path uses). Follow the existing `self.last_origin = origin;` usage: set `self.last_origin` to the landed-slot origin instead of the tapped one. Only override when the app is on the grid; if `landed` is `None` (app is docked), keep the tapped origin.

- [ ] **Step 4:** `nix develop --command cargo test -p sc-compositor` → PASS. `nix develop --command cargo build -p sc-compositor` → clean.

- [ ] **Step 5: Commit** — `git add crates/sc-compositor/ && git commit -m "feat(compositor): grid reflow springs + launch page-follow"`

---

## Task 3: Render animated grid

**Files:** Modify: `crates/sc-compositor/src/main.rs` + `drm_backend.rs` (build `grid_positions`), `crates/sc-compositor/src/render.rs` (`DrawCtx` field + pass-through), `crates/sc-compositor/src/skia_gl.rs` (`draw_home` per-app pass).

- [ ] **Step 1: Thread `grid_positions` via `DrawCtx`.**
  - `render.rs`: add `pub grid_positions: &'a std::collections::HashMap<String, (f32, f32)>` to `DrawCtx` (screen-space centers). In `draw_scene`, pass `ctx.grid_positions` into `draw_home` as a new parameter.
  - `main.rs` (~1476 `DrawCtx {`) and `drm_backend.rs` (~443): before building `DrawCtx`, compute screen-space positions from `state.grid_anim` and the current `page_scroll`:
    ```rust
    let page_scroll = if let UiState::Home { page_spring, .. } = &state.ui { page_spring.value } else { 0.0 };
    let w = state.output_size.0 as f32;
    let grid_positions: std::collections::HashMap<String, (f32, f32)> =
        state.grid_anim.iter()
            .map(|(app, (sx, sy))| (app.clone(), (sx.value - page_scroll * w, sy.value)))
            .collect();
    ```
    Store it in a `let` that outlives `ctx`, set `grid_positions: &grid_positions` on `DrawCtx`. (Mirror `pressed_app` lifetime handling; both backends.)

- [ ] **Step 2: Per-app animated grid pass in `draw_home`.**
  - Add param `grid_positions: &HashMap<String, (f32, f32)>` to `draw_home`.
  - Replace the `pages_to_draw` loop (skia_gl.rs ~245-267) that draws grid icons per page with a single pass: build the visible animated slots once —
    ```rust
    let mut anim_slots: Vec<sc_layout::IconSlot> = Vec::new();
    for (app, (sx, sy)) in grid_positions {
        // cull: skip icons fully off-screen (with one-icon margin)
        if *sx < -(width as f32) * 0.3 || *sx > width as f32 * 1.3 { continue; }
        anim_slots.push(sc_layout::slot_at_center(app.clone(), *sx, *sy, width as f32, height as f32));
    }
    for slot in &anim_slots {
        draw_icon_slot(canvas, slot, &self.icon_images, &self.font, app_catalog, pressed_app == Some(slot.app_id.as_str()));
    }
    ```
    Remove the now-unused `pages_to_draw`/per-page grid loop and the `page_offset` grid usage (keep `page` for dots). `page_offset` is no longer needed for the grid (positions already baked); if it becomes fully unused, drop the param — but dots/dock still use `page`/`model`.
  - **Arrange badges from animated slots**: in the arrange block that currently draws grid remove-badges from `current_layout.grid`, iterate `anim_slots` instead (so grid badges follow the sliding icons). Dock badges continue to use `current_layout.dock` (dock doesn't reflow).
  - Keep dock, dots, bar, Done button, drag-ghost exactly as they are.

- [ ] **Step 3: Build + test.** `nix develop --command cargo build -p sc-compositor` → clean. `nix develop --command cargo test` (workspace) → green.

- [ ] **Step 4: Manual verify (run-springchick).** Back up `~/.config/springchick/state.toml` first; restore after.
  - `driver.sh build` (background/long timeout), then `up`, `client`, screenshot home.
  - Launch an app that lives on page 2 (`down` its icon center, brief hold NOT needed — a tap launches; but to see reflow, tap and immediately screenshot a few times, or use `settle` between shots): tap → the grid should slide and the view follow to page 1, the app landing in slot 0, then zoom-open from there.
  - Enter arrange mode, pin an app → the grid should slide (not snap) as it leaves for the dock; hide an app → remaining icons slide up with badges attached.
  - Confirm no detached badges, no icons stuck off-screen, page dots correct.

- [ ] **Step 5: Commit** — `git add crates/sc-compositor/ && git commit -m "feat(compositor): render animated grid reflow"`

---

## Done criteria

- `cargo test` green across `sc-layout` + `sc-compositor`.
- Launching an app slides the grid, follows the view to the app's new page, lands it in slot 0, and zoom-opens from there.
- Arrange pin/unpin/hide slide the grid (badges stay attached to their icons).
- Page-swipe still works and doesn't fight an in-flight reflow.
- No teleporting icons, no detached badges, off-screen icons culled.
