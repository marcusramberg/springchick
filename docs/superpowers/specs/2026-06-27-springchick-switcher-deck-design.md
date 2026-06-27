# springchick — Interactive Switcher Deck Design

**Date:** 2026-06-27
**Status:** Draft
**Builds on:** M3 (gestures/transitions: grab → shrink → settle, app-open/close zoom,
quick-switch, interruptible springs) and M4 (DRM device backend; shared `render.rs`
transformed-surface compositing; `DrawCtx` with `transform`/`skia_flip_y`).

## Summary

Today, switching between running apps is only possible via the horizontal bar quick-switch
(flick left/right through the MRU history). This adds an **interactive switcher deck**: drag
the bottom bar up into a middle zone to reveal the running apps as a **fanned stack** of
window cards, then tap a card to switch, swipe a card up to close it, or scroll to unfold the
stack. The quick-switch flick stays as-is; the switcher is an addition.

The switcher deck was deferred from M3 (`NavTarget::Switcher` exists but `Settling` currently
treats it as Home; `NavState::SwitcherPreview` exists but no cards are rendered). This design
implements it.

## UX

- **Invoke:** grab the bar, drag up. A **middle zone** reveals the deck (the current window
  shrinks into the front card while older windows peek, stacked behind to the left). Drag
  *further* up → Home. **Release in the middle zone → stay in the switcher.** Release below →
  back to the app.
- **Fanned stack:** the most-recent window is the front card, on the right; older windows are
  tucked behind it, stacked leftward with only their edges peeking. **Scroll left** unfolds the
  stack into a spread so each card is individually selectable.
- **Tap a card** → switch to that app (zoom-open from the card).
- **Swipe a card up** → close that app.
- **Tap empty area** → Home.
- **Scroll left/right** → a single continuous `scroll` scalar: from folded it first *unfolds*
  the stack, then (if there are more cards than fit) *pans* the spread so earlier cards come
  into view. Free scroll with momentum; rubber-bands at the ends.

**Deferred (YAGNI):** drag-down-to-cancel, multi-touch, live previews for backgrounded cards,
manual card reordering.

## Architecture

Approach A (chosen): a new pure geometry module plus a new `UiState`, mirroring the existing
`sc-layout` / `scene` / `ui_state` split. Card rendering reuses M3's transformed-surface
composite path. Rejected: folding card layout + deck state into `scene.rs` (bloats it, mixes
concerns); adopting Smithay `Space` (overkill, replaces the hand-rolled compositing M3/M4 rely
on).

### Module layout

```
crates/sc-compositor/src/
  switcher.rs   NEW  Pure deck geometry + hit-testing. No Smithay/GPU. Unit-tested.
  ui_state.rs   MOD  UiState::Switcher + switcher UiEvents; SwitcherPreview-release → Switcher.
  scene.rs      MOD  compute_scene(Switcher) → ordered card transforms (from switcher.rs).
  render.rs     MOD  Draw each card from its last-committed buffer (or Skia placeholder).
  input_common.rs MOD Route press/move/release to switcher events when in Switcher.
  app_history.rs     Reused as the card ordering source (front = most recent).
```

`switcher.rs` owns geometry + hit-testing only. `ui_state` owns the deck's live state +
transitions. `scene`/`render` consume geometry to draw. The only Smithay coupling is the
existing per-surface render path.

## `switcher.rs` — card geometry (pure)

```rust
pub struct CardRect {
    pub toplevel: ToplevelId,
    pub center_x: f32,
    pub center_y: f32,
    pub scale: f32,          // card size vs fullscreen (~0.62 for the front card)
    pub corner_radius: f32,
    pub z: usize,            // draw order; front-most drawn last
    pub close_progress: f32, // 0 = normal, →1 = sliding up to close (lift + fade)
}

pub enum CardHit { Card(usize), Empty }

/// `cards[0]` is the most-recent (front). `scroll` is a single continuous scalar:
/// 0 = folded (front-right, rest tucked left); increasing first unfolds the stack into a
/// spread, then pans the spread leftward so earlier cards come into view when they overflow
/// the screen. `size` is the logical output size.
pub fn layout(cards: &[ToplevelId], scroll: f32, size: (f32, f32)) -> Vec<CardRect>;

/// Topmost card whose rect contains the point, else Empty.
pub fn hit_test(rects: &[CardRect], x: f32, y: f32) -> CardHit;
```

**Fan math:** the front card (index 0) sits centered-right at `scale≈0.62`. Each older card
`i` is offset left by `peek_x * spread(scroll)` and scaled down by a small per-depth step, with
increasing `z` toward the front. At `scroll = 0` the cards are tightly stacked (edges peek,
~18px); as `scroll` grows the per-card horizontal gap widens to a full spread where every card
is tappable, and beyond full spread the whole row pans left so off-screen earlier cards scroll
into view (handles N cards wider than the screen). `scroll` clamps to `[0, max]`, where `max`
brings the oldest card fully on-screen (= 0 once everything already fits); past the ends it
rubber-bands. Cards are vertically centered. `close_progress` lifts a card upward (`center_y -=
close_progress * H`) and fades it for the swipe-to-close animation.

**Edge cases:** 0 cards → empty deck (caller exits to Home); 1 card → only the front, scroll is
a no-op rubber-band.

**Tests:** front is rightmost; ordering follows history; folded stacks tight; increasing scroll
widens the x-gap monotonically; scroll clamps + rubber-bands; 0/1/N-card cases; `hit_test`
picks the topmost card at overlapping points and returns `Empty` off-card; `close_progress`
lifts a card.

## `ui_state.rs` — state & transitions

New state:

```rust
UiState::Switcher {
    cards: Vec<ToplevelId>,     // MRU order; cards[0] = front/current
    scroll: Spring,             // single unfold-then-pan scalar (see switcher::layout)
    drag: Option<SwitcherDrag>, // active press: which card + axis (scroll vs close)
}
```

New events: `SwitcherScroll { dx }`, `SwitcherTapCard { i }`, `SwitcherCloseCard { i }`,
`SwitcherDismiss`, plus the existing `Tick` advancing the `scroll` spring and any
`close_progress`.

**Entry:** on `GrabRelease` classified `NavTarget::Switcher`, build `UiState::Switcher` from
`app_history` (front = current) instead of going Home. The front card continues from the grab
transform and settles to its deck rest rect; the deck becomes interactive.

**Exits:**
- `SwitcherTapCard { i }` → `AppOpening` for `cards[i]`, **zoom origin = that card's rect**;
  promote `cards[i]` in `app_history`.
- `SwitcherCloseCard { i }` → send xdg `close` to `cards[i]`, animate the card up/out, remove
  it from `cards` immediately (don't wait for client teardown). If `cards` is now empty → Home.
- `SwitcherDismiss` → animate the deck out → Home.
- `ToplevelClosed { toplevel }` while in `Switcher` → remove that card from `cards` live; empty
  → Home.

**`AppOpening`/`AppClosing` generalization:** their zoom origin field (currently `icon_center`)
becomes a generic origin rect/point so card-zoom reuses the existing animation states — no new
animation machinery.

**Tests (pure):** SwitcherPreview-release → Switcher; tap card → AppOpening with that toplevel +
card-rect origin + history promoted; close card → removed (last → Home); dismiss → Home;
ToplevelClosed mid-switcher removes the card; scroll updates the spring.

## Input (`input_common.rs`, in `UiState::Switcher`)

- **Press** on a card → record `drag_origin` + card index (tentative). Press on empty →
  tentative dismiss.
- **Move:** first significant delta picks the axis (reusing `sc-input`'s axis-dominance idea,
  like grab-vs-page-swipe today):
  - horizontal → `SwitcherScroll { dx }`: set `scroll` to follow the finger (rubber-band past
    ends).
  - vertical-up on the pressed card → grow that card's `close_progress` tracking the finger.
- **Release:**
  - small movement on a card → `SwitcherTapCard { i }`.
  - card lifted past the close threshold (`close_progress > 0.4` or a fast up-flick) →
    `SwitcherCloseCard { i }`; otherwise spring `close_progress` back to 0.
  - horizontal scroll → settle the `scroll` spring with momentum from `tracker.velocity` (free
    scroll, no paging).
  - tap on empty area → `SwitcherDismiss`.

One finger only (multi-touch stays out).

## Rendering (`scene.rs` + `render.rs`)

`compute_scene(Switcher)` returns the ordered (back-to-front by `z`) card transforms from
`switcher::layout`. The render walk:

- **Per card:** resolve the app's surface; build elements via
  `render_elements_from_surface_tree` on its **last-committed buffer** (backgrounded apps keep
  their last buffer — no frame callback needed), then apply the existing `Relocate` + `Rescale`
  path to the card rect + rounded corners (same machinery as M3's shrinking-window composite,
  looped per card).
- **No buffer** (client never drew / released it) → Skia placeholder card: app icon + name on a
  rounded panel (reuses the icon cache).
- **Background:** the home screen (Skia) drawn behind the deck, dimmed, so tap-empty → Home
  reads visually.
- **Front card** is the live current app (its real surface); other cards are static
  last-buffers (no live updating — matches the M3 switcher-preview decision).
- **Draw order:** dimmed home → cards back-to-front → bar. Card draws respect the
  `transform` / `skia_flip_y` already threaded through `DrawCtx`.

**Frame callbacks:** only the front (live) app receives them while in the switcher; backgrounded
apps stay frozen (intended).

**Perf:** N cards = N transformed texture draws per frame — same per-card cost as M3's
single-window transform. A handful of cards stays well within the ~5 ms/frame budget measured
on-device in M4.

## Testing strategy

- **`switcher.rs` (pure, primary):** geometry + hit-test as above.
- **`ui_state` transitions (pure):** entry/exit/close/dismiss/scroll as above.
- **`scene` (pure):** `compute_scene(Switcher)` → card transforms back-to-front; geometry
  matches `switcher::layout`; dimmed home behind.
- **Spring/settle:** entry settles; scroll momentum settles; close animation completes within
  expected frame count.
- **Manual (winit + device):** drag-up-hold → deck; scroll-left unfolds; tap switches (zoom
  from card); swipe-up closes; tap-empty → Home.

## Scope

**In:** fanned-stack switcher (drag-up-hold to enter, release-in-zone to stay); scroll to
unfold; tap-to-switch (zoom from card); swipe-card-up to close; tap-empty → Home; horizontal
scroll with momentum; card previews from last-committed buffers with icon/name placeholder
fallback; `AppOpening`/`AppClosing` origin generalized for card-zoom; pure `switcher.rs`
geometry + tests.

**Out (later):** drag-down-to-cancel; multi-touch; live previews for backgrounded cards;
manual card reordering; HiDPI/output-scale (separate spec, next).

## Key risks

- **Last-buffer retention.** A client that destroys its buffer on background leaves a card with
  no texture. Mitigation: icon+name placeholder (acceptable); a future snapshot-on-background
  copy if needed.
- **Axis disambiguation.** Scroll-vs-close-vs-tap from one finger must feel right. Mitigation:
  reuse the existing first-significant-delta dominance logic already proven for grab vs
  page-swipe; thresholds hot-tunable.
- **Close/teardown race.** Removing a card from the deck before the client finishes closing.
  Mitigation: drop from `cards` immediately; treat the later `ToplevelClosed` as confirmation
  (idempotent).
- **Deck empties mid-interaction.** Closing the last card, or all apps exiting, must land
  cleanly on Home. Covered by the empty-deck → Home rule + tests.
