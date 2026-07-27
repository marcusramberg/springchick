# Frecency-ordered Home Grid Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Order the home springboard grid by frecency (frequency + recency of launches), persisted across restarts.

**Architecture:** Add a `FrecencyStore` (per-app exponential-decay score + last-launch timestamp) to `ShellModel`. Grid `pages` become a runtime-derived view (`#[serde(skip)]`), recomputed from catalog + frecency at startup and after each launch. `state.toml` persists only `dock` + `frecency`. The launch choke point `launch_or_raise` records usage, recomputes pages, and saves.

**Tech Stack:** Rust, serde/toml, existing `sc-shell-model` (pure, `#![forbid(unsafe_code)]`) and `sc-config` crates.

**Spec:** `docs/superpowers/specs/2026-07-27-frecency-home-grid-design.md`

---

## File Structure

- `crates/sc-shell-model/src/lib.rs` — add `AppStat`, `FrecencyStore`, `HALF_LIFE_SECS`, `eff`, `record_launch`; change `ShellModel` (skip `pages`, add `frecency`, add `recompute_pages`). Core logic + unit tests live here.
- `crates/sc-config/src/state.rs` — persistence tests (round-trip with frecency, legacy-file load). Save/load code is unchanged (struct change flows through `toml`).
- `crates/sc-compositor/src/main.rs` — bootstrap seeding + `recompute_pages` (replaces `place` loop); `record_launch` + recompute + save in `launch_or_raise`; `unix_now` helper.

Design note: `pages` stays a field on `ShellModel` so every existing reader (`render.rs`, `skia_gl.rs`, `input_dispatch.rs`, `input_common.rs`, `main.rs`) is untouched — it is just no longer serialized and is filled by `recompute_pages` instead of `place`.

---

## Task 1: Frecency store + derived pages in `sc-shell-model`

**Files:**
- Modify: `crates/sc-shell-model/src/lib.rs`
- Test: same file (`#[cfg(test)] mod tests`)

- [ ] **Step 0: Add the `toml` dev-dependency**

`crates/sc-shell-model/Cargo.toml` has **no `[dev-dependencies]` section today**. The `pages_not_serialized_frecency_is` test needs `toml`. Add, pinning to the same version `sc-config` uses (check `crates/sc-config/Cargo.toml`):

```toml
[dev-dependencies]
toml = "<match sc-config>"
```

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `crates/sc-shell-model/src/lib.rs`:

```rust
#[test]
fn eff_halves_after_one_half_life() {
    let stat = AppStat { score: 8.0, last_launch: 0 };
    let now = HALF_LIFE_SECS as u64;
    assert!((eff(&stat, now) - 4.0).abs() < 1e-6);
}

#[test]
fn record_launch_on_fresh_app_scores_one() {
    let mut s = FrecencyStore::default();
    s.record_launch(&"a".to_string(), 1000);
    let stat = &s.apps["a"];
    assert!((stat.score - 1.0).abs() < 1e-9);
    assert_eq!(stat.last_launch, 1000);
}

#[test]
fn record_launch_decays_before_incrementing() {
    let mut s = FrecencyStore::default();
    s.record_launch(&"a".to_string(), 0);
    // one half-life later: 1.0 decays to 0.5, then +1 => 1.5
    s.record_launch(&"a".to_string(), HALF_LIFE_SECS as u64);
    assert!((s.apps["a"].score - 1.5).abs() < 1e-6);
}

#[test]
fn seed_first_run_is_zero_later_install_is_one() {
    let mut empty = FrecencyStore::default();
    empty.seed(&"a".to_string(), 5000);
    assert_eq!(empty.apps["a"], AppStat { score: 0.0, last_launch: 0 });

    let mut populated = FrecencyStore::default();
    populated.record_launch(&"x".to_string(), 100);
    populated.seed(&"b".to_string(), 5000);
    assert_eq!(populated.apps["b"], AppStat { score: 1.0, last_launch: 5000 });
}

#[test]
fn recompute_pages_orders_by_frecency_excluding_dock() {
    let mut m = ShellModel::default();
    m.dock = vec!["docked".into()];
    m.frecency.record_launch(&"low".to_string(), 0);
    m.frecency.record_launch(&"high".to_string(), 0);
    m.frecency.record_launch(&"high".to_string(), 0); // score 2.0
    let catalog = ["high", "low", "docked", "zzz"].map(String::from).to_vec();
    m.recompute_pages(&catalog, 0);
    // docked excluded; high > low > zzz(zero, alpha last among zeros)
    assert_eq!(m.pages[0][0], "high");
    assert_eq!(m.pages[0][1], "low");
    assert_eq!(m.pages[0][2], "zzz");
    assert!(!m.pages.iter().flatten().any(|a| a == "docked"));
}

#[test]
fn recompute_pages_all_zero_is_alphabetical() {
    let mut m = ShellModel::default();
    let catalog = ["c", "a", "b"].map(String::from).to_vec();
    m.recompute_pages(&catalog, 0);
    assert_eq!(m.pages[0], vec!["a", "b", "c"]);
}

#[test]
fn recompute_pages_chunks_into_pages() {
    let mut m = ShellModel::default();
    let catalog: Vec<String> = (0..(PAGE_CAP + 3)).map(|i| format!("app{i:03}")).collect();
    m.recompute_pages(&catalog, 0);
    assert_eq!(m.pages.len(), 2);
    assert_eq!(m.pages[0].len(), PAGE_CAP);
    assert_eq!(m.pages[1].len(), 3);
}

#[test]
fn pages_not_serialized_frecency_is() {
    let mut m = ShellModel::default();
    m.frecency.record_launch(&"a".to_string(), 42);
    m.pages = vec![vec!["a".into()]];
    let s = toml::to_string_pretty(&m).unwrap();
    assert!(!s.contains("pages"));
    assert!(s.contains("frecency") || s.contains("[frecency"));
    let back: ShellModel = toml::from_str(&s).unwrap();
    assert!(back.pages.is_empty());
    assert_eq!(back.frecency.apps["a"].last_launch, 42);
}
```

Add `toml` as a dev-dependency if not already present (check `crates/sc-shell-model/Cargo.toml`; `sc-config` already uses `toml`, confirm the workspace version).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p sc-shell-model`
Expected: FAIL — `AppStat`, `FrecencyStore`, `eff`, `HALF_LIFE_SECS`, `seed`, `recompute_pages` not found.

- [ ] **Step 3: Implement**

In `crates/sc-shell-model/src/lib.rs`:

```rust
use std::collections::HashMap;

/// Frecency half-life: a launch's contribution halves every 30 days.
pub const HALF_LIFE_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStat {
    pub score: f64,
    pub last_launch: u64, // unix seconds; 0 = never launched
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FrecencyStore {
    pub apps: HashMap<AppId, AppStat>,
}

/// Decayed score of `stat` evaluated at `now` (unix secs). Compare all apps at
/// the same `now` to get a consistent ordering.
pub fn eff(stat: &AppStat, now: u64) -> f64 {
    let elapsed = now.saturating_sub(stat.last_launch) as f64;
    stat.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS)
}

impl FrecencyStore {
    /// Record an app launch: decay the stored score to `now`, then add 1.
    pub fn record_launch(&mut self, app: &AppId, now: u64) {
        let s = self.apps.entry(app.clone()).or_default();
        let elapsed = now.saturating_sub(s.last_launch) as f64;
        s.score = s.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS) + 1.0;
        s.last_launch = now;
    }

    /// Insert an app not yet in the store. First-ever run (store empty) → zero.
    /// Later install (store non-empty) → seed 1.0 at `now` so it surfaces.
    pub fn seed(&mut self, app: &AppId, now: u64) {
        if self.apps.contains_key(app) {
            return;
        }
        let stat = if self.apps.is_empty() {
            AppStat { score: 0.0, last_launch: 0 }
        } else {
            AppStat { score: 1.0, last_launch: now }
        };
        self.apps.insert(app.clone(), stat);
    }
}
```

Change `ShellModel`:

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ShellModel {
    /// Runtime-derived grid view. Recomputed from catalog + frecency; never
    /// persisted (see `recompute_pages`).
    #[serde(skip)]
    pub pages: Vec<Vec<AppId>>,
    pub dock: Vec<AppId>, // len <= DOCK_CAP
    #[serde(default)]
    pub frecency: FrecencyStore,
}
```

Add method:

```rust
impl ShellModel {
    /// Rebuild `pages` from the catalog ordered by frecency at `now`,
    /// excluding docked apps, chunked into PAGE_CAP-sized pages.
    pub fn recompute_pages(&mut self, catalog_ids: &[AppId], now: u64) {
        let mut ids: Vec<AppId> = catalog_ids
            .iter()
            .filter(|id| !self.dock.contains(id))
            .cloned()
            .collect();
        ids.sort_by(|a, b| {
            let ea = self.frecency.apps.get(a).map_or(0.0, |s| eff(s, now));
            let eb = self.frecency.apps.get(b).map_or(0.0, |s| eff(s, now));
            eb.total_cmp(&ea).then_with(|| a.cmp(b))
        });
        self.pages = ids
            .chunks(PAGE_CAP)
            .map(|c| c.to_vec())
            .collect();
    }
}
```

Keep the existing `place`, `delete`, `move_to` methods (still exercised by their tests; unused by the compositor after Task 3). Add `#[allow(dead_code)]` only if the crate denies warnings.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sc-shell-model`
Expected: PASS (all new + pre-existing tests).

- [ ] **Step 5: Commit**

```bash
git add crates/sc-shell-model/
git commit -m "feat(shell-model): frecency store + derived grid pages"
```

---

## Task 2: Persistence round-trip + legacy load (`sc-config`)

**Files:**
- Test: `crates/sc-config/src/state.rs` (`#[cfg(test)] mod tests`)
- Modify: `crates/sc-config/src/state.rs` only if a test exposes a real gap.

- [ ] **Step 1: Write failing tests**

Add to the `tests` module in `crates/sc-config/src/state.rs`:

```rust
#[test]
fn round_trips_frecency() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("springchick/state.toml");
    let mut m = ShellModel::default();
    m.dock.push("org.gnome.Console".into());
    m.frecency.record_launch(&"org.gnome.Maps".to_string(), 12345);
    save(&m, &path).unwrap();
    let back = load(&path).unwrap();
    assert_eq!(m.dock, back.dock);
    assert_eq!(m.frecency, back.frecency);
    assert!(back.pages.is_empty()); // pages are not persisted
}

#[test]
fn legacy_file_loads_dock_defaults_frecency() {
    // A pre-frecency state.toml: has pages + dock, no frecency table.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    std::fs::write(
        &path,
        "dock = [\"org.gnome.Console\"]\npages = [[\"org.gnome.Maps\"]]\n",
    )
    .unwrap();
    let m = load(&path).unwrap();
    assert_eq!(m.dock, vec!["org.gnome.Console".to_string()]);
    assert!(m.frecency.apps.is_empty());
    assert!(m.pages.is_empty()); // legacy pages ignored (skipped field)
}
```

- [ ] **Step 2: Run tests to verify they fail (or reveal a gap)**

Run: `cargo test -p sc-config`
Expected: `round_trips_frecency` and `legacy_file_loads_dock_defaults_frecency` compile and pass *if* Task 1's serde attributes are correct. If `legacy_file` fails on the unknown `pages` key, add `#[serde(skip)]` (already planned) — `toml` ignores unknown keys by default, so this should pass without changing `state.rs`.

Note: the existing `round_trips` test asserts full `ShellModel` equality including `pages`; since `pages` is now `#[serde(skip)]`, a saved-then-loaded model has empty `pages`. Update that existing test to compare `dock` + `frecency` (and drop its `m.place(...)` page assertion) rather than full equality.

- [ ] **Step 3: Implement / fix**

Expected: no code change to `state.rs` needed. Only adjust the pre-existing `round_trips` test as noted above.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p sc-config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-config/
git commit -m "test(config): frecency round-trip + legacy state.toml load"
```

---

## Task 3: Bootstrap seeding + derive pages at startup (`main.rs`)

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` (bootstrap block ~399-425; `config_path` region for `unix_now`)

- [ ] **Step 1: Add a `unix_now` helper**

Near `config_path()` (`main.rs:133`):

```rust
/// Current unix time in whole seconds (monotonic-enough for frecency).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
```

- [ ] **Step 2: Replace the `place` bootstrap loop**

Replace the block at `main.rs:405-419` (the `existing`/`for id ... model.place` loop) with:

```rust
        // Seed any catalog apps not yet tracked, then derive the grid order.
        let mut model = model;
        let now = unix_now();
        let mut catalog_ids: Vec<String> = app_catalog.keys().cloned().collect();
        catalog_ids.sort(); // deterministic seeding + first-run alpha order
        for id in &catalog_ids {
            model.frecency.seed(id, now);
        }
        model.recompute_pages(&catalog_ids, now);
```

`page_count` at `main.rs:~421` (`model.pages.len().max(1)`) now reads the derived pages — leave as is.

- [ ] **Step 3: Build + run existing compositor tests**

Run: `cargo test -p sc-compositor`
Expected: PASS (no behavior test covers bootstrap ordering directly; this confirms it compiles and nothing regressed).

- [ ] **Step 4: Sanity-check the render path compiles**

Run: `cargo build -p sc-compositor`
Expected: builds clean. `render.rs` / `skia_gl.rs` / `input_dispatch.rs` still read `model.pages`, now populated by `recompute_pages`.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat(compositor): seed frecency + derive grid pages at startup"
```

---

## Task 4: Record launches + persist on each launch (`main.rs`)

**Files:**
- Modify: `crates/sc-compositor/src/main.rs` — `launch_or_raise` (514-540)

- [ ] **Step 1: Record + recompute + save in `launch_or_raise`**

`launch_or_raise` has two exit paths: the raise branch (`return` inside the loop, ~528) and the launch-new branch (end, ~539). Record on both — raising counts as usage (per spec). Add a helper and call it before each of the two outcomes, or wrap the whole body. Simplest: record at the top of the function, before the running-check, since both paths are "the user chose this app":

```rust
    fn launch_or_raise(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;

        // Record usage for frecency, re-derive grid order, persist.
        let now = unix_now();
        self.model.frecency.record_launch(&app_id.to_string(), now);
        let mut catalog_ids: Vec<String> = self.app_catalog.keys().cloned().collect();
        catalog_ids.sort();
        self.model.recompute_pages(&catalog_ids, now);
        if let Err(e) = config_state::save(&self.model, &config_path()) {
            warn!(%e, "failed to save shell model after launch");
        }

        // ... existing running-check + launch-new body unchanged ...
    }
```

Ensure `config_state` and `warn` are in scope (both already used in `main.rs`).

- [ ] **Step 2: Add a targeted test for the record+derive behavior**

The compositor `State` is heavy to construct in a unit test; the ordering logic is already covered in `sc-shell-model`. Instead assert the intended sequence at the model level (belongs in `sc-shell-model`, add if not already covered by Task 1):

```rust
#[test]
fn launch_promotes_app_to_front_of_grid() {
    let mut m = ShellModel::default();
    let catalog = ["a", "b", "c"].map(String::from).to_vec();
    m.recompute_pages(&catalog, 0);     // alpha: a, b, c
    m.frecency.record_launch(&"c".to_string(), 10);
    m.recompute_pages(&catalog, 10);    // c now highest
    assert_eq!(m.pages[0][0], "c");
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p sc-shell-model -p sc-compositor`
Expected: PASS.

- [ ] **Step 4: Manual verification in the running compositor**

Use the `run-springchick` skill to launch locally with the debug input socket. Tap an app icon on the grid, return home, and confirm that app has moved toward the front (page 1, top-left region) on the next home view. Confirm `~/.config/springchick/state.toml` (see `config_path`) now contains a `[frecency.apps.<id>]` entry and no `pages` key.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs crates/sc-shell-model/
git commit -m "feat(compositor): record launches + persist frecency each launch"
```

---

## Done criteria

- `cargo test` green across `sc-shell-model`, `sc-config`, `sc-compositor`.
- Grid order reflects launch frecency and survives a compositor restart.
- `state.toml` persists `dock` + `frecency`, no `pages`.
- Legacy `state.toml` loads without error (dock preserved, order resets to alphabetical once).
- Dock untouched by frecency. Drag-to-dock remains a separate future spec.
