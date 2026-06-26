# springchick — Design Spec

**Date:** 2026-06-26
**Status:** Approved for planning

## Summary

springchick is a Wayland compositor for Linux phones that clones the iOS SpringBoard
experience: a fast, GPU-accelerated icon home screen with editing, and bottom-bar
direct-manipulation navigation between running apps. The top priority is **maximum
performance and buttery-smooth, interruptible animations** at the device refresh rate.

**First target device:** Fairphone 5 (Qualcomm QCM6490, Adreno 643 GPU, ~90Hz 1224×2700
OLED). **Dev OS:** NixOS on FP5 (also expected to work on postmarketOS). Reproducible
toolchain pinned via a Nix flake.

## Architecture

### Core decision: fused compositor

springchick is a **single Rust binary** that is *both* the Wayland compositor and the
Springboard shell. The shell (home grid, dock, bottom bar, app switcher) is drawn
**in-process inside the compositor's own render loop** — there is no separate shell
client and no IPC on the animation hot path. Running apps are ordinary XDG-shell
toplevels composited as GPU textures.

This mirrors how iOS fuses SpringBoard with the window server, and it is the only
architecture whose performance ceiling matches the "smoothness above all" priority.
Explicitly rejected alternatives:

- **Separate WM/shell client (phosh-style layer-shell, or river + external WM):**
  every home-screen frame would roundtrip through the compositor — the dominant source
  of phone-shell jank. Rejected.
- **Go (or any GC language) on the render path:** GC pauses blow the ~11ms frame budget
  exactly during heavy animation. Rejected.

### Tech stack

- **Language:** Rust (no GC, predictable latency).
- **Compositor framework:** Smithay — DRM/KMS output, libinput, XDG-shell, EGL/GLES2
  rendering, frame/vblank clock.
- **2D shell renderer:** Skia via `skia-safe`, using the **Ganesh GL/GLES backend on the
  same EGL/GLES context as Smithay's renderer**. Skia draws the shell into a GL texture
  that Smithay composites — single graphics API, **no cross-API dmabuf/fence sync**.
  Skia provides production-grade gaussian blur (dock/folders), shadows, rounded-rect
  clipping, and world-class text out of the box.
- **Animation:** custom spring-physics engine ticked off the frame clock.

### Backend abstraction (dev vs device)

The same binary selects its backend at runtime via `SPRINGCHICK_BACKEND=winit|drm`:

- **winit (desktop dev):** runs as a native window on the NixOS desktop using the real
  desktop GPU and the real Skia/GLES path. Window forced to FP5 logical geometry
  (1224×2700 + scale) so layout and animation match device pixel-for-pixel.
  **Mouse → single touch point**; keyboard chords → simulated power/volume buttons.
- **drm (device):** real KMS output + libinput touch on the FP5.

This is the verification loop: **every feature is verified in the nested winit backend
first; on-device runs are for final confirmation only.** Building this harness is the
foundational task — everything else renders into it.

### Modules

One binary, internally split into focused modules. The pure-logic modules
(`anim`, `input`, `config`, and the grid model) have **zero rendering dependencies** so
they are unit-testable headless.

- **`compositor`** — Smithay wiring: backend abstraction (winit/drm), DRM/KMS output,
  libinput, XDG-shell, frame clock, damage/present. Composites app textures.
- **`shell`** — the Springboard, drawn in-loop via Skia: home grid, dock, bottom bar,
  app switcher, app-open/close transitions. Owns scene state.
- **`anim`** — spring-physics engine (position/scale/opacity/corner-radius/blur springs),
  interruptible, ticked with real `dt`. Pure logic.
- **`input`** — gesture recognizer + navigation state machine (see below). Pure logic;
  routes between shell and focused app.
- **`power`** — DPMS screen on/off, idle timeout, hardware buttons (power, volume) via
  libinput key events.
- **`config`** — `.desktop` app catalog, XDG icon-theme resolution, grid-state
  persistence under `$XDG_CONFIG_HOME/springchick/`. Pure logic (filesystem-backed).

## Frame loop

Single render loop driven by the DRM present/vblank clock (~90Hz on FP5). Each frame:

1. `input` drains libinput events → updates gesture state.
2. `anim` advances all active springs by real `dt`.
3. `shell` updates scene state from gesture + anim output.
4. Render pass: composite focused app texture(s) via Smithay GLES, then Skia draws shell
   layers (bar, grid, dock, blur) into the same GL context.
5. Submit with damage → page-flip.

The shell is always drawn by springchick in-loop, so home/switcher stay at refresh rate
even if an app client is stalled. Backgrounded apps stop receiving frame callbacks (idle
to save power) but keep their last texture for switcher cards.

## Bottom-bar navigation (the defining UX)

When a finger lands on the bottom bar, the active window **detaches into an interactive
transform layer holding its live texture** — pixel-identical to the running app, not a
snapshot. It tracks the finger 1:1 (scale + position + corner-radius) with rubber-banding
past bounds. This is direct manipulation, not a canned animation.

### Gesture state machine

- **GRAB** — touch-down in bottom bar detaches the active window into the interactive
  layer under the finger.
- **SHRINK** (mostly-vertical drag) — window scales toward a card, tracking finger Y.
  Dragging **far enough fans in neighbor cards = live switcher preview** (distance-gated,
  no dwell required).
- **QUICK-SWITCH** (horizontal) — current window slides off, adjacent app slides in,
  tracking finger X. Works both as an L/R move during a grab **and** as a direct
  horizontal flick on the bar without lifting first.
- **SETTLE** (on release) — finger velocity is handed to a spring whose **projected
  landing point** (position + velocity × decel) selects the target. Fully
  **interruptible**: re-grabbing mid-settle cancels the spring and re-attaches to finger.

### Release targets

- Tiny rise / low velocity → **back to app**.
- Fast upward **flick** → **home** (card flies into its icon) regardless of distance.
- Slow controlled drag ending far up (deck revealed) → **switcher deck**.
- Fast horizontal flick → **adjacent app** (quick-switch).

### Switcher

Card deck of live app textures. Tap a card → open it. **Tap outside the cards → home.**

## Home screen (layout A: paginated grid + dock)

- Horizontal **pages** of icons; horizontal swipe on the home screen changes page
  (distinct from in-app L/R quick-switch — semantics are context-dependent).
- Persistent **dock** row of favorites that stays across pages, sitting just above the
  nav bar grab zone.
- Page-indicator dots.

### App lifecycle (phone model, one foreground app)

- Tap icon → spawn/raise XDG toplevel, force fullscreen → app-open animation (icon zooms
  into window).
- Home gesture → window shrinks back toward its icon, shell returns.
- Switcher → card deck of live textures.

### Edit mode

- **MVP:** long-press → jiggle; drag-to-rearrange; delete icon.
- **Phase 2:** folders (drag icon onto icon → nested grid + blurred background);
  page-reorder.

## App catalog, power & buttons

- **App discovery:** freedesktop `.desktop` entries (name + `Exec` + `Icon`); icons
  resolved from the XDG icon theme; rendered by Skia. Launch = exec the `Exec` line as an
  XDG client.
- **Persistence:** grid state (page/slot order, deletions, dock contents) as JSON/TOML
  under `$XDG_CONFIG_HOME/springchick/`.
- **Power:** power-button short-press → DPMS display off/on; idle timeout → dim → blank.
  Volume keys forwarded as key events.

## Testing strategy

- **Unit (headless):** feed synthetic touch sequences into the gesture state machine,
  assert target classification (back/home/switcher/app); assert spring convergence and
  shape; assert grid rearrange/delete logic. Enabled by the pure-logic module split.
- **Nested winit backend:** full compositor as a desktop window for rapid manual
  iteration (mouse-as-touch).
- **On-device DRM backend:** same binary on the FP5 for final verification.

## MVP scope

**In v1:** fused compositor (Skia-on-GLES spike first) · nested winit dev harness ·
paginated home grid + dock (layout A) · launch app (XDG fullscreen) · bottom-bar nav
(grab live window, shrink, home, switcher deck, L/R quick-switch) · spring engine ·
app-open/close animation · edit mode (jiggle + drag-rearrange + delete) · power-button
screen on/off + idle blank · `.desktop` catalog + icon theme · grid persistence.

**Deferred (phase 2+):** folders · page-reorder · auth lock screen · volume OSD ·
suspend/resume · notifications · settings UI.

## Key risks

- **Skia ⊕ Smithay GLES context sharing + aarch64 cross-build.** De-risked by making a
  Skia-on-Smithay-GLES spike the first feature task: prove one blurred rounded rect
  renders, in the nested backend, before building anything else. `skia-safe` ships
  prebuilt aarch64 binaries; the Nix flake pins the toolchain.
- **Frame-budget discipline:** the pure-logic / render split and damage tracking are the
  structural defenses against jank.
