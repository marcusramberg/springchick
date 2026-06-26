# springchick Milestone 2 — Home Screen + Launch: Design Spec

**Date:** 2026-06-27
**Status:** Approved for planning
**Builds on:** M1 Foundation (`2026-06-26-springchick-design.md`) — pure-logic crates
(`sc-anim`, `sc-input`, `sc-shell-model`, `sc-config`) and the `sc-compositor` binary
(Smithay 0.7 winit backend + proven Skia-on-Smithay-GLES shared-context rendering).

## Summary

M2 turns the M1 spike into a usable Springboard: the home screen renders the user's real
installed apps (icons, labels, paginated grid + dock), tapping an icon launches a real
Wayland app composited fullscreen, and a placeholder affordance returns home. Launched
apps receive touch + keyboard input. Priority remains smooth rendering, but M2's job is
**a working home→app→home loop**, not final polish.

The defining bottom-bar navigation gestures, app-open zoom animation, edit mode, and the
device backend remain in later milestones.

## Architecture

### Backbone: Smithay's calloop architecture

M1's compositor used a self-contained winit pump loop with no Wayland server. M2 adopts
Smithay's idiomatic **calloop** architecture (a trimmed `anvil`). A single `State` struct
holds the Wayland globals, running toplevels, the `ShellModel`, the renderer + `SkiaGl`,
and the UI state. calloop event sources:

- **winit** backend events (dev) — later swapped for DRM/libinput (M5) under the same loop.
- the **Wayland socket** — client connections and requests.
- a **~90 Hz render timer** — drives the frame callback.

This is the idiomatic path and sets up M3 (gestures need real toplevels) and M5 (device
backend swaps in under the same calloop) cleanly. Rejected alternative: keep the M1 pump
loop and dispatch a `Display` manually per frame — fights Smithay's grain and would be
rebuilt anyway.

### Module structure

Two new **pure** crates (no GPU/Wayland deps, unit-tested) plus integration in
`sc-compositor`:

- **`sc-layout`** (new, pure): geometry + hit-testing. `(output size, page index,
  ShellModel) → Layout` giving on-screen rects for each icon, the dock, and the bottom
  bar; and the inverse `point → Hit` (icon / dock slot / bar zone / miss). Resolution-
  independent; heavily unit-tested.
- **`sc-icons`** (new, pure-ish IO): resolve an `AppId`/icon name → decoded RGBA pixels.
  Pragmatic theme lookup + `resvg` for SVG + first-letter placeholder fallback. Returns
  raw pixels (NO Skia dep); `sc-compositor` uploads to a GL/Skia image. Tested against
  fixture icon dirs.
- **`sc-compositor`** (extend): the calloop `State`, Wayland handlers (compositor,
  xdg-shell, seat, shm, dmabuf), render orchestration, input routing, and app launching.

The split keeps fiddly geometry and icon logic in fast pure tests, leaving only genuine
integration glue in the compositor.

## UI state & core loop

A small, pure-testable state enum drives the render callback:

```
UiState::Home { page: usize, swipe: Option<PageDrag> }
UiState::App  { toplevel: ToplevelId }
```

- **Home:** Skia draws the current page's icons (rects from `sc-layout`, textures from
  `sc-icons`), the dock, and the bottom bar. Horizontal touch drag updates `swipe`; on
  release a `sc-anim` spring snaps to the nearest page.
- **Tap an icon** → launch or raise that app → `App`.
- **App:** Smithay composites that toplevel's texture fullscreen.
- **Return-home placeholder:** the on-screen **bottom-bar tap zone** (and **Esc** in the
  dev winit window). M3's gesture replaces this.
- **Focused app's client closes/dies** → auto-return to `Home`.

The Home↔App transition is a pure function `(UiState, Event) → UiState`, unit-tested
without Wayland. Page-snap animation reuses the M1 spring engine. **No app-open zoom in
M2** — M2 cuts between home and app; the zoom is M3 polish.

## App launch & compositing

- **Catalog → home:** on startup scan `.desktop` entries (`sc-config`), build the
  `ShellModel` (persisted grid order if present, else place all apps), resolve icons.
- **Launch:** strip field codes (`%U %f %F %u %i %c %k` …) from the `Exec` line; spawn the
  process with `WAYLAND_DISPLAY` pointing at our socket.
- **Match & focus:** match the new toplevel to the launched app by `app_id` when it equals
  the `.desktop` id; otherwise treat the newest toplevel as the launched app. If an app
  already has a live toplevel, tapping its icon **raises** it instead of relaunching.
- **Composite:** force the focused toplevel to full output size; import its buffer
  (shm + linux-dmabuf) to a GLES texture via Smithay's helpers; draw fullscreen. One
  foreground app at a time; backgrounded apps stay running (no frame callbacks) so M3's
  switcher has live windows.
- **Teardown:** toplevel destroyed → drop it; if it was foreground → `Home`.

## Input routing

Full seat: keyboard + pointer/touch. Routing depends on UI state:

- **Home:** events drive the shell (icon tap → launch, horizontal drag → page swipe).
- **App:** events forward to the focused toplevel via the seat (keyboard focus on its
  surface; pointer/touch mapped output→surface-local, which is 1:1 since the app is
  fullscreen) — **except** the reserved return-home affordances (bottom-bar tap zone and
  Esc), which the shell intercepts before forwarding.

## Rendering integration

Reuse the M1 Skia-on-Smithay-GLES path, with the M1 spike's per-frame surface rebuild
**fixed**: cache the Skia `Surface` + `BackendRenderTarget` keyed on `(fboid, width,
height)`, recreating only on change. Home UI is Skia; the foreground app is a Smithay-
composited GLES texture. (Home and app are mutually exclusive in M2, so they don't
overlap except the always-on-top bar zone drawn by Skia.)

## Error handling

- Spawn failure → log, stay `Home`.
- Icon decode/resolve failure → placeholder icon.
- Client buffer import failure → log, skip that frame's app draw (never crash).
- Client disconnect → remove toplevel; if foreground → `Home`.
- Missing display (dev) → same clean failure as M1.

## Testing strategy

- **`sc-layout`** (pure): icon rects per page, dock placement, page count from model size,
  hit-testing (point → icon/dock/bar, misses, page boundaries).
- **`sc-icons`** (pure-ish, fixture dirs): finds PNG, finds + rasterizes SVG, picks
  largest, placeholder fallback when absent.
- **`sc-compositor`** (pure): `UiState` transition machine — tap→App, bar/Esc→Home,
  client-close→Home, page-swipe snap targeting.
- **Integration:** nested winit harness (manual, mouse-as-touch) + on-device, visual —
  same approach as M1. Launch a real app (e.g. a terminal or weston-terminal), confirm it
  renders fullscreen and takes input, confirm return-home.

## Scope

**In (M2):** real home screen (icons/labels/pages/dock) from installed apps · page-swipe
with spring snap · tap-to-launch real Wayland apps · fullscreen composite (shm + dmabuf) ·
touch + keyboard input forwarding to the foreground app · placeholder return-home (bar tap
/ Esc) · raise-if-running · auto-return on app close · cached Skia `Surface`.

**Out (later):** bottom-bar grab/switcher/quick-switch gestures + app-open zoom (M3) ·
edit mode / folders / page-reorder (M4) · power / idle / DRM device backend (M5) ·
multi-output · notifications · IME/text-input protocol · clipboard.

## Key risks

- **First real Wayland server.** xdg-shell + seat + shm/dmabuf via Smithay is well-trodden
  (anvil is the reference) but it's the largest new surface area. Mitigation: build and
  verify the server incrementally — first accept a client and composite one fullscreen
  toplevel (test with `weston-terminal`/`foot`) before wiring the home-screen tap-to-launch
  on top.
- **dmabuf import on the dev GPU vs FP5 Adreno.** Verify in the nested harness first;
  revisit formats on device (M5) as already flagged in `skia_gl.rs`.
- **Icon coverage.** Pragmatic lookup will miss some apps → placeholder. Acceptable for
  M2; full theme-spec compliance deferred.
