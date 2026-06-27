# springchick — Development Guide

## Project overview

springchick is an iOS Springboard-style Wayland compositor for mobile Linux
(Fairphone 5). Fused compositor: shell UI (home grid, dock, gestures, animations)
drawn by Skia into the same GLES context Smithay uses for client compositing.

## Architecture

```
crates/
  sc-anim/         Pure spring physics engine
  sc-input/        Pure gesture recognizer + nav state machine
  sc-shell-model/  Pure grid/dock data model (pages, slots, dock)
  sc-config/       .desktop parsing + state persistence (filesystem IO)
  sc-layout/       Pure geometry: (size, page, model) → icon rects + hit-testing
  sc-icons/        Icon resolution (filesystem IO + resvg). Returns raw RGBA pixels.
  sc-compositor/   The binary. Smithay + Skia + calloop + all wiring.
    src/
      main.rs           Entry point, State struct, Wayland handlers, render loop
      ui_state.rs       Pure state machine (UiState enum + transitions)
      scene.rs          Pure scene computation (UiState → WindowTransform)
      input_dispatch.rs Input routing (pointer events → UiEvents)
      app_history.rs    MRU stack for quick-switch
      launcher.rs       Process spawning (strip field codes, set env)
      skia_gl.rs        Skia-on-Smithay-GLES rendering
      backend.rs        Backend constants (FP5 dimensions)
```

**Key principle:** push logic into pure crates (no GPU, no Wayland deps), test
heavily there. The compositor crate does integration/wiring only.

## Building

```bash
nix develop          # Enter dev shell with all native deps
cargo build          # Debug build
cargo check          # Fast type-check (no linking)
cargo check --tests  # Verify test code compiles
```

The binary is `target/debug/springchick`. It opens a winit window (nested
compositor) on the host Wayland session.

## Testing

### Unit tests (pure crates)

```bash
cargo test -p sc-layout -p sc-icons -p sc-shell-model -p sc-anim -p sc-input
```

These run without a display, GPU, or Wayland. They cover:
- Layout geometry + hit-testing (sc-layout)
- Icon resolution + placeholder fallback (sc-icons)
- Grid/dock model operations (sc-shell-model)
- Spring convergence + interruptibility (sc-anim)
- Gesture classification + nav targets (sc-input)

### Compositor unit tests

The `ui_state.rs`, `scene.rs`, `app_history.rs` modules have `#[cfg(test)]`
tests. They can't link in this Nix environment (missing native libs for the
binary target), but `cargo check --tests` verifies they compile.

To actually run them, use `nix develop`:
```bash
nix develop --command cargo test -p sc-compositor
```

### Integration testing (nested compositor)

The compositor runs nested via the winit backend:
```bash
nix develop --command ./target/debug/springchick
```

To launch a client into it:
```bash
WAYLAND_DISPLAY=springchick-0 foot
```

Automated smoke test pattern (from agent tooling):
```bash
nix develop --command bash -c '
  ./target/debug/springchick &
  SC_PID=$!
  sleep 2
  WAYLAND_DISPLAY=springchick-0 foot &
  FOOT_PID=$!
  sleep 2
  # Verify both alive
  kill -0 $SC_PID && kill -0 $FOOT_PID && echo "OK"
  kill $FOOT_PID $SC_PID
  wait
'
```

### What can't be tested automatically

- Visual correctness (icon rendering, animation smoothness, transform quality)
- Gesture feel (spring tuning, dead zones, interruptibility)
- These require human eyes on the nested winit window

### Test philosophy

- **Pure logic → unit test.** Every state transition, layout computation, and
  gesture classification has a unit test.
- **Integration → smoke test.** Compositor starts, accepts clients, doesn't crash.
- **Visual → human.** Screenshots shared inline for review. No pixel-diff infra yet.
- **Write the test before or alongside the code.** If a module is pure, there's
  no excuse for missing tests.

## Key patterns

### UiState machine (ui_state.rs)

All Home↔App transitions go through `transition(&mut state, event) -> Effect`.
Pure function, no side effects except the state mutation. Effects (Launch, etc.)
are returned for the caller to execute.

States: `Home`, `App`, `AppOpening`, `AppClosing`, `Grabbing`, `Settling`.
Events: `TapIcon`, `AppMapped`, `ReturnHome`, `GrabStart`, `GrabMove`,
`GrabRelease`, `Interrupt`, `Tick`, `ToplevelClosed`, `PageDrag`, `PageRelease`.

### Scene computation (scene.rs)

`compute_scene(state, output_size) -> Scene` maps the current UiState to a
`WindowTransform` (scale, center, corner_radius) the renderer applies.
Pure, testable, no GPU deps.

### Render pipeline (main.rs render_frame)

1. Tick animations (`UiEvent::Tick`)
2. Compute scene
3. Smithay pass 1: clear background + fullscreen app (if not transitioning)
4. Skia: draw home screen (if `scene.show_home`)
5. Smithay pass 2: draw scaled app elements (if transitioning, using
   `RescaleRenderElement` + `RelocateRenderElement`)
6. Skia: draw bar overlay (always)
7. Send frame callbacks to client
8. Submit

### Skia-on-Smithay GLES sharing

Skia and Smithay share one EGL/GLES context. Key rules:
- Call `context.reset(None)` before any Skia draw (invalidates Skia's GL state cache)
- Call `context.flush_and_submit()` after Skia draws (ensures pixels land before swap)
- Cache the Skia `Surface` keyed on `(fboid, width, height)` — recreate only on change

### Input dispatch (input_dispatch.rs)

All pointer/touch events → normalized `Pt` (0..1) → routed based on UiState:
- Home: hit-test icons or start page drag
- App: bar zone → grab start, else forward to client
- Grabbing: update tracker
- Settling/Opening/Closing: interrupt

## Smithay specifics

- Version: 0.7.0 (pinned, no master tracking)
- Wayland protocols implemented: compositor, xdg-shell, shm, seat, wl_output,
  xdg-decoration, data-device
- Missing (expected warnings from clients): primary-selection, xdg-activation,
  fractional-scale, cursor-shape, text-input, server-side cursors
- Toplevels configured fullscreen + activated + server-side decoration
- `ListeningSocket::bind_auto` for the Wayland socket

## NixOS specifics

- App .desktop files: `/run/current-system/sw/share/applications/`,
  `/etc/profiles/per-user/$USER/share/applications/`, `~/.local/share/applications/`
- Icons: `/run/current-system/sw/share/icons/hicolor/` (not `/usr/share/icons/`)
- Must run from `nix develop` for native library deps (wayland, fontconfig, etc.)
- Binary can't link in bare `cargo build` outside nix shell (expected)

## Common issues

- **SVG parse warnings (marker-start/mid/end):** Harmless resvg noise from system
  icons with unsupported SVG features. No fix needed.
- **"compositor does not provide required interfaces" (GTK apps):** Missing optional
  protocols. Most GTK4/libadwaita apps work; some need additional protocols.
- **App renders wider than window:** The winit host constrains our window size.
  We use `backend.window_size()` as actual output dimensions; apps get configured
  to that real size.
- **CSD header bar still visible:** Set `xdg_toplevel::State::Fullscreen` on configure.
  Libadwaita apps hide their header bar in fullscreen mode.

## Milestones

- **M1** (done): Foundation. Pure crates + Skia-on-Smithay spike.
- **M2** (done): Home screen + app launch + fullscreen compositing.
- **M3** (in progress): Bottom-bar gestures + app transitions (grab/shrink/settle).
- **M4** (planned): Edit mode / folders / page-reorder.
- **M5** (planned): Device backend (DRM/libinput on FP5).
