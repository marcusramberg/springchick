# springchick Milestone 4 — Device Backend + Perf Validation

**Date:** 2026-06-27
**Status:** Draft
**Builds on:** M3 (gestures + transitions: grab/shrink/settle, app-open/close zoom,
quick-switch, interruptible springs — all running in the nested winit dev harness).

## Why this milestone now (roadmap reorder)

M4 and M5 are **swapped** from the original master-design ordering. The device backend
(was M5) is promoted ahead of edit-mode/folders/page-reorder (now M5). Rationale: the
gesture/animation system is the defining risk of springchick, and we have only ever run it
in a nested winit window on a desktop-class GPU. Before investing in more shell features we
want to know whether the spring-driven, per-frame transformed compositing holds **90 Hz on
real Fairphone 5 hardware** (Adreno GPU, mobile thermal/power envelope). De-risk the
performance unknown now; pile on features later.

This milestone is a **measurement spike**, not a feature build. Its deliverable is: the
existing compositor running on the FP5 over real DRM/KMS, drivable by touch, emitting frame
-timing numbers.

## Goal

Run the existing springchick compositor on a real Fairphone 5 over DRM/KMS + libinput,
drive the full M3 interaction loop with touch, and capture frame-timing statistics so we can
judge whether the animation system performs acceptably on-device.

## Scope

**In (M4):**
- DRM/KMS + GBM output backend, selected by `SPRINGCHICK_BACKEND=drm`.
- libinput input (single-touch + keyboard) via a libseat/logind session.
- `run_drm()` on a calloop event loop, frame-paced by DRM page-flip (vblank).
- Shared render path extracted from the winit `render_frame` so both backends draw with
  identical GLES code (approach A — see below).
- Frame-timing instrumentation (`FrameStats`): per-second `fps / p50 / p99 / dropped` log
  line, gated by `SPRINGCHICK_PERF`.
- Nix package output (`packages.aarch64-linux.springchick`) so `nix run .#springchick`
  works on-device with runtime deps declared.
- On-device run runbook (VT-launch alongside Phosh).

**Out (deferred):**
- Multi-touch (pinch / multi-finger). Single-touch only — the nav gesture is one finger.
- greetd / selectable-session integration. VT-launch is sufficient for a spike. (M5+)
- Power-button DPMS + idle dim/blank. Not needed to measure gesture perf. (M5+)
- Unifying the winit dev path onto calloop. The winit loop stays as-is; the DRM path is
  additive. (later cleanup)
- Any frame-rate **target/tuning gate**. This milestone measures; tuning is follow-up.
- Edit mode / folders / page-reorder — that is now **M5**.

## Target environment

- **Device:** Fairphone 5 (Adreno GPU), reachable over SSH as `dmsmobile`. Device has its
  own build host, so builds happen on-device (`git clone` + `nix run`), not cross-compiled.
- **OS:** Mobile NixOS, currently running Phosh as the default Wayland session on VT1.
- **Host arch:** development host is already aarch64 — same arch as the device, so no
  cross-compilation is required at any point.
- **Session/seat:** logind (Mobile NixOS). springchick obtains DRM-master + input via a
  libseat session bound to the **active** VT.

## Architecture

### Approach (chosen): A — extract shared draw, duplicate only bind/submit

Both the winit backend (`WinitGraphicsBackend<GlesRenderer>`) and the DRM backend use
Smithay's `GlesRenderer`. The inner draw — clear pass, home (Skia), transformed app
two-pass composite, bar overlay — is therefore **identical** across backends. Only
`bind` (acquire renderer + framebuffer) and `submit`/page-flip differ, plus what drives
the frame.

Rejected alternatives: a `Backend` trait (premature interface for two impls; churns working
M3 code) and Smithay's desktop `Space`/`OutputManager` (overkill; would replace the
hand-rolled transform compositing M3 depends on).

### Module layout

```
crates/sc-compositor/src/
  main.rs          entry: BackendKind::from_env() → run_winit() | run_drm()
  backend.rs       (exists) BackendKind enum; unchanged
  render.rs   NEW  build_elements(state, &scene) -> Vec<…RenderElement>
                   draw_scene(&mut GlesRenderer, &mut Framebuffer, &State, &Scene, size)
                     — extracted verbatim from the current render_frame two-pass logic,
                       including the Skia home/bar interleaving and FrameStats hooks.
  drm_backend.rs NEW  run_drm(): calloop loop, libseat session, udev/DRM/GBM, libinput
                   source, wayland source, vblank-driven render, VT-switch handling.
  main.rs run_winit  now calls render::draw_scene instead of an inline body.
```

`State`, `ui_state`, `scene`, `input_dispatch`, `app_history` are shared and untouched.

### `run_drm()` internals

- **Session:** `LibSeatSession::new()` → DRM-master + input permissions from logind on the
  active VT. Supplies device open/close and VT-switch (activate/deactivate) signals.
- **udev / DRM / GBM:** `UdevBackend` enumerates the GPU; the DRM node is opened through the
  session. A `GbmAllocator` + `GbmBufferedSurface` is created on the connector's preferred
  mode (expected 1224×2700 @ 90 Hz). The `GlesRenderer` is built from the GBM/EGL display.
- **Input:** `LibinputInputBackend` (libinput context seated from the session) registered as
  a calloop source. Events are normalized (touch → `Pt` in 0..1) and fed through the **same**
  dispatch logic as winit: touch-down → press, touch-motion → move, touch-up → release;
  keyboard forwarded as today.
- **calloop sources:**
  1. DRM device — on page-flip/vblank completion: tick springs, `render::draw_scene`, queue
     the next flip.
  2. libinput — input dispatch.
  3. `WaylandSource` — client dispatch + flush.
  4. Session notifier — VT-switch: on deactivate pause DRM-master + renderer; on activate
     resume and force a redraw.
- **Frame pacing:** rendering is driven by page-flip completion (natural 90 Hz vsync), not a
  `thread::sleep`. This is the real performance signal we are after.

### Coordinate / orientation note

The winit path currently renders with `Transform::Flipped180`. The DRM path uses the
connector's real transform (likely `Normal`). Panel orientation must be verified on first
boot and corrected in a single place. GBM buffer format/modifiers on Adreno are an unknown;
fall back to a linear modifier if the preferred modifier set fails.

## Perf instrumentation

Lives in the **shared** render path so winit and DRM numbers are directly comparable.

- **`FrameStats`** (in `render.rs`): a ring buffer of the last N frame durations. Frame time
  is wall-clock between successive page-flip completions (DRM) / submit calls (winit).
- Per frame, record total frame time plus a coarse split: `tick` (spring step),
  `build+draw` (GLES element build + draw), `submit`/flip-wait.
- **Every ~1 s**, emit one log line:
  `fps=NN p50=X.Xms p99=Y.Yms dropped=K`
  where `dropped` counts frames exceeding the 11.1 ms (90 Hz) budget.
- Gated by `SPRINGCHICK_PERF` (default on for DRM; off for winit unless set).
- No on-screen HUD (YAGNI). The stdout/journal log line is the artifact.

## Deploy / run

Builds happen on-device. Flake gains a package output so `nix run .#springchick` resolves
runtime deps (libseat, libinput, libgbm, mesa/EGL, libxkbcommon) rather than assuming them
present.

### Runbook

1. `ssh dmsmobile`; `git clone` / `git pull` the repo.
2. From the SSH shell, switch to a free getty: `chvt 3` (or `openvt`), and log in on that VT
   — this makes that login the seat owner / active session.
3. On that VT: `SPRINGCHICK_BACKEND=drm SPRINGCHICK_PERF=1 nix run .#springchick`.
4. Phosh stays alive on VT1. `chvt 1` to return to it. Ctrl-C or VT-switch stops springchick.
5. Capture perf with `… 2>&1 | tee perf.log`, or read it from the journal — the per-second
   fps line is on stdout.

No NixOS module / greetd session entry this milestone (deferred). VT launch only.

## Done criteria

Milestone is complete when, on the FP5 over DRM:

- springchick starts, takes DRM-master from a VT, and renders the home screen.
- Touch input drives the full M3 loop: **home → tap-open (zoom) → grab-shrink → release →
  home**, plus at least one **quick-switch**.
- The `FrameStats` line prints sustained numbers across that interaction.

No frame-rate target gates completion — this milestone produces the numbers; deciding what
to do about them (tuning, render changes) is follow-up work.

## Testing strategy

- **Unit (headless):** `FrameStats` percentile/dropped-frame math tested with synthetic
  duration sequences. `BackendKind::from_env` already covered.
- **Winit regression:** after the `render.rs` extraction, the winit dev harness must behave
  identically to M3 (same gestures, same visuals). This is the safety net for the refactor.
- **On-device (manual):** the runbook loop above. Primary acceptance is manual: drive the
  gesture loop, read the perf line.

## Key risks

- **libseat ↔ logind on Mobile NixOS.** Seat acquisition may require the session to be on the
  **active** VT (hence the `chvt` before launch). If libseat refuses, fall back to launching
  from a directly-logged-in VT rather than over SSH.
- **Panel orientation / transform.** Unknown until first boot; correct in one place.
- **GBM format/modifier support on Adreno.** Preferred modifiers may be unsupported; fall
  back to linear. Buffer allocation failure is the most likely first-boot blocker.
- **Render-path extraction regression.** Pulling `render_frame` apart risks breaking the
  working M3 winit path. Mitigation: extract verbatim, lean on the winit regression check.
- **Thermals.** Sustained 90 Hz transformed compositing may throttle. That is exactly the
  signal this milestone exists to surface — not a risk to mitigate, a result to record.
