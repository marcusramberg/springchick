# springchick Milestone 3 — Bottom-Bar Gestures + App Transitions

**Date:** 2026-06-27
**Status:** Draft
**Builds on:** M2 (calloop compositor, home screen, tap-to-launch, fullscreen compositing,
page swipe, placeholder return-home via bar-tap / Esc).

## Summary

M3 replaces M2's placeholder return-home tap with the **defining bottom-bar navigation
gesture** from the design spec: grab → shrink → settle, with quick-switch and live
switcher preview. It also adds the **app-open zoom animation** (icon → fullscreen) and
**app-close shrink** (fullscreen → icon). The gesture is interruptible: re-grabbing
mid-animation cancels the spring and re-attaches to the finger.

After M3, the core interaction loop is complete: home → tap icon (zoom open) → use app →
grab bar (shrink/fling) → home or switcher or adjacent app, all spring-animated at 90 Hz.

## Architecture

### What exists from M1/M2 (reused directly)

- **`sc-input`** — `Tracker` (normalized touch + velocity), `NavState` (Idle/Grabbing/
  SwitcherPreview/QuickSwitching), `NavTarget` (BackToApp/Home/Switcher/QuickSwitch),
  `classify_release`, thresholds. All pure, unit-tested.
- **`sc-anim`** — `Spring` (critically-damped, interruptible retarget).
- **`sc-layout`** — `compute()` gives icon rects (needed for zoom-to-icon origin).
- **`sc-compositor`** — calloop loop, Smithay Wayland server, Skia rendering, `UiState`
  enum, foreground toplevel compositing.

### New/extended modules

- **`sc-compositor::ui_state`** (extend): new states for transition animations.
- **`sc-compositor::scene`** (new module): owns the animated scene — window transform
  (scale, position, corner-radius), home-icon zoom origin, switcher card layout. Pure
  struct updated each frame from springs; the render pass reads it.
- **`sc-compositor::input_dispatch`** (new module): wires winit/touch events into
  `sc-input::Tracker`, feeds results into `UiState` transitions + `scene` spring targets.
  Replaces the ad-hoc pointer handling in M2's main.rs.

### UiState extensions

```rust
enum UiState {
    Home { page, page_spring, page_count },
    App { toplevel, app_id },

    // NEW: transition states
    AppOpening {
        toplevel: ToplevelId,
        app_id: String,
        /// Spring 0→1 driving the zoom from icon rect to fullscreen.
        progress: Spring,
        /// Icon center (origin of the zoom).
        icon_center: (f32, f32),
    },
    AppClosing {
        toplevel: ToplevelId,
        app_id: String,
        /// Spring 1→0 driving the shrink from fullscreen to icon rect.
        progress: Spring,
        icon_center: (f32, f32),
    },
    Grabbing {
        toplevel: ToplevelId,
        app_id: String,
        /// The sc-input Tracker (normalized coords).
        tracker: Tracker,
        /// Live nav state (from sc-input::live_state).
        nav: NavState,
    },
    Settling {
        toplevel: ToplevelId,
        app_id: String,
        target: NavTarget,
        /// Spring driving the settle animation toward the target.
        progress: Spring,
    },
}
```

### Scene state (per-frame snapshot for rendering)

```rust
struct Scene {
    /// The foreground window transform (applied to the composited texture).
    window: WindowTransform,
    /// Switcher cards (visible during SwitcherPreview / Settling→Switcher).
    cards: Vec<CardTransform>,
    /// Whether to draw the home screen behind (partially visible during shrink).
    show_home: bool,
}

struct WindowTransform {
    /// Scale 0..1 (1 = fullscreen, ~0.5 = card size).
    scale: f32,
    /// Center position (logical px).
    center: (f32, f32),
    /// Corner radius (0 = sharp when fullscreen, grows as shrinks).
    corner_radius: f32,
}

struct CardTransform {
    toplevel: ToplevelId,
    scale: f32,
    center: (f32, f32),
    corner_radius: f32,
}
```

## Gesture flow (detailed)

### 1. App-open (tap icon on home screen)

1. `UiState::Home` + tap icon → `UiState::AppOpening { progress: Spring(0→1) }`.
2. Each frame: advance spring, compute `WindowTransform` interpolated from icon_rect →
   fullscreen. Corner radius shrinks from ~24px → 0. Window texture appears immediately
   (even if client hasn't rendered yet — show last buffer or a solid color).
3. Spring settles → `UiState::App`.

### 2. Grab (finger down on bar zone while in App state)

1. `UiState::App` + touch-down in bar zone → `UiState::Grabbing`.
2. Each frame: `tracker.update(point, dt)` → `live_state(tracker)` gives `NavState`.
3. Render: window texture transforms to track finger Y (scale = 1 - progress * 0.5),
   position shifts up, corner radius grows. If `NavState::SwitcherPreview`, fan in
   neighbor cards on either side.
4. If `NavState::QuickSwitching`, slide current window off in the drag direction, bring
   adjacent app's last texture from the other side.

### 3. Release → settle

1. Finger up → `classify_release(tracker)` → `NavTarget`.
2. `UiState::Grabbing` → `UiState::Settling { target, progress }`.
3. Spring drives toward target:
   - **BackToApp:** progress → 0 (window zooms back to fullscreen) → `UiState::App`.
   - **Home:** progress → 1 (window shrinks to icon origin) → `UiState::Home`.
   - **Switcher:** progress → switcher-rest-position → (future: `UiState::Switcher`,
     M3 MVP just goes Home since full switcher deck is complex).
   - **QuickSwitch(dir):** slide-out animation → `UiState::App` with the adjacent
     toplevel raised.

### 4. Interruptible

- Touch-down during `Settling` → cancel spring, return to `Grabbing` at current
  transform. `tracker.begin(current_point)` preserves visual continuity.
- Touch-down during `AppOpening` → cancel, jump to `App` (skip rest of zoom).
- Touch-down during `AppClosing` → cancel, jump to `App` (re-grab).

## Rendering changes

- **AppOpening/AppClosing/Grabbing/Settling:** composite the window texture with the
  `WindowTransform` (scale + translate + clip to rounded rect). Skia's `canvas.save()` +
  `canvas.scale/translate` + `clipRRect` on the texture draw.
- **During shrink:** draw the home screen *behind* the shrinking window (partially
  visible). Render order: home → app texture (transformed) → bar.
- **Switcher preview cards:** draw 1-2 neighbor app last-buffers as smaller cards flanking
  the main card. These don't need to be live-updating in M3 (static last-buffer is fine).
- **Quick-switch:** both the outgoing and incoming window textures drawn with opposing
  horizontal transforms.

## Input dispatch rewrite

Replace the ad-hoc `handle_winit_input` with a structured dispatch:

1. All pointer/touch events → normalized `Pt` (0..1 coords).
2. Route based on `UiState`:
   - `Home` → page drag or icon tap (existing M2 logic, cleaned up).
   - `App` → check bar zone for grab start; else forward to client.
   - `Grabbing` → update tracker, recompute nav state.
   - `Settling` → touch-down interrupts.
   - `AppOpening`/`AppClosing` → touch-down interrupts.
3. Keyboard Esc → still works as return-home shortcut in dev.

## Quick-switch: app history

Need a simple app-history stack to know which app is "previous" and "next":

```rust
struct AppHistory {
    /// Most-recently-used order. Front = current foreground.
    stack: Vec<ToplevelId>,
}
```

QuickSwitch(-1) raises `stack[1]` (previous app). QuickSwitch(+1) raises `stack[2]` or
wraps. Updated on every `App` transition.

## Testing strategy

- **`sc-input`** (already done): gesture classification covers all release targets.
- **`ui_state` transitions** (pure): tap→AppOpening→(settled)→App,
  grab→Grabbing→(release)→Settling→Home, interrupt scenarios.
- **`scene` computation** (pure): given UiState + springs → correct WindowTransform at
  various progress values (0, 0.5, 1). Corner radius interpolation. Card positions.
- **Spring convergence**: verify AppOpening/Closing settle within expected frame count.
- **Integration (manual):** nested winit — grab bar, drag up, release → animates home.
  Tap icon → zoom open. Quick-switch gesture.

## Scope

**In (M3):**
- Bottom-bar grab gesture (touch-down in bar zone detaches window).
- Live window transform tracking finger (shrink + translate + corner radius).
- Release classification → spring-animated settle to target.
- App-open zoom animation (icon → fullscreen).
- App-close shrink animation (fullscreen → icon).
- Quick-switch (horizontal flick switches adjacent app).
- Interruptible: re-grab cancels any settle/zoom animation.
- Switcher preview (neighbor cards fan in during drag) — static textures.
- App history stack for quick-switch ordering.
- Structured input dispatch replacing M2 ad-hoc handler.

**Out (later):**
- Full interactive switcher deck with tap-to-select (M3.5 or M5).
- Device backend / DRM + on-device perf validation (M4). *(promoted ahead of edit mode —
  validate animation perf on real hardware before adding shell features.)*
- Edit mode / folders / page-reorder (M5).

## Key risks

- **Transform rendering performance.** Skia `clipRRect` + scale on a fullscreen texture
  every frame at 90 Hz. Mitigation: the texture is a single Smithay-composited buffer,
  not re-rendered per frame. Skia just samples it at the transformed coordinates. This is
  the same workload as iOS/Android animation compositors.
- **Spring feel tuning.** The gesture must feel right — too slow is sluggish, too fast is
  jarring. Mitigation: all spring constants in `sc-anim` are hot-tunable; the nested winit
  harness enables rapid iteration.
- **Quick-switch with few apps.** If only one app is running, quick-switch is a no-op.
  Need graceful fallback (rubber-band and snap back).
- **Last-buffer retention for switcher cards.** Backgrounded apps don't receive frame
  callbacks, so their last buffer stays valid. But if a client destroys its buffer on
  background, we lose the texture. Mitigation: copy the texture on background transition
  (or accept a blank card — acceptable for M3).
