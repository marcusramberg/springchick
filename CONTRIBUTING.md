# springchick — Development Guide

## Project overview

springchick is an iOS Springboard-style Wayland compositor for mobile Linux
(Fairphone 5, Mobile NixOS). Fused compositor: shell UI (home grid, dock,
gestures, animations, task switcher) drawn by Skia into the same GLES context
Smithay uses for client compositing. No separate shell process.

## Architecture

```
crates/
  sc-anim/         Pure spring physics engine
  sc-input/        Pure gesture recognizer + nav state machine
  sc-shell-model/  Pure grid/dock data model (4x6 pages, dock, frecency) + persist
  sc-config/       config.toml parsing ([main] + [keybinds])
  sc-catalog/      .desktop scan/parse, field-code stripping, search ranking
  sc-keys/         Short/long key-press timing rules (pure; types live in sc-config)
  sc-layout/       Pure geometry: (size, page, model) → icon rects + hit-testing
  sc-icons/        Icon resolution (filesystem IO + resvg). Returns raw RGBA pixels.
  sc-search/       Standalone pull-down search app (eframe client, not the compositor)
  sc-compositor/   The binary. Smithay + Skia + calloop + all wiring.
```

Every pure crate is `#![forbid(unsafe_code)]`.

**Key principle:** push logic into pure crates (no GPU, no Wayland deps), test
heavily there. The compositor crate does integration/wiring only.

### sc-compositor modules

`main.rs` is a thin entry point only: it dispatches `springchick ipc …` to the
IPC client, then picks a backend from `SPRINGCHICK_BACKEND`.

| Module | Role |
| --- | --- |
| `state.rs` | The central `State`: protocol state objects, shell model, input bookkeeping, `FramePrep` render snapshot |
| `handlers.rs` | Smithay protocol handler impls + `delegate_*` glue |
| `toplevel.rs` | App window lifecycle, focus, decoration, rotation |
| `ui_state.rs` | Pure state machine — `transition(&mut state, event) -> Effect` |
| `scene.rs` | Pure `compute_scene(state, output_size) -> Scene` (window transforms) |
| `input_dispatch.rs`, `input_common.rs`, `touch.rs`, `keybinds.rs` | Input normalization + routing by `UiState` |
| `arrange.rs` | Home-grid reflow springs; arrange mode (long-press wiggle, drag to reorder/pin/unpin) |
| `frame.rs` | Per-frame shell advance, popups, animation gating |
| `render.rs` | Shared render path for both backends |
| `skia_gl.rs` | Skia-on-Smithay-GLES context sharing |
| `winit_backend.rs` / `drm_backend.rs` | The two ways to present |
| `session.rs` | Wayland display/socket plumbing shared by both backends |
| `debug_input.rs`, `ipc.rs` | Synthetic-input socket + its CLI client |
| `app_history.rs`, `switcher.rs` | MRU stack and the task-switcher deck |
| `launcher.rs` | Process spawning (strip field codes, set env) |
| `layer_shell.rs`, `popups.rs`, `idle_notify.rs`, `idle_inhibit.rs`, `gamma_control.rs`, `background_effect.rs`, `content_type.rs`, `session_lock.rs`, `rotation.rs`, `blank.rs`, `osd.rs`, `touch_viz.rs` | Protocol extras and shell chrome |
| `backend.rs`, `frame_stats.rs` | Backend selection/dev-window size; frame timing (`SPRINGCHICK_PERF`) |

## Building

Everything runs inside the nix devshell. `rust-toolchain.toml` pins stable via
rust-overlay, so bare `cargo` tries to rustup-download a toolchain and fails;
`sc-compositor` additionally needs native libs (libudev, libseat, pkg-config,
libclang for skia's bindgen) that only the devshell provides.

```bash
nix develop --command bash -c 'cargo build -p sc-compositor'
nix develop --command bash -c 'cargo check --tests'   # fast, no linking
```

`nix develop --command true` warms the shell but builds nothing. A cold skia
build is long — run it in the background or with a large timeout.

The binary is `target/debug/springchick`. With no `SPRINGCHICK_BACKEND` it opens
a winit window (nested compositor) on the host Wayland session;
`SPRINGCHICK_BACKEND=drm` takes over a real DRM/KMS device.

## Testing

### Unit tests

```bash
nix develop --command bash -c 'cargo test --workspace'
nix develop --command bash -c 'cargo test -p sc-layout'                # one crate
nix develop --command bash -c 'cargo test -p sc-compositor ui_state::' # one module
```

Pure crates run without a display, GPU, or Wayland. They cover layout geometry +
hit-testing, icon resolution + placeholder fallback, grid/dock model operations,
spring convergence + interruptibility, gesture classification + nav targets,
.desktop parsing + search ranking, and key-press timing.

`sc-compositor`'s own `#[cfg(test)]` modules (`ui_state`, `scene`,
`app_history`, `backend`, …) do link and run inside the devshell.

### Coverage

```bash
nix develop --command bash -c 'cargo llvm-cov --workspace --summary-only'
nix develop --command bash -c 'cargo llvm-cov --workspace --html'  # target/llvm-cov/html
```

The dev shell provides `cargo-llvm-cov` and adds `llvm-tools-preview` to the
pinned toolchain (in `flake.nix`, deliberately not in `rust-toolchain.toml` —
see the comment there).

**Read the number with its blind spot in mind: it instruments the unit tests
only, so it cannot see the VM checks at all.** Everything that talks to Wayland,
DRM, or the GPU — `render`, `toplevel`, `touch`, `state`, `handlers`, the two
backends — reports 0% while actually being exercised end-to-end by `checks`.
The figure is useful for the pure logic, where it is meaningful and high (the
pure crates sit at 89–99%); treat 0% on a wiring module as "no unit tests here,
by design", not as "unverified".

### VM tests (headless, real DRM path)

The `checks` in `flake.nix` boot springchick on its **DRM** backend inside a
NixOS QEMU VM (virtio-gpu + llvmpipe software GL), autologin the shipped
session, and assert against the guest journal and framebuffer screenshots.

```bash
nix build .#checks.aarch64-linux.vm-boot -L      # boot + client render + app_id
nix build .#checks.aarch64-linux.vm-switcher -L  # gesture semantics (MRU, quick-switch)
nix build .#checks.aarch64-linux.vm-dialog -L    # xdg-dialog / CSD child windows
nix build .#checks.aarch64-linux.vm-rotation -L  # rotation / content-type hints
nix build .#checks.aarch64-linux.vm-lock -L      # ext-session-lock (swaylock)
```

- **Build the host arch, never cross-build.** Cross-building runs the whole
  release tree under qemu-user emulation and SIGSEGVs rustc (`qemu: uncaught
  target signal 11`). Match `nix eval --raw --impure --expr
  builtins.currentSystem`.
- `nix/package.nix` filters `src` to `Cargo.toml`/`Cargo.lock`/`crates/`, so
  editing `nix/`, `tests/`, or `docs/` does **not** rebuild the compositor.
- Iterate live with `nix build .#checks.<sys>.vm-boot.driverInteractive` and
  drive `result/bin/nixos-test-driver` — probe a running VM with no recompile.
  It writes screenshots to `$CWD`, so `cd` somewhere writable first.
- Don't grep the guest journal for bare `panic` — the kernel cmdline (`panic=1`)
  and virtio-gpu's `drm panic` planes both match. Use
  `panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault`.
- DRM `Permission denied` errors at `machine.shutdown()` are benign: logind
  revokes DRM master as the seat tears down.

### Driving a running compositor

The compositor always listens on `$XDG_RUNTIME_DIR/springchick-ipc.sock`
(override with `SPRINGCHICK_IPC_SOCK`; `SPRINGCHICK_DEBUG_SOCK` is the legacy
name). `springchick ipc <verb>` sends one line and prints the reply:

```bash
springchick ipc tap 640 400
springchick ipc swipe 640 788 1080 788 500
springchick ipc key XF86AudioRaiseVolume 900
springchick ipc settle 1000
```

Verbs: `tap`, `swipe`, `key`, `down`/`move`/`up`, `settle`. Works nested, in the
VM, and on-device. Coordinates are in the **actual** output size — a nested host
compositor may clamp the window well below the FP5 constants.

Nested iteration is wrapped by `.claude/skills/run-springchick/driver.sh`
(`build` / `up` / `client` / `send` / `shot` / `down`); see that skill's
`SKILL.md` for its full gotcha list.

`tests/integration.sh` is the older nested-winit shell suite (socket creation,
multi-client, clean shutdown, Esc-to-home, keybind short/long press). Parts are
still being ported to the VM checks.

### What can't be tested automatically

- Visual correctness (icon rendering, animation smoothness, transform quality)
- Gesture feel (spring tuning, dead zones, interruptibility)
- These need human eyes on a screenshot or the device.

### Test philosophy

- **Pure logic → unit test.** Every state transition, layout computation, and
  gesture classification has a unit test.
- **Integration → VM check.** Compositor boots on real DRM, accepts clients,
  renders, and honours gestures.
- **Visual → human.** Screenshots shared inline for review. No pixel-diff infra.
- **Write the test before or alongside the code.** If a module is pure, there's
  no excuse for missing tests.

## Key patterns

### UiState machine (ui_state.rs)

All navigation goes through `transition(&mut state, event) -> Effect`. Pure
function, no side effects except the state mutation. Effects (Launch,
CloseToplevel, …) are returned for the caller to execute.

States: `Home`, `App`, `AppOpening`, `AppClosing`, `Grabbing`, `Settling`,
`QuickSwitch`, `Switcher`.
Events: `AppMapped`, `RaiseApp`, `ReturnHome`, `ToplevelClosed`, `GrabStart`,
`GrabMove`, `GrabRelease`, `Interrupt`, `Tick`, `EnterSwitcher`,
`SwitcherTapCard`, `SwitcherCloseCard`, `SwitcherDismiss`.

The switcher/quick-switch carousel puts the most-recent app on the **right**:
swipe right → older app, swipe left → more-recent.

### Scene computation (scene.rs)

`compute_scene(state, output_size) -> Scene` maps the current UiState to
`WindowTransform`s (scale, center, corner radius) the renderer applies. Pure,
testable, no GPU deps.

### Render pipeline (render.rs)

Both backends own a `GlesRenderer` and differ only in how they acquire the
framebuffer (`bind`) and present (`submit` / page-flip). Everything between is
shared:

1. Tick animations (`UiEvent::Tick`)
2. Compute scene
3. Smithay pass 1: clear background + fullscreen app (if not transitioning)
4. Skia: draw home screen (if `scene.show_home`)
5. Smithay pass 2: draw scaled app elements (if transitioning, using
   `RescaleRenderElement` + `RelocateRenderElement`)
6. Skia: draw bar overlay and any OSD/touch-viz chrome
7. Send frame callbacks to client
8. Submit

Rounded app cards come from a custom fragment shader derived verbatim from
smithay's `texture.frag` at the pinned rev, plus `corner_radius`/`card_size`
uniforms and an SDF mask.

### Skia-on-Smithay GLES sharing

Skia and Smithay share one EGL/GLES context. Key rules:

- Call `context.reset(None)` before any Skia draw (invalidates Skia's GL state
  cache).
- Call `context.flush_and_submit()` after Skia draws (ensures pixels land before
  swap).
- Cache the Skia `Surface` keyed on `(fboid, width, height)` — recreate only on
  change.
- On DRM, `SkiaGl::finish_gpu()` (glFinish) must run before the page-flip or
  buffers get presented before the GPU finished (visible tearing).
- The DRM partial-damage fast path only redraws regions it knows about — a Skia
  overlay missing from its guard silently disappears over a fullscreen app. Add
  new overlays there.
- The GBM scanout buffer is vertically flipped relative to winit: Skia chrome
  needs `flip_y` on DRM while the Wayland app layer keeps `Transform::Normal`
  (Skia bypasses smithay's output transform, so the two layers differ).

### Input dispatch

All pointer/touch events → normalized `Pt` (0..1) → routed on UiState:

- Home: hit-test icons, start a page drag, or begin a long-press (arrange mode)
- App: bar zone → grab start, else forward to client
- Grabbing: update tracker
- Settling/Opening/Closing: interrupt

## Smithay specifics

- **Pinned to upstream git**, not crates.io:
  `https://github.com/Smithay/smithay.git` rev `7ddcd17`.
  Needed for xkbcommon 0.9, which fixes wvkbd keymap loading — the xkbcommon
  0.8 `size-1` bug. Don't swap back to a release without re-checking that.
  Dispatch goes through the single `delegate_dispatch2!(State)` in
  `handlers.rs`; the per-protocol `delegate_*!` macros no longer exist upstream.
- **`use_system_lib` is load-bearing.** It picks libwayland-server over the
  pure-Rust `wayland-backend`, and that choice decides whether two smithay bugs
  are fatal. Several role handlers post a protocol error from a `wl_surface`
  pre-commit hook *after* the role object is destroyed — the "destroy role →
  attach nil → commit" teardown every Qt/quickshell client does on close. The
  Rust backend delivers that error on the dead object and kills the client;
  libwayland drops it. Two known instances: layer surfaces (Smithay#1979,
  `width 0 requested without setting left and right anchors`, dms panel close)
  and lock surfaces (`Committed before the first ack_configure.`, dms unlock).
  Both reproduce with a ~15-line quickshell client the moment the feature is
  removed. This is why we no longer carry a smithay fork.
- libinput / DRM / GBM / session types are used via `smithay::reexports::*` to
  avoid version skew. calloop 0.14 is declared directly only to turn on its
  `signals` feature (SIGTERM handling), via feature unification.
- Protocols implemented: compositor, xdg-shell, xdg-decoration, xdg-dialog,
  layer-shell, shm, dmabuf, seat, wl_output, viewporter, fractional-scale,
  content-type, text-input, input-method, virtual-keyboard, idle-inhibit,
  idle-notify, data-device, primary-selection, ext-data-control,
  xdg-activation, ext-image-capture-source, ext-image-copy-capture,
  ext-background-effect, session-lock, presentation-time, fifo, commit-timing.
- Not implemented (expect the odd client warning): cursor-shape,
  pointer-constraints (the handler exists to satisfy a trait bound, but no
  global is advertised), explicit sync (linux-drm-syncobj), xdg-toplevel-icon.
  The `xwayland` cargo feature is enabled but no XWayland is wired up yet.
- Client-facing timestamps are all CLOCK_MONOTONIC — frame callbacks, input
  events and presentation feedback. Clients do arithmetic across them; a
  process-local epoch breaks that silently.
- Top-level apps are configured **Maximized**, not Fullscreen — maximized fills
  the screen while leaving toolkits their normal layout (which keeps a dialog's
  buttons on screen). Fullscreen is set only when a client asks for it (e.g.
  video). `prefer_no_csd` (default true) asks for server-side decoration on
  toplevels; child windows always keep client-side decoration so GTK still draws
  the header bar holding a file chooser's Open/Cancel.

## Configuration

`config.example.toml` documents every option at its compiled-in default. Lookup
order: `SPRINGCHICK_CONFIG` → `$XDG_CONFIG_HOME/springchick/config.toml` →
`/etc/springchick/config.toml`. Validation is deliberately lenient: a bad entry
is dropped with a warning and the rest still applies — on a phone, refusing to
start over a config typo means a recovery session, while a skipped binding is
just a dead button.

Persisted *state* (dock, pages, frecency) is separate: `sc_shell_model::persist`
→ `state.toml`.

Environment: `SPRINGCHICK_BACKEND` (`drm`, else winit), `SPRINGCHICK_CONFIG`,
`SPRINGCHICK_IPC_SOCK`, `SPRINGCHICK_DEBUG_SOCK` (legacy),
`SPRINGCHICK_WINIT_SIZE` (`WxH`), `SPRINGCHICK_OUTPUT`, `SPRINGCHICK_PERF`.

## NixOS specifics

- App .desktop files: `/run/current-system/sw/share/applications/`,
  `/etc/profiles/per-user/$USER/share/applications/`,
  `~/.local/share/applications/`. springchick scans `XDG_DATA_DIRS` **at
  startup**.
- Icons: `/run/current-system/sw/share/icons/hicolor/` (not `/usr/share/icons/`).
- `nix/module.nix` exposes `programs.springchick.enable`: it installs the package
  and registers it in `services.displayManager.sessionPackages`, so the greeter
  lists springchick as a Wayland session and hands it a real logind seat — no
  seatd-over-SSH hack needed. `bin/springchick-session` is the same binary with
  `SPRINGCHICK_BACKEND=drm` + `XDG_SESSION_TYPE=wayland`.
- The service is `Type=notify` (via `sd-notify`): "active" means DRM master
  taken and the first frame rendered. It no-ops when `NOTIFY_SOCKET` is unset,
  so a bare VT launch still works.
- Skia in the Nix build: `skia-bindings` would fetch prebuilt Skia over the
  network — see `docs/RUNBOOK-device.md` for how that's handled.
- Device deployment and on-device builds: `docs/RUNBOOK-device.md`.

## Screen capture

springchick implements `ext-image-copy-capture-v1` (the wlr-screencopy
successor), so capture is zero-copy into the client's dmabuf. Clients that
allocate shm instead (grim does) get a readback path: the scene is redrawn into
an offscreen texture and `glReadPixels`'d into their pool.

`zwlr_screencopy_v1` is also implemented (`wlr_screencopy.rs`), shm only, for
wlr-era clients. Both protocols share the buffer plumbing in `capture.rs`
(`shm_target` / `offscreen` / `readback_into_shm`); the draw-and-read-back glue
around it is per-backend (`capture_region_shm` in each of `drm_backend.rs` and
`winit_backend.rs`) because the two draw the scene differently.

- `scripts/screenshot.sh` — grim. Run this first after touching capture code; if
  it writes a correct PNG the protocol + readback are good.
- `scripts/record.sh` — wl-screenrec with hardware h264. **wf-recorder does not
  work** — it only speaks wlr-screencopy.

## Common issues

- **SVG parse warnings (marker-start/mid/end):** harmless resvg noise from system
  icons using unsupported SVG features.
- **"compositor does not provide required interfaces" (GTK apps):** a missing
  optional protocol from the list above. Most GTK4/libadwaita apps still work.
- **App renders as a small card in the top-left instead of filling the window:**
  the output-scale render path regressed — see `app_scale` in `render.rs`.
  `[main].dpi` (default 3) is what makes clients render at output scale.
- **Two instances at once:** a leftover springchick keeps `springchick-0`, so the
  next instance bumps to `springchick-1` while new clients still connect to the
  stale one. Confirm exactly one socket before testing.
- **Never `pkill foot`** on a dev box — the developer's own terminal is usually a
  foot window. Kill by recorded PID.

## Milestones

- **M1** (done): Foundation. Pure crates + Skia-on-Smithay spike.
- **M2** (done): Home screen + app launch + fullscreen compositing.
- **M3** (done): Bottom-bar gestures + app transitions (grab/shrink/settle),
  task switcher, quick-switch.
- **M4** (done): Device backend + perf validation. DRM/KMS + libinput on the
  FP5 — 1224x2700@90, render cost p50 ~4.9ms / p99 ~5.4ms, comfortably inside
  the 11.1ms budget. M4 and M5 were **swapped** on 2026-06-27 to de-risk the
  animation-perf unknown on real hardware first.
- **M5** (in progress): Shell features. Arrange mode (long-press wiggle, drag to
  reorder / pin / unpin) and pull-down search have shipped; folders and
  page-reorder are still open.
