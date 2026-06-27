# springchick M4 — Device Backend + Perf Validation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the existing springchick compositor on a real Fairphone 5 over DRM/KMS + libinput, drivable by touch, emitting per-second frame-timing statistics.

**Architecture:** Approach A — extract the shared GLES draw path into `render.rs` so the existing winit backend and a new DRM backend draw with identical code, differing only in `bind`/`submit` and what drives the frame. Add `run_drm()` on its own calloop event loop (libseat/logind session + udev/DRM/GBM + libinput + wayland sources), frame-paced by page-flip. Winit dev path stays working as the refactor regression net. `FrameStats` instrumentation lives in the shared path so winit and DRM numbers are comparable.

**Tech Stack:** Rust, Smithay 0.7 (`backend_drm`, `backend_gbm`, `backend_udev`, `backend_libinput`, `backend_session_libseat`, `renderer_gl`), calloop 0.14, GlesRenderer, Skia (home/bar overlay), Nix flake.

**Spec:** `docs/superpowers/specs/2026-06-27-springchick-m4-device-backend-perf.md`

**Reference reading before starting DRM tasks:** Smithay's `anvil` example (udev backend) and `smallvil` example are the canonical references for the exact 0.7 API surface used in Tasks 5–7. Smithay's API churns between versions; when a type/method name below differs from what `cargo build` reports, trust the compiler + anvil over this document, and keep the *structure* described here.

**Out of scope (do not implement):** multi-touch, greetd/selectable session, power/DPMS/idle, unifying the winit path onto calloop.

---

## File Structure

| File | Responsibility |
|------|----------------|
| `crates/sc-compositor/src/frame_stats.rs` (new) | Pure frame-timing ring buffer: record durations, compute p50/p99/fps/dropped, format the log line. Unit-tested. |
| `crates/sc-compositor/src/render.rs` (new) | Shared render path: `build_elements(state, scene)` and `draw_scene(renderer, framebuffer, state, scene, size, stats)`. Extracted verbatim from current `render_frame`. Owns the two-pass composite + Skia home/bar interleave. |
| `crates/sc-compositor/src/main.rs` (modify) | `main()` dispatches `BackendKind::from_env()` → `run_winit()` | `run_drm()`. `run_winit` + `render_frame` call into `render::draw_scene`. |
| `crates/sc-compositor/src/drm_backend.rs` (new) | `run_drm()`: session, udev/DRM/GBM, GlesRenderer, calloop sources (drm/libinput/wayland/session), vblank-driven render, VT-switch. |
| `crates/sc-compositor/src/input_common.rs` (new, small) | Backend-agnostic input handling shared by winit + libinput: given a normalized event kind + `Pt`, drive `State` via the existing dispatch. Extracted from `handle_winit_input` so libinput reuses it. |
| `crates/sc-compositor/Cargo.toml` (modify) | Add smithay device-backend features. |
| `flake.nix` (modify) | Add `packages.<system>.springchick` derivation + `nix run` wiring with runtime `LD_LIBRARY_PATH`. |
| `docs/RUNBOOK-device.md` (new) | On-device VT-launch runbook + perf-capture steps. |

Each task below is independently committable. Tasks 1–4 are pure/refactor and fully testable on the dev host. Tasks 5–7 are device backend code that cannot be unit-tested (requires real DRM/seat); they are verified by `cargo build` on the host and manual on-device runs per the runbook. Task 8 (nix) is verified by `nix build`. Task 9 is docs.

---

## Task 1: FrameStats (pure, TDD)

**Files:**
- Create: `crates/sc-compositor/src/frame_stats.rs`
- Modify: `crates/sc-compositor/src/main.rs` (add `mod frame_stats;`)

- [ ] **Step 1: Write the failing tests**

```rust
// in frame_stats.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ms(n: f64) -> Duration { Duration::from_secs_f64(n / 1000.0) }

    #[test]
    fn percentiles_from_known_set() {
        let mut s = FrameStats::new(Duration::from_micros(11_111)); // 90Hz budget
        for v in [10.0, 10.0, 10.0, 10.0, 30.0] {
            s.record_frame(ms(v));
        }
        let snap = s.snapshot();
        assert!((snap.p50_ms - 10.0).abs() < 0.5);
        // p99 of this tiny set is the max sample.
        assert!((snap.p99_ms - 30.0).abs() < 0.5);
    }

    #[test]
    fn dropped_counts_over_budget_frames() {
        let mut s = FrameStats::new(Duration::from_micros(11_111));
        for v in [5.0, 5.0, 20.0, 20.0] {
            s.record_frame(ms(v));
        }
        assert_eq!(s.snapshot().dropped, 2);
    }

    #[test]
    fn fps_is_inverse_of_mean() {
        let mut s = FrameStats::new(Duration::from_micros(11_111));
        for _ in 0..10 { s.record_frame(ms(10.0)); } // 10ms => 100fps
        let snap = s.snapshot();
        assert!((snap.fps - 100.0).abs() < 1.0);
    }

    #[test]
    fn ring_buffer_evicts_old_samples() {
        let mut s = FrameStats::with_capacity(Duration::from_micros(11_111), 3);
        for v in [100.0, 100.0, 100.0, 10.0, 10.0, 10.0] {
            s.record_frame(ms(v));
        }
        // Only the last 3 (all 10ms) should remain.
        assert!((s.snapshot().p50_ms - 10.0).abs() < 0.5);
    }
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `nix develop -c cargo test -p sc-compositor frame_stats -- --nocapture`
Expected: FAIL to compile (`FrameStats` not defined).

- [ ] **Step 3: Implement FrameStats**

```rust
//! Pure frame-timing statistics for perf validation (M4).
//!
//! Records per-frame wall-clock durations in a ring buffer and computes
//! fps / p50 / p99 / dropped-frame counts. Backend-agnostic so winit and DRM
//! numbers are directly comparable.

use std::collections::VecDeque;
use std::time::Duration;

/// A computed snapshot of recent frame timings.
#[derive(Clone, Copy, Debug)]
pub struct StatsSnapshot {
    pub fps: f64,
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub dropped: usize,
    pub samples: usize,
}

/// Ring buffer of recent frame durations.
pub struct FrameStats {
    budget: Duration,
    cap: usize,
    samples: VecDeque<Duration>,
}

impl FrameStats {
    /// Default capacity ~ a couple seconds at 90Hz.
    pub fn new(budget: Duration) -> Self {
        Self::with_capacity(budget, 256)
    }

    pub fn with_capacity(budget: Duration, cap: usize) -> Self {
        Self { budget, cap, samples: VecDeque::with_capacity(cap) }
    }

    /// Record one frame's wall-clock duration.
    pub fn record_frame(&mut self, dt: Duration) {
        if self.samples.len() == self.cap {
            self.samples.pop_front();
        }
        self.samples.push_back(dt);
    }

    /// Number of recorded samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Compute a snapshot. Returns zeros when empty.
    pub fn snapshot(&self) -> StatsSnapshot {
        if self.samples.is_empty() {
            return StatsSnapshot { fps: 0.0, p50_ms: 0.0, p99_ms: 0.0, dropped: 0, samples: 0 };
        }
        let mut sorted: Vec<f64> = self.samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| -> f64 {
            let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
            sorted[idx.min(sorted.len() - 1)]
        };
        let mean: f64 = sorted.iter().sum::<f64>() / sorted.len() as f64;
        let budget_ms = self.budget.as_secs_f64() * 1000.0;
        let dropped = sorted.iter().filter(|&&v| v > budget_ms).count();
        StatsSnapshot {
            fps: if mean > 0.0 { 1000.0 / mean } else { 0.0 },
            p50_ms: pct(50.0),
            p99_ms: pct(99.0),
            dropped,
            samples: sorted.len(),
        }
    }

    /// One-line summary for logging.
    pub fn format_line(&self) -> String {
        let s = self.snapshot();
        format!(
            "fps={:.0} p50={:.1}ms p99={:.1}ms dropped={} n={}",
            s.fps, s.p50_ms, s.p99_ms, s.dropped, s.samples
        )
    }
}
```

Add to `main.rs` module list: `mod frame_stats;`

- [ ] **Step 4: Run tests, verify pass**

Run: `nix develop -c cargo test -p sc-compositor frame_stats`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/frame_stats.rs crates/sc-compositor/src/main.rs
git commit -m "feat(m4): pure FrameStats frame-timing ring buffer

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 2: Extract shared render path to `render.rs`

This is a pure refactor: move the body of `render_frame` (the element build + two-pass composite + Skia home/bar interleave) into `render.rs`, parameterized over `&mut GlesRenderer` + framebuffer. The winit `render_frame` becomes a thin caller. **No behavior change** — the winit harness must look and behave exactly as on M3. That equivalence is the regression net for the whole milestone.

**Files:**
- Create: `crates/sc-compositor/src/render.rs`
- Modify: `crates/sc-compositor/src/main.rs` (`mod render;`, rewrite `render_frame` body to delegate, thread a `FrameStats` field into `State`)

- [ ] **Step 1: Add `stats: FrameStats` to `State`**

In `State` struct add field `stats: FrameStats,` and in `State::new` initialize:
```rust
stats: crate::frame_stats::FrameStats::new(std::time::Duration::from_micros(11_111)),
```

- [ ] **Step 2: Move element-build + draw into `render.rs`**

Create `render.rs` exposing two functions. Copy the existing logic out of `render_frame` (main.rs current lines ~794–905) verbatim — the clear pass, the `is_fullscreen` branch, the `RescaleRenderElement`/`RelocateRenderElement` two-pass, the Skia `draw_home`/`draw_bar_overlay` calls, and `send_frames_surface_tree`.

```rust
//! Shared render path for the winit and DRM backends.
//!
//! Both backends own a `GlesRenderer` and differ only in how they acquire the
//! framebuffer (`bind`) and present (`submit`/page-flip). Everything between —
//! clearing, the transformed two-pass app composite, and the Skia home/bar
//! overlay — is identical and lives here.

use crate::scene::Scene;
use crate::skia_gl::SkiaGl;
// ... (imports: GlesRenderer, Frame, render elements, WlSurface, Size, etc.)

/// Inputs the shared draw needs that aren't the renderer/framebuffer itself.
pub struct DrawCtx<'a> {
    pub scene: &'a Scene,
    pub app_surface: Option<&'a smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>,
    pub skia: &'a mut SkiaGl,
    pub model: &'a sc_shell_model::ShellModel,
    pub icon_cache: &'a std::collections::HashMap<String, sc_icons::IconPixels>,
    pub app_catalog: &'a std::collections::HashMap<String, sc_config::AppEntry>,
    pub transform: smithay::utils::Transform,
}

/// Execute the full two-pass scene draw against an already-bound framebuffer.
/// `size` is the physical output size. Returns nothing; presentation is the
/// caller's job.
pub fn draw_scene<F>(
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    framebuffer: &mut F,
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    ctx: &mut DrawCtx<'_>,
)
where
    F: smithay::backend::renderer::Framebuffer, // bound type differs per backend
{
    // ... clear pass, fullscreen-in-pass-1 branch, skia home, scaled pass-2, skia bar ...
}
```

Notes for the implementer:
- The winit path currently renders with `Transform::Flipped180`. Make `transform` a `DrawCtx` field so the DRM path can pass the connector's real transform. Winit keeps `Flipped180`.
- The exact framebuffer generic bound (`F`) may need to be the concrete bound type from each backend rather than a trait. If a clean trait bound is awkward in Smithay 0.7, it is acceptable to make `draw_scene` generic over the renderer's frame via a small closure, or to keep two thin wrapper fns that share a private `draw_inner`. Prefer whatever compiles with the least abstraction — the goal is *shared inner draw*, not a perfect signature.
- Keep `send_frames_surface_tree` and `build_elements` helpers in `render.rs` too.

- [ ] **Step 3: Rewrite `render_frame` to delegate + record stats**

`render_frame` keeps: tick springs, page_count restore, compute scene, resolve `app_surface`, `backend.bind()`, then call `render::draw_scene`, then `backend.submit(...)`. Wrap the frame in timing:
```rust
let frame_start = std::time::Instant::now();
// ... bind + draw_scene + submit ...
state.stats.record_frame(frame_start.elapsed());
```
Add a 1-second throttle to log `state.stats.format_line()` via `tracing::info!`, gated by an env flag read once at startup (see Task 4 for `perf_enabled`).

- [ ] **Step 4: Verify no regression**

Run: `nix develop -c cargo build -p sc-compositor` → Expected: builds clean.
Run: `nix develop -c cargo test` → Expected: all existing tests still pass.
Run: `nix develop -c cargo clippy --all-targets` → Expected: no new warnings.
Manual: `nix develop -c cargo run -p sc-compositor` (winit) → home screen renders, tap-open / grab / home behave exactly as M3.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/render.rs crates/sc-compositor/src/main.rs
git commit -m "refactor(m4): extract shared render path to render.rs

Winit render_frame now delegates to render::draw_scene; no behavior change.
Threads FrameStats into the frame loop.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 3: Extract backend-agnostic input handling

`handle_winit_input` currently owns both winit-specific event decoding *and* the state-driving logic (press/move/release → `on_press`/`on_move`/transitions). Pull the backend-agnostic half into `input_common.rs` so libinput (Task 6) reuses it. Pure-ish refactor.

**Files:**
- Create: `crates/sc-compositor/src/input_common.rs`
- Modify: `crates/sc-compositor/src/main.rs` (`mod input_common;`, call into it from `handle_winit_input`)

- [ ] **Step 1: Define the shared entry points**

```rust
//! Backend-agnostic input handling. winit and libinput both decode their
//! native events into these calls.

use crate::State;

/// Absolute pointer/touch position update (pixels).
pub fn on_motion(state: &mut State, x: f32, y: f32) { /* set last_pointer_pos; if pointer_down, page-drag + grab-move (moved from handle_winit_input PointerMotionAbsolute) */ }

/// Touch-down / button-press at the last known position.
pub fn on_press(state: &mut State) { /* moved from PointerButton Pressed arm */ }

/// Touch-up / button-release.
pub fn on_release(state: &mut State) { /* moved from PointerButton Released arm: bar-drag classify, page snap, grab release + icon_center override, page_count restore */ }

/// Keyboard handling stays backend-specific (winit uses the smithay keyboard
/// handle); the Esc→return-home shortcut can be a small helper here.
pub fn on_escape(state: &mut State) -> bool { /* returns true if handled */ }
```

- [ ] **Step 2: Move the bodies**

Cut the logic out of `handle_winit_input`'s `PointerButton`/`PointerMotionAbsolute` arms into these functions; `handle_winit_input` decodes winit events and calls them. Keyboard forwarding to the focused client stays in `handle_winit_input` (it needs the `KeyboardHandle`).

- [ ] **Step 3: Verify no regression**

Run: `nix develop -c cargo build -p sc-compositor && nix develop -c cargo test`
Manual: winit run — gestures identical to M3.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/input_common.rs crates/sc-compositor/src/main.rs
git commit -m "refactor(m4): extract backend-agnostic input handling

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 4: Backend dispatch + perf env flag

**Files:**
- Modify: `crates/sc-compositor/src/main.rs`
- Create: `crates/sc-compositor/src/drm_backend.rs` (stub for now)

- [ ] **Step 1: Add the stub module**

```rust
//! DRM/KMS device backend (M4). See docs/superpowers/specs/...-m4-...md.
pub fn run_drm() {
    tracing::error!("DRM backend not yet implemented");
}
```
Add `mod drm_backend;` to main.rs.

- [ ] **Step 2: Dispatch in `main()`**

```rust
fn main() {
    init_tracing();
    match backend::BackendKind::from_env() {
        backend::BackendKind::Winit => {
            info!("springchick M4 — winit dev backend");
            run_winit();
        }
        backend::BackendKind::Drm => {
            info!("springchick M4 — DRM device backend");
            drm_backend::run_drm();
        }
    }
}
```

- [ ] **Step 3: Perf flag**

Add a helper `fn perf_enabled(kind: backend::BackendKind) -> bool` returning true when `SPRINGCHICK_PERF` is set, or default-true for DRM. Store the resolved bool in `State` (`perf_log: bool`) and gate the Task 2 per-second log line on it.

- [ ] **Step 4: Verify**

Run: `nix develop -c cargo build -p sc-compositor` → builds.
Run: `SPRINGCHICK_BACKEND=drm nix develop -c cargo run -p sc-compositor` → logs "DRM backend not yet implemented" and exits.
Run: `nix develop -c cargo run -p sc-compositor` → winit as before.

- [ ] **Step 5: Commit**

```bash
git add crates/sc-compositor/src/main.rs crates/sc-compositor/src/drm_backend.rs
git commit -m "feat(m4): backend dispatch (winit|drm) + SPRINGCHICK_PERF flag

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 5: Smithay device-backend features + DRM/GBM bring-up

**Files:**
- Modify: `crates/sc-compositor/Cargo.toml`
- Modify: `crates/sc-compositor/src/drm_backend.rs`

> Tasks 5–7 are hardware backend code. They cannot be unit-tested; verification is `cargo build` on the host (compiles against real Smithay APIs) plus manual on-device runs (Task 9 runbook). Lean on the `anvil` udev example for exact API usage.

- [ ] **Step 1: Enable smithay features**

In `crates/sc-compositor/Cargo.toml`, extend the smithay feature list with:
```toml
  "backend_drm",
  "backend_gbm",
  "backend_udev",
  "backend_libinput",
  "backend_session_libseat",
  "renderer_multi",
```
Run: `nix develop -c cargo build -p sc-compositor` → Expected: builds (features resolve).

- [ ] **Step 2: Session + udev device discovery**

In `run_drm`, build a calloop `EventLoop`, then:
- `LibSeatSession::new()` → `(session, notifier)`.
- Insert `notifier` into the loop (session events).
- `UdevBackend::new(session.seat())` to enumerate DRM devices; pick the primary GPU node (the one with a connected connector). For the FP5 there is a single Adreno DRM node.
- Open the DRM node through the session: `session.open(path, flags)` → fd; wrap in `DrmDeviceFd` / `DrmDevice::new(...)`.

Reference: anvil `udev.rs` `UdevData` + `device_added`.

- [ ] **Step 3: GBM allocator + surface on the preferred mode**

- Create `GbmDevice` from the DRM fd; `GbmAllocator`.
- Enumerate connectors; pick the connected one; choose its **preferred** mode (expect 1224×2700 @ 90Hz). Log the chosen mode + connector transform.
- Build an `EglDisplay` from the GBM device, then a `GlesRenderer`.
- Create the scanout surface: a **`GbmBufferedSurface`** (`smithay::backend::drm::surface::gbm`) bound to the crtc/connector/mode.
  - **Correction (decided during impl):** do NOT use the higher-level `DrmCompositor`. `DrmCompositor::render_frame` owns the frame and takes a *list of render elements*, which is incompatible with springchick's shared `draw_scene` — that path does manual two-pass `GlesRenderer` rendering with **raw Skia GL interleaved** (home/bar) against a bound framebuffer. `GbmBufferedSurface` gives exactly the bind/submit primitives Approach A needs: `next_buffer()` → `Dmabuf`, `renderer.bind(&mut dmabuf)` → `GlesTarget` (same `GlesRenderer::Framebuffer` winit yields) → call the identical `render::draw_scene` → `queue_buffer(sync, damage, ())` to page-flip → `frame_submitted()` on vblank.
- Store `output_size` from the mode; reuse the existing `Output` setup so clients get correct geometry.

- [ ] **Step 4: First render — home screen on device**

Wire a render function that:
- builds a frame on the renderer,
- calls `render::draw_scene` with `transform` = the connector transform and a `DrawCtx` referencing `State`'s skia/model/icons,
- submits via `DrmCompositor::render_frame(...)` + `queue_frame(...)`.
Drive an initial render once after setup, then re-render on page-flip completion (Step 5).

- [ ] **Step 5: Page-flip-driven loop + wayland source**

- Register the `DrmDevice` (or compositor's) event source; on `DrmEvent::VBlank` / frame-submitted callback: mark the previous frame presented, tick springs (`UiEvent::Tick`), `record_frame`, render the next frame, queue the next flip.
- Insert a `WaylandSource` (wrapping the `Display`) into the loop for client dispatch + flush, exactly as the socket-accept/dispatch logic in `run_winit` but calloop-driven.
- Keep accepting clients on the `ListeningSocket` (insert it as a calloop generic source, or poll it in the loop as today).

- [ ] **Step 6: Verify build**

Run: `nix develop -c cargo build -p sc-compositor` → Expected: builds against real DRM APIs. (Runtime verification is on-device, Task 9.)

- [ ] **Step 7: Commit**

```bash
git add crates/sc-compositor/Cargo.toml crates/sc-compositor/src/drm_backend.rs
git commit -m "feat(m4): DRM/GBM bring-up — session, udev, scanout, page-flip render loop

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 6: libinput input source

**Files:**
- Modify: `crates/sc-compositor/src/drm_backend.rs`

- [ ] **Step 1: Create the libinput backend**

- `Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()))`, `assign_seat(seat_name)`.
- Wrap in `LibinputInputBackend::new(libinput)`; insert as a calloop source.

- [ ] **Step 2: Dispatch events through `input_common`**

On each `InputEvent`:
- `TouchDown`: convert absolute coords (libinput touch is normalized to the device; map to output pixels via the touch transform / output size) → `input_common::on_motion(state, x, y)` then `on_press(state)`.
- `TouchMotion`: → `on_motion`.
- `TouchUp`: → `on_release`.
- `Keyboard`: feed the seat keyboard; handle Esc via `input_common::on_escape`.
Reuse the same normalization the winit path uses so gestures behave identically.

- [ ] **Step 3: Verify build**

Run: `nix develop -c cargo build -p sc-compositor` → builds.

- [ ] **Step 4: Commit**

```bash
git add crates/sc-compositor/src/drm_backend.rs
git commit -m "feat(m4): libinput touch+keyboard source wired to shared input dispatch

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 7: VT-switch handling

**Files:**
- Modify: `crates/sc-compositor/src/drm_backend.rs`

- [ ] **Step 1: Handle session activate/deactivate**

On the session notifier event:
- **Deactivate** (VT switched away): pause the DRM device (`drm.pause()` / drop master), stop rendering.
- **Activate** (VT switched back): resume the DRM device, reset the renderer/scanout state, force a redraw, re-queue a flip.
Reference: anvil `udev.rs` session-event handling.

- [ ] **Step 2: Verify build**

Run: `nix develop -c cargo build -p sc-compositor` → builds.

- [ ] **Step 3: Commit**

```bash
git add crates/sc-compositor/src/drm_backend.rs
git commit -m "feat(m4): handle VT-switch pause/resume of DRM master

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 8: Nix package output for on-device `nix run` — DEFERRED

**Status (decided during impl): deferred.** `skia-safe`'s build script downloads a
prebuilt Skia archive at build time. `nix build` runs in a no-network sandbox, so a
`buildRustPackage` derivation fails to fetch it. The dev shell works only because `cargo`
runs outside the sandbox. Properly packaging means vendoring Skia as a fixed-output
derivation (or pointing `SKIA_SOURCE_DIR`/`SKIA_BINARIES_URL` at a nixpkgs Skia) — a
worthwhile cleanup but out of scope for a perf spike. The on-device workflow uses
`nix develop -c cargo run` (network available, identical build inputs), so `nix run`
packaging buys nothing for M4. Revisit when springchick needs a real installable session.

### Original (kept for reference)

**Files:**
- Modify: `flake.nix`

- [ ] **Step 1: Add a package derivation**

Inside the `eachDefaultSystem` body, add a `springchick` package alongside `devShells`:

```nix
packages.springchick = pkgs.rustPlatform.buildRustPackage {
  pname = "springchick";
  version = "0.1.0";
  src = ./.;
  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = [ pkgs.pkg-config pkgs.clang pkgs.makeWrapper ];
  buildInputs = [
    pkgs.wayland pkgs.libinput pkgs.libxkbcommon pkgs.libGL pkgs.mesa
    pkgs.udev pkgs.seatd pkgs.libgbm pkgs.fontconfig pkgs.freetype
  ];
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  # GLES/EGL/wayland/gbm are dlopen'd at runtime — wrap LD_LIBRARY_PATH.
  postInstall = ''
    wrapProgram $out/bin/springchick \
      --prefix LD_LIBRARY_PATH : "${pkgs.lib.makeLibraryPath [
        pkgs.wayland pkgs.libxkbcommon pkgs.libGL pkgs.mesa pkgs.libgbm pkgs.libinput pkgs.seatd
      ]}"
  '';
};
packages.default = packages.springchick;
```

(Adjust `cargoLock` if the repo has no committed `Cargo.lock` — generate one with `nix develop -c cargo generate-lockfile` and commit it; `buildRustPackage` requires it.)

- [ ] **Step 2: Verify build on host**

Run: `nix build .#springchick` → Expected: produces `result/bin/springchick`.
Run: `SPRINGCHICK_BACKEND=winit ./result/bin/springchick` (host) → winit window opens. Confirms the wrapped binary finds its runtime libs.

- [ ] **Step 3: Commit**

```bash
git add flake.nix Cargo.lock
git commit -m "build(m4): nix package output for on-device nix run

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Task 9: Runbook + on-device validation

**Files:**
- Create: `docs/RUNBOOK-device.md`

- [ ] **Step 1: Write the runbook**

Document the spec's runbook concretely:
```markdown
# Running springchick on the Fairphone 5

Device: `ssh dmsmobile` (Mobile NixOS, Phosh on VT1). Builds happen on-device.

1. ssh dmsmobile
2. git -C ~/springchick pull   # or clone first time
3. From the SSH shell, switch to a free getty and log in there:
     sudo chvt 3      # then log in on the device's VT3
4. On VT3:
     cd ~/springchick
     SPRINGCHICK_BACKEND=drm SPRINGCHICK_PERF=1 nix run .#springchick 2>&1 | tee /tmp/perf.log
5. Drive the loop by touch: home → tap icon (zoom open) → grab bar, drag up → release (home);
   then a horizontal bar flick (quick-switch).
6. Read the per-second line:  fps=.. p50=..ms p99=..ms dropped=.. n=..
7. Return to Phosh:  sudo chvt 1     (Ctrl-C on VT3 stops springchick)

If libseat refuses the seat over SSH, log in physically on the VT instead (seat must be the
active session). If the panel is upside-down/rotated, fix the connector transform in
drm_backend.rs (single place).
```

- [ ] **Step 2: On-device acceptance run**

Perform the runbook on `dmsmobile`. **Done criteria (spec):** springchick takes DRM master from the VT, renders home, the full touch loop (home → open → grab → home + one quick-switch) works, and the `FrameStats` line prints sustained numbers across the interaction. Record the observed numbers in the commit message / a note.

- [ ] **Step 3: Commit**

```bash
git add docs/RUNBOOK-device.md
git commit -m "docs(m4): on-device runbook + perf-capture steps

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Done

When all tasks are committed and the Task 9 acceptance run prints frame stats on the FP5, M4 is complete. The numbers — not a pass/fail threshold — are the deliverable; deciding what to do about them is follow-up work.
