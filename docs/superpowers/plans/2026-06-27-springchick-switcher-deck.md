# springchick Switcher Deck Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an interactive fanned-stack task switcher: drag the bar up into the middle zone to reveal running apps as window cards, tap to switch (zoom from the card), swipe a card up to close, scroll to unfold the stack.

**Architecture:** Approach A — a new pure `switcher.rs` geometry module (fanned-stack card rects + hit-test, unit-tested) and a new `UiState::Switcher` holding the deck's live state. The grab middle-zone release (already classified `NavTarget::Switcher`) wires to the new state instead of Home. `scene.rs`/`render.rs` draw each card from the app's last-committed buffer (icon+name placeholder fallback), reusing M3's `RescaleRenderElement`/`RelocateRenderElement` transform path. `input_common.rs` routes scroll/tap/swipe-close/dismiss. `app_history` is the MRU ordering. The `AppOpening`/`AppClosing` zoom origin is generalized from a point to `{center, scale}` so card-zoom reuses the existing animation.

**Tech Stack:** Rust, Smithay 0.7 (GlesRenderer transformed-surface composite), Skia (placeholder cards + dimmed home), `sc-anim::Spring`, existing `sc-input` tracker/thresholds.

**Spec:** `docs/superpowers/specs/2026-06-27-springchick-switcher-deck-design.md`

**Run tests with:** `nix develop -c cargo test -p sc-compositor` (and `-p sc-input` etc. as noted). The dev host is aarch64; the nix dev shell provides the toolchain.

**Out of scope:** drag-down-to-cancel, multi-touch, live previews for backgrounded cards, manual card reordering, HiDPI/output scaling (separate spec).

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/sc-compositor/src/switcher.rs` (new) | Pure deck geometry: `CardRect`, `layout(cards, scroll, size)` (fanned stack → unfold → pan), `hit_test`. Unit-tested. No Smithay/GPU. |
| `crates/sc-compositor/src/ui_state.rs` (modify) | `ZoomOrigin{center,scale}` replacing `icon_center`; `UiState::Switcher`; switcher `UiEvent`s; entry/exit/scroll/close transitions; `Effect::CloseToplevel`. |
| `crates/sc-compositor/src/scene.rs` (modify) | `WindowTransform::from_zoom_progress` takes a `ZoomOrigin`; `compute_scene(Switcher)` returns ordered card transforms from `switcher::layout`. |
| `crates/sc-compositor/src/render.rs` (modify) | Draw the deck: dimmed home behind, cards back-to-front from last-committed buffers (placeholder when absent), bar on top. |
| `crates/sc-compositor/src/input_common.rs` (modify) | When in `Switcher`: press/move/release → scroll / tap-card / swipe-close / dismiss events. |
| `crates/sc-compositor/src/main.rs` (modify) | Build the deck from `app_history` on entry; apply `Effect::CloseToplevel` (`send_close`); tap-card → raise/zoom + history promote. |

Each task is independently committable. Tasks 1–4 are pure and TDD'd on the host. Tasks 5–7 wire input/render/main (build-verified + manual). Task 8 is manual verification.

---

## Task 1: Generalize the zoom origin (`icon_center` → `ZoomOrigin{center, scale}`)

Card-zoom interpolates from a card rect (center + scale ≈ 0.62), not a 0.1-scale icon. Replace the `(f32,f32)` origin with a small struct carrying the start scale. Pure refactor — icon launches keep scale 0.1, so behavior is unchanged.

**Files:**
- Modify: `crates/sc-compositor/src/ui_state.rs`
- Modify: `crates/sc-compositor/src/scene.rs`
- Modify: `crates/sc-compositor/src/input_dispatch.rs` (carries `icon_center` in `DownAction`/events)
- Modify: `crates/sc-compositor/src/main.rs` (construct origins)

- [ ] **Step 1: Add the `ZoomOrigin` type (ui_state.rs)**

```rust
/// Origin of a zoom animation: where the window grows from / shrinks to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomOrigin {
    /// Center in logical pixels.
    pub center: (f32, f32),
    /// Start scale (icon ≈ 0.1, switcher card ≈ 0.62).
    pub scale: f32,
}

impl ZoomOrigin {
    pub fn icon(center: (f32, f32)) -> Self {
        Self { center, scale: 0.1 }
    }
    pub fn card(center: (f32, f32), scale: f32) -> Self {
        Self { center, scale }
    }
}
```

- [ ] **Step 2: Replace `icon_center: (f32,f32)` with `origin: ZoomOrigin`**

In `UiState::{AppOpening, AppClosing, Settling}` and `UiEvent::{AppMapped, ReturnHome}` (and any other `icon_center` carrier), rename the field to `origin: ZoomOrigin`. Update `transition()` arms and the existing `(0.5,0.5)` placeholder in `GrabRelease` to `ZoomOrigin::icon((0.5, 0.5))`. Update all `#[cfg(test)]` constructions (e.g. `icon_center: (100.0, 200.0)` → `origin: ZoomOrigin::icon((100.0, 200.0))`).

- [ ] **Step 3: `from_zoom_progress` takes a `ZoomOrigin` (scene.rs)**

```rust
pub fn from_zoom_progress(progress: f32, origin: ZoomOrigin, width: f32, height: f32) -> Self {
    let p = progress.clamp(0.0, 1.0);
    let scale = origin.scale + p * (1.0 - origin.scale);   // origin.scale → 1.0
    let cx = origin.center.0 + (width / 2.0 - origin.center.0) * p;
    let cy = origin.center.1 + (height / 2.0 - origin.center.1) * p;
    let corner_radius = 24.0 * (1.0 - p);
    Self { scale, center_x: cx, center_y: cy, corner_radius }
}
```
Update `compute_scene` call sites to pass `*origin`.

- [ ] **Step 4: Update main.rs / input_dispatch origins**

Where launches build `icon_center` (`DownAction::LaunchApp`, `AppMapped`, `ReturnHome`, `last_icon_center`), wrap as `ZoomOrigin::icon(center)`. `State.last_icon_center` becomes `last_origin: ZoomOrigin` (icon by default).

- [ ] **Step 5: Run the existing tests — behavior unchanged**

Run: `nix develop -c cargo test -p sc-compositor`
Expected: all current tests pass (the refactor preserves icon-zoom behavior).

- [ ] **Step 6: Commit**

```bash
git add crates/sc-compositor/src
git commit -m "refactor: generalize zoom origin to ZoomOrigin{center,scale}

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: `switcher.rs` — fanned-stack geometry (pure, TDD)

**Files:**
- Create: `crates/sc-compositor/src/switcher.rs`
- Modify: `crates/sc-compositor/src/main.rs` (`mod switcher;`)

- [ ] **Step 1: Write failing tests**

```rust
// in switcher.rs
#[cfg(test)]
mod tests {
    use super::*;
    const SIZE: (f32, f32) = (1224.0, 2700.0);

    #[test]
    fn front_is_rightmost_when_folded() {
        let rects = layout(&[0, 1, 2], 0.0, SIZE);
        // cards[0] is the front; it sits furthest right and is largest / top z.
        let front = rects.iter().find(|r| r.toplevel == 0).unwrap();
        for r in &rects {
            if r.toplevel != 0 {
                assert!(front.center_x > r.center_x);
                assert!(front.scale >= r.scale);
                assert!(front.z >= r.z);
            }
        }
    }

    #[test]
    fn folded_stacks_tight_unfold_widens() {
        let folded = layout(&[0, 1, 2], 0.0, SIZE);
        let open = layout(&[0, 1, 2], 1.0, SIZE);
        let gap = |v: &[CardRect]| (v[0].center_x - v[1].center_x).abs();
        assert!(gap(&open) > gap(&folded)); // unfolding widens the x-gap
    }

    #[test]
    fn scroll_clamps_and_rubber_bands() {
        // Past max, positions keep moving but sub-linearly (rubber-band), never NaN.
        let a = layout(&[0, 1, 2], 5.0, SIZE);
        let b = layout(&[0, 1, 2], 50.0, SIZE);
        assert!(a.iter().all(|r| r.center_x.is_finite()));
        assert!(b.iter().all(|r| r.center_x.is_finite()));
    }

    #[test]
    fn single_card_centers() {
        let rects = layout(&[7], 0.0, SIZE);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].toplevel, 7);
    }

    #[test]
    fn empty_is_empty() {
        assert!(layout(&[], 0.0, SIZE).is_empty());
    }

    #[test]
    fn hit_test_picks_topmost() {
        let rects = layout(&[0, 1, 2], 1.0, SIZE);
        let front = rects.iter().max_by_key(|r| r.z).unwrap();
        match hit_test(&rects, front.center_x, front.center_y) {
            CardHit::Card(i) => assert_eq!(rects[i].toplevel, front.toplevel),
            _ => panic!("expected a card hit at the front card center"),
        }
    }

    #[test]
    fn hit_test_empty_off_card() {
        let rects = layout(&[0], 0.0, SIZE);
        assert!(matches!(hit_test(&rects, 5.0, 5.0), CardHit::Empty));
    }
}
```

- [ ] **Step 2: Run, verify they fail** — `nix develop -c cargo test -p sc-compositor switcher` → FAIL (undefined).

- [ ] **Step 3: Implement the geometry**

```rust
//! Pure switcher-deck geometry: fanned stack of window cards.
//!
//! `cards[0]` is the most-recent (front). `scroll` is a single continuous scalar:
//! 0 = folded (front on the right, older cards tucked behind to the left); increasing
//! first unfolds the stack into a spread, then pans left so earlier cards scroll into
//! view when the spread is wider than the screen.

use crate::ui_state::ToplevelId;

#[derive(Clone, Copy, Debug)]
pub struct CardRect {
    pub toplevel: ToplevelId,
    pub center_x: f32,
    pub center_y: f32,
    pub scale: f32,
    pub corner_radius: f32,
    pub z: usize,
    pub close_progress: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardHit {
    Card(usize),
    Empty,
}

const FRONT_SCALE: f32 = 0.62;
const DEPTH_SCALE_STEP: f32 = 0.06; // each card behind is this much smaller
const FOLDED_PEEK: f32 = 18.0;      // px of edge showing when stacked
const CORNER: f32 = 28.0;

/// Compute card rects, back-to-front. `cards[0]` = front.
pub fn layout(cards: &[ToplevelId], scroll: f32, size: (f32, f32)) -> Vec<CardRect> {
    let (w, h) = size;
    let n = cards.len();
    if n == 0 {
        return Vec::new();
    }
    let front_w = w * FRONT_SCALE;
    // Front card rests centered-right with a small right margin.
    let front_cx = w - front_w / 2.0 - w * 0.06;
    let cy = h / 2.0;

    // Spread distance per card grows with scroll: folded → just the peek; open → full card.
    let spread = soft_clamp(scroll); // 0..~1+ (rubber-band handled in soft_clamp)
    let gap = FOLDED_PEEK + spread * (front_w * 0.55);

    // Pan: once unfolded, extra scroll beyond the point where all cards fit pans left.
    let total_w = gap * (n as f32 - 1.0);
    let overflow = (total_w - (front_cx - w * 0.04)).max(0.0);
    let pan = (spread - 1.0).max(0.0) * overflow;

    cards
        .iter()
        .enumerate()
        .map(|(i, &toplevel)| {
            let depth = i as f32;
            CardRect {
                toplevel,
                center_x: front_cx - gap * depth + pan,
                center_y: cy,
                scale: (FRONT_SCALE - DEPTH_SCALE_STEP * depth).max(0.30),
                corner_radius: CORNER,
                z: n - i, // front (i=0) has the highest z
                close_progress: 0.0,
            }
        })
        .collect()
}

/// Topmost (highest z) card whose rect contains the point, else Empty.
pub fn hit_test(rects: &[CardRect], x: f32, y: f32) -> CardHit {
    let mut best: Option<usize> = None;
    for (i, r) in rects.iter().enumerate() {
        // Use the on-screen size from the panel aspect (cards keep the screen aspect).
        let cw = 1224.0 * r.scale; // see note: pass real size in if aspect differs
        let ch = 2700.0 * r.scale;
        let inside = (x - r.center_x).abs() <= cw / 2.0 && (y - r.center_y).abs() <= ch / 2.0;
        if inside && best.map_or(true, |b| r.z > rects[b].z) {
            best = Some(i);
        }
    }
    match best {
        Some(i) => CardHit::Card(i),
        None => CardHit::Empty,
    }
}

/// Map raw scroll to a spread factor with rubber-banding past the ends.
fn soft_clamp(scroll: f32) -> f32 {
    if scroll < 0.0 {
        scroll * 0.3
    } else {
        scroll // upper rubber-band applied by the caller's pan term; positions stay finite
    }
}
```

> Implementer note: `hit_test` needs the real output size to convert `scale` → pixel
> half-extents. Either pass `size` into `hit_test(rects, x, y, size)` or store `w_px/h_px` on
> `CardRect`. Pick one and make the tests match — do NOT hard-code 1224/2700 in shipping code;
> the constants above are a placeholder to be removed.

- [ ] **Step 4: Run tests, verify pass** — `nix develop -c cargo test -p sc-compositor switcher` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/switcher.rs crates/sc-compositor/src/main.rs
git commit -m "feat: pure fanned-stack switcher geometry + hit-test

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: `UiState::Switcher` + events + transitions (pure, TDD)

**Files:**
- Modify: `crates/sc-compositor/src/ui_state.rs`

- [ ] **Step 1: Write failing transition tests**

```rust
#[test]
fn switcher_preview_release_enters_switcher() {
    // From a grab released in the switcher settle zone, build the deck.
    let mut state = UiState::App { toplevel: 1, app_id: "a".into() };
    transition(&mut state, UiEvent::EnterSwitcher { cards: vec![1, 2, 3] });
    assert!(matches!(state, UiState::Switcher { .. }));
    if let UiState::Switcher { cards, .. } = &state {
        assert_eq!(cards, &vec![1, 2, 3]);
    }
}

#[test]
fn tap_card_opens_that_app() {
    let mut state = UiState::Switcher {
        cards: vec![1, 2, 3], scroll: Spring::new(0.0), drag: None,
    };
    let eff = transition(&mut state, UiEvent::SwitcherTapCard {
        index: 1, origin: ZoomOrigin::card((600.0, 1350.0), 0.62),
    });
    assert!(matches!(state, UiState::AppOpening { toplevel: 2, .. }));
    assert_eq!(eff, Effect::None);
}

#[test]
fn close_card_removes_and_emits_effect() {
    let mut state = UiState::Switcher {
        cards: vec![1, 2, 3], scroll: Spring::new(0.0), drag: None,
    };
    let eff = transition(&mut state, UiEvent::SwitcherCloseCard { index: 1 });
    assert_eq!(eff, Effect::CloseToplevel { toplevel: 2 });
    if let UiState::Switcher { cards, .. } = &state {
        assert_eq!(cards, &vec![1, 3]);
    } else { panic!("still in switcher"); }
}

#[test]
fn close_last_card_goes_home() {
    let mut state = UiState::Switcher {
        cards: vec![9], scroll: Spring::new(0.0), drag: None,
    };
    transition(&mut state, UiEvent::SwitcherCloseCard { index: 0 });
    assert!(matches!(state, UiState::Home { .. }));
}

#[test]
fn dismiss_goes_home() {
    let mut state = UiState::Switcher {
        cards: vec![1, 2], scroll: Spring::new(0.0), drag: None,
    };
    transition(&mut state, UiEvent::SwitcherDismiss);
    assert!(matches!(state, UiState::Home { .. }));
}

#[test]
fn toplevel_closed_removes_card() {
    let mut state = UiState::Switcher {
        cards: vec![1, 2, 3], scroll: Spring::new(0.0), drag: None,
    };
    transition(&mut state, UiEvent::ToplevelClosed { toplevel: 2 });
    if let UiState::Switcher { cards, .. } = &state {
        assert_eq!(cards, &vec![1, 3]);
    } else { panic!("expected still switcher"); }
}
```

- [ ] **Step 2: Run, verify fail.**

- [ ] **Step 3: Implement state + events + transitions**

Add to `UiState`:
```rust
Switcher {
    cards: Vec<ToplevelId>,      // MRU; cards[0] = front
    scroll: Spring,              // single unfold-then-pan scalar
    drag: Option<SwitcherDrag>,  // active press: card + axis
},
```
```rust
#[derive(Clone, Debug)]
pub struct SwitcherDrag {
    pub card: Option<usize>,     // None = pressed empty area
    pub axis: DragAxis,
    pub close_progress: f32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragAxis { Undecided, Scroll, Close }
```
Add events:
```rust
EnterSwitcher { cards: Vec<ToplevelId> },
SwitcherScroll { delta: f32 },
SwitcherTapCard { index: usize, origin: ZoomOrigin },
SwitcherCloseCard { index: usize },
SwitcherDismiss,
```
Add to `Effect`:
```rust
CloseToplevel { toplevel: ToplevelId },
```
Transitions in `transition()`:
- `EnterSwitcher { cards }` → `UiState::Switcher { cards, scroll: Spring::new(0.0), drag: None }`.
- `SwitcherScroll { delta }` (in Switcher) → `scroll.value += delta; scroll.target = scroll.value; scroll.velocity = 0` (finger-follow like page-drag).
- `SwitcherTapCard { index, origin }` (in Switcher) → start `AppOpening { toplevel: cards[index], origin, .. }` (reuse the existing opening spring setup).
- `SwitcherCloseCard { index }` → remove `cards[index]`; if empty → `home(...)`; else stay; return `Effect::CloseToplevel { toplevel }`.
- `SwitcherDismiss` → `home(...)`.
- Extend `ToplevelClosed` to also handle the `Switcher` case (remove the card; empty → home).
- Extend `Tick` to advance `scroll` (and any `drag.close_progress` spring-back) in `Switcher`.
- `needs_animation()` / `foreground_toplevel()` get `Switcher` arms (animating while scroll unsettled; foreground = `cards.first()`).

Wire entry from the grab: in `GrabRelease`, when `classify_release` returns `NavTarget::Switcher`, do **not** build `Settling`; instead the caller (main/input) emits `EnterSwitcher` with the history. (Keep `transition` pure — it can't read `app_history`; the cards list is supplied by the caller. See Task 5/7.)

- [ ] **Step 4: Run tests, verify pass.**

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/ui_state.rs
git commit -m "feat: UiState::Switcher with entry/tap/close/dismiss/scroll transitions

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: `scene.rs` — render the deck (pure, TDD)

**Files:**
- Modify: `crates/sc-compositor/src/scene.rs`

- [ ] **Step 1: Failing test**

```rust
#[test]
fn switcher_scene_has_cards_back_to_front() {
    let state = UiState::Switcher {
        cards: vec![0, 1, 2], scroll: sc_anim::Spring::new(0.0), drag: None,
    };
    let scene = compute_scene(&state, TEST_SIZE);
    assert_eq!(scene.cards.len(), 3);
    assert!(scene.show_home);                 // dimmed home behind
    // sorted back-to-front by z (ascending z order for draw)
    assert!(scene.cards.windows(2).all(|w| w[0].z <= w[1].z));
}
```

- [ ] **Step 2: Extend `Scene`**

Add `pub cards: Vec<switcher::CardRect>` to `Scene` (empty for non-switcher states). In `compute_scene`, add a `UiState::Switcher` arm that calls `switcher::layout(cards, scroll.value, size)`, sorts the result ascending by `z`, sets `show_home = true` (dim flag), and `window = None`. Default `cards: Vec::new()` in all other arms.

- [ ] **Step 3: Run, pass. Step 4: Commit**

```bash
git add crates/sc-compositor/src/scene.rs
git commit -m "feat: compute_scene returns switcher card transforms

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: `input_common.rs` — switcher input routing

**Files:**
- Modify: `crates/sc-compositor/src/input_common.rs`
- Modify: `crates/sc-compositor/src/main.rs` (emit `EnterSwitcher` with history on grab release)

- [ ] **Step 1: Entry — emit cards on grab release**

In `on_release` (the grab path), when `classify_release` yields `NavTarget::Switcher`, build the deck list from `state.history.stack` (front = current) and `transition(EnterSwitcher { cards })` instead of the `GrabRelease`/Settling path. Keep the other `NavTarget`s as today.

- [ ] **Step 2: In-switcher press/move/release**

Add a branch: when `state.ui` is `Switcher`:
- **press(x,y):** `hit_test` the current scene cards (compute via `switcher::layout`) → record `SwitcherDrag { card, axis: Undecided }` on the state; remember origin point.
- **move(x,y):** if axis `Undecided`, decide by first significant delta (horizontal-dominant → `Scroll`, vertical-up on a card → `Close`); then `Scroll` → `SwitcherScroll { delta }`; `Close` → grow that card's `close_progress`.
- **release(x,y):** small move on a card → `SwitcherTapCard { index, origin: card-rect center+scale }`; past close threshold → `SwitcherCloseCard { index }`; horizontal → settle scroll spring with `tracker.velocity`; empty tap → `SwitcherDismiss`.

Reuse the existing `Tracker` for velocity/up-progress (same as grab). Thresholds: reuse `QUICK_SWITCH_PROGRESS` for axis dominance and a new `SWITCHER_CLOSE_PROGRESS = 0.4` in `sc-input::thresholds`.

- [ ] **Step 3: Build-verify + winit manual**

Run: `nix develop -c cargo build -p sc-compositor` → builds.
Manual (winit, mouse-as-touch): drag bar up & hold → deck appears; drag horizontally → unfolds/scrolls.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/input_common.rs crates/sc-compositor/src/main.rs crates/sc-input/src/thresholds.rs
git commit -m "feat: route switcher input (scroll/tap/close/dismiss) + entry from grab

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: `render.rs` — draw the deck

**Files:**
- Modify: `crates/sc-compositor/src/render.rs`
- Modify: `crates/sc-compositor/src/skia_gl.rs` (dimmed-home + placeholder card helpers)

- [ ] **Step 1: Draw order in `draw_scene`**

When `scene.cards` is non-empty:
1. Draw the home screen via Skia (existing `draw_home`), **dimmed** — add a `dim: f32` arg or draw a translucent dark rect over it.
2. For each `CardRect` (already back-to-front): resolve the toplevel's surface; if it has a committed buffer, build `render_elements_from_surface_tree` and apply the existing `Relocate`+`Rescale` to the card center/scale + `clipRRect`-equivalent corner rounding (same code path as the M3 scaled-window pass, factored into a helper `draw_card(renderer, fb, surface, rect)`). If no buffer, draw a Skia placeholder (rounded panel + app icon + name).
3. Draw the bar on top.

The per-card surface lookup needs the `toplevels` map; pass a resolver closure or a `&[Option<(ToplevelId, WlSurface)>]` into `DrawCtx` for the switcher (the main render already resolves `app_surface` — generalize it to resolve all card surfaces).

- [ ] **Step 2: Skia helpers (skia_gl.rs)**

`draw_home(..., dim)` (or a `draw_dim_overlay`) and `draw_card_placeholder(width, height, rect, icon, name, flip_y)`. Respect `flip_y` like the existing draws.

- [ ] **Step 3: Build-verify** — `nix develop -c cargo build -p sc-compositor`.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/render.rs crates/sc-compositor/src/skia_gl.rs
git commit -m "feat: render switcher deck — cards from last buffers + dimmed home

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: `main.rs` — close + tap-to-switch wiring

**Files:**
- Modify: `crates/sc-compositor/src/main.rs`

- [ ] **Step 1: Apply `Effect::CloseToplevel`**

Wherever `transition()` is called and returns an `Effect`, handle `Effect::CloseToplevel { toplevel }` by looking up `self.toplevels[toplevel]` and calling `surface.send_close()`. The actual destruction arrives later via `toplevel_destroyed` → `ToplevelClosed` (idempotent: the card is already removed).

- [ ] **Step 2: Tap-to-switch history promote**

`SwitcherTapCard` produces `AppOpening` for `cards[index]`. After the transition, `self.history.push_foreground(toplevel)` so MRU stays correct (front of the deck next time).

- [ ] **Step 3: Build + tests**

Run: `nix develop -c cargo build -p sc-compositor && nix develop -c cargo test -p sc-compositor` → builds, all pass.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/main.rs
git commit -m "feat: wire switcher close (send_close) + tap-to-switch history promote

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Manual verification (winit + device)

- [ ] **Step 1: Winit harness**

Run: `nix develop -c cargo run -p sc-compositor`. Launch 2–3 clients (e.g. `foot`, weston-terminal). Verify: grab bar, drag up & hold → deck (front-right, others stacked left); drag horizontally → unfold + scroll; tap a card → zoom-opens that app; swipe a card up → closes it; tap empty → home; close last card → home.

- [ ] **Step 2: On-device (per `docs/RUNBOOK-device.md`)**

Run the seatd-over-SSH flow; confirm the deck renders right-side-up, no tearing, touch drives the full switcher loop, and the perf line stays within budget with several cards.

- [ ] **Step 3: Final clippy/test sweep + commit any fixes**

Run: `nix develop -c cargo clippy --all-targets` and `nix develop -c cargo test` → clean.

---

## Done

All tasks committed, `switcher.rs`/`ui_state`/`scene` unit tests green, and the manual loop works in winit and on-device. The switcher is reachable (drag-up-hold), interactive (scroll/tap/close), and integrates with the existing zoom-open and `app_history`.
