# Frecency-ordered home grid — design

Date: 2026-07-27
Status: Approved (design)

## Problem

The home springboard grid is ordered by `.desktop` catalog scan order, frozen at
first run into `state.toml` (`ShellModel.pages`). Order never adapts to use.
Manual icon-drag rearrange (`ShellModel::move_to`) exists in the model but is
**not wired to any input path** on device, so users cannot reorder either. Goal:
order grid pages by *frecency* (frequency + recency of launches), and persist the
underlying data so ordering survives restarts.

## Scope

In scope:
- Grid pages reorder by frecency.
- Persist per-app frecency data; recompute page order each session and after each
  launch.

Out of scope (flagged follow-ups):
- **Drag-to-dock** — dragging an app off the grid into the dock. Requires a new
  icon-drag gesture (jiggle/edit mode) that does not exist today. Independent
  feature, separate spec.
- Manual grid rearrange / hide-app. `move_to`/`delete` stay unused by this work.

## Non-goals / decisions

- Dock stays **fixed** (user-pinned favorites); frecency never touches it.
- Pages become **derived**, not persisted. `state.toml` persists `dock` + frecency
  store only. The `pages` field is dropped from the persisted model and recomputed
  at runtime from catalog + frecency.
- Persist frecency on **each launch** (device is often killed uncleanly, not shut
  down; per-launch write is a small TOML file, cheap).

## Data model

New store (module in `sc-shell-model`, pure/no-unsafe crate):

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStat {
    pub score: f64,
    pub last_launch: u64, // unix seconds; 0 = never launched
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FrecencyStore {
    pub apps: std::collections::HashMap<AppId, AppStat>,
}
```

Exponential decay, evaluated lazily. Half-life constant:

```rust
pub const HALF_LIFE_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0; // 30 days
```

Effective (decayed) score at reference time `now`, used for sorting so every app
is compared at the same instant:

```rust
fn eff(stat: &AppStat, now: u64) -> f64 {
    let elapsed = now.saturating_sub(stat.last_launch) as f64;
    stat.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS)
}
```

Record a launch (decay stored score to now, then +1):

```rust
fn record_launch(&mut self, app: &AppId, now: u64) {
    let s = self.apps.entry(app.clone()).or_default();
    let elapsed = now.saturating_sub(s.last_launch) as f64;
    s.score = s.score * 0.5_f64.powf(elapsed / HALF_LIFE_SECS) + 1.0;
    s.last_launch = now;
}
```

Note: an app whose `last_launch == 0` (never launched, first-run seed) has
`eff == score == 0`.

`eff`, `record_launch`, `FrecencyStore`, `AppStat`, and the seeding entry point
are all `pub` (used by both `sc-shell-model` unit tests and page-derivation code
in `main.rs`). "Launch" here means `launch_or_raise` firing for an app —
**raising an already-running app counts as usage** and records too.

## Seeding new apps

During catalog scan, for each catalog `id` not already present in the store:

- Store **empty** (first-ever springchick run) → insert `{ score: 0.0,
  last_launch: 0 }`. All pre-existing apps start equal.
- Store **non-empty** (app newly installed after first run) → insert `{ score:
  1.0, last_launch: now }` → its `eff` floats above every never-launched app, so a
  freshly installed app surfaces toward page 1 until proven unused, then decays.

The store's own emptiness distinguishes first-run from later-install — no extra
"seen" flag needed.

Docked apps are seeded and recorded into the store too (seeding iterates all
catalog ids; raising a docked app records a launch). Harmless and intended — they
are simply filtered out at page derivation, so their score just never affects grid
order.

## Page derivation

Pages are computed, never stored. At startup and after each launch:

```
let now = unix_now();
pages = catalog_ids
    .filter(|id| !dock.contains(id))
    .sorted_by(|a, b|
        eff(b, now).total_cmp(&eff(a, now))      // higher frecency first
            .then_with(|| a.cmp(b)))              // stable alpha tiebreak (also covers all-zero first run)
    .chunks(PAGE_CAP)
    .map(collect)
    .collect();
```

Tiebreak is alphabetical `AppId` order; on first run (all `eff == 0`) this yields
a deterministic alphabetical grid.

## Persistence

`state.toml` schema change:

- `ShellModel` keeps `dock: Vec<AppId>`.
- `ShellModel` gains `frecency: FrecencyStore`, annotated `#[serde(default)]` so a
  legacy `state.toml` with no `frecency` key loads (toml errors on a missing field
  otherwise) instead of silently resetting.
- `ShellModel.pages` is **removed** from the persisted struct. Runtime page layout
  lives outside the persisted model (computed view).

Backward compat: old `state.toml` files contain a `pages` array and no
`frecency`. `serde` ignores unknown `pages` and defaults `frecency` to empty →
old file loads as "first run" (empty store, dock preserved). Acceptable: one-time
reset of ordering to alphabetical, dock intact.

Save timing: call `config_state::save` after each `launch_or_raise` that records a
launch (in addition to existing shutdown save).

## Integration points

- `launch_or_raise(app_id, origin)` (`main.rs:514`) — single launch choke point.
  After deciding to launch/raise, `record_launch(app_id, now)`, recompute pages,
  `save`.
- Catalog placement loop (`main.rs:~414`) — replace `model.place(id)` bootstrap
  with seeding into the frecency store, then derive pages.
- `sc-config/state.rs` — serialize new `ShellModel` shape (mechanical; struct
  change flows through `toml`).
- Wherever runtime code reads `state.model.pages` for rendering/input
  (`render.rs`, `skia_gl.rs`, `input_dispatch.rs`, `input_common.rs:407`,
  `main.rs:821`) — now reads the derived pages held in `State`.

## Testing

Unit (`sc-shell-model`):
- `eff` halves after exactly `HALF_LIFE_SECS`.
- `record_launch` on fresh app → score 1.0; second immediate launch → ~2.0.
- decayed launch: old score decays before +1.
- sort: higher frecency first, alpha tiebreak, all-zero → alphabetical.
- seeding: empty store → new app score 0; non-empty store → new app score 1.0,
  `last_launch = now`.
- pagination: N apps chunk into `ceil(N/PAGE_CAP)` pages, dock excluded.

Persistence (`sc-config`):
- round-trip `ShellModel` with populated `frecency`.
- legacy file (has `pages`, no `frecency`) loads with empty store + dock intact.

## Risks

- Grid order shifts under the user as they launch apps. Expected/desired, but a
  just-launched app changing page position could surprise. Mitigation: reorder is
  recomputed but the app they launched is now foreground (grid not visible during
  use); order settles by next home visit.
- All-zero first run yields alphabetical, not the prior catalog-scan order. One-
  time cosmetic change, acceptable.
