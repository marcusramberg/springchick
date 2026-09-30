# springchick development guide

## Project overview

springchick is an iOS Springboard-style Wayland compositor for mainline Linux
phones (the development device is a Fairphone 5). It is a fused compositor: the
shell UI (home grid, dock, gestures, animations, task switcher) is drawn by Skia
into the same GLES context Smithay uses for client compositing. There is no
separate shell process.

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

Every crate except `sc-compositor` and `sc-search` is `#![forbid(unsafe_code)]`.

Push logic into pure crates (no GPU, no Wayland deps) and test it heavily there.
The compositor crate does integration and wiring only.

### sc-compositor modules

`main.rs` is a thin entry point: it dispatches `springchick ipc …` to the IPC
client, then picks a backend from `SPRINGCHICK_BACKEND`.

| Module | Role |
| --- | --- |
| `state.rs` | The central `State`: protocol state objects, shell model, input bookkeeping, `FramePrep` render snapshot |
| `handlers.rs` | Smithay protocol handler impls and the `delegate_dispatch2!` call |
| `toplevel.rs` | App window lifecycle, focus, decoration, rotation |
| `ui_state.rs` | Pure state machine: `transition(&mut state, event) -> Effect` |
| `scene.rs` | Pure `compute_scene(state, output_size) -> Scene` (window transforms) |
| `input_dispatch.rs`, `input_common.rs`, `touch.rs`, `keybinds.rs`, `kbd_switch.rs` | Input normalization and routing by `UiState`; held-modifier (Super+Tab) switching |
| `arrange.rs`, `icon_menu.rs` | Home-grid reflow springs; arrange mode (long-press wiggle, drag to reorder/pin/unpin); the long-press icon menu |
| `library.rs` | The app library: a derived last home page of category folders |
| `frame.rs` | Per-frame shell advance, popups, animation gating |
| `render.rs` | Shared render path for both backends |
| `skia_gl.rs` | Skia-on-Smithay-GLES context sharing |
| `winit_backend.rs` / `drm_backend.rs` | The two ways to present |
| `session.rs`, `mirror.rs` | Wayland display/socket plumbing shared by both backends; external-display mirroring on DRM |
| `debug_input.rs`, `ipc.rs` | Synthetic-input socket and its CLI client |
| `app_history.rs`, `switcher.rs` | MRU stack and the task-switcher deck |
| `launcher.rs`, `provenance.rs`, `resources.rs` | Process spawning (one systemd scope per app); which launch a window belongs to; per-app cgroup resource tiers |
| `catalog_watch.rs` | inotify watch on the XDG `applications/` dirs |
| `layer_shell.rs`, `popups.rs`, `idle_notify.rs`, `idle_inhibit.rs`, `gamma_control.rs`, `output_power.rs`, `background_effect.rs`, `content_type.rs`, `session_lock.rs`, `presentation.rs`, `pacing.rs` | Protocol extras |
| `capture.rs`, `wlr_screencopy.rs`, `screenshot.rs` | Screen capture (see below) |
| `rotation.rs`, `sensor.rs`, `blank.rs`, `sleep.rs`, `uclamp.rs` | Device: rotation and its accelerometer, panel blanking, blank-before-suspend, render-thread `util_min` |
| `bar_hint.rs`, `osd.rs`, `touch_viz.rs` | Shell chrome |
| `backend.rs`, `frame_stats.rs` | Backend selection and dev-window size; frame timing |

## Building

Everything runs inside the nix devshell. `rust-toolchain.toml` pins stable via
rust-overlay, so bare `cargo` tries to rustup-download a toolchain and fails.
`sc-compositor` also needs native libs (libudev, libseat, pkg-config, libclang
for skia's bindgen) that only the devshell provides.

```bash
nix develop --command bash -c 'cargo build -p sc-compositor'
nix develop --command bash -c 'cargo check --tests'   # fast, no linking
```

`nix develop --command true` warms the shell but builds nothing. A cold skia
build is long, so run it in the background or with a large timeout.

The binary is `target/debug/springchick`. With no `SPRINGCHICK_BACKEND` it opens
a winit window (nested compositor) on the host Wayland session.
`SPRINGCHICK_BACKEND=drm` takes over a real DRM/KMS device.

## Testing

### Unit tests

```bash
nix develop --command bash -c 'cargo test --workspace'
nix develop --command bash -c 'cargo test -p sc-layout'                # one crate
nix develop --command bash -c 'cargo test -p sc-compositor ui_state::' # one module
```

Pure crates run without a display, GPU, or Wayland. They cover layout geometry
and hit-testing, icon resolution and placeholder fallback, grid/dock model
operations, spring convergence and interruptibility, gesture classification and
nav targets, .desktop parsing and search ranking, and key-press timing.

`sc-compositor`'s own `#[cfg(test)]` modules (`ui_state`, `scene`,
`app_history`, `backend`, …) link and run inside the devshell.

### Coverage

```bash
nix develop --command bash -c 'cargo llvm-cov --workspace --summary-only'
nix develop --command bash -c 'cargo llvm-cov --workspace --html'  # target/llvm-cov/html
```

The dev shell provides `cargo-llvm-cov` and adds `llvm-tools-preview` to the
pinned toolchain. That happens in `flake.nix` and not in `rust-toolchain.toml`,
so the package derivation and the VM checks don't rebuild.

Coverage instruments the unit tests only and cannot see the VM checks.
Everything that talks to Wayland, DRM, or the GPU (`render`, `toplevel`,
`touch`, `state`, `handlers`, the two backends) reports 0% even though `checks`
exercises it end to end. The figure is meaningful for the pure crates, which sit
at 89–99%. Read 0% on a wiring module as "no unit tests here, by design".

### VM tests (headless, real DRM path)

The `checks` in `flake.nix` boot springchick on its DRM backend inside a NixOS
QEMU VM (virtio-gpu + llvmpipe software GL), autologin the shipped session, and
assert against the guest journal and framebuffer screenshots.

```bash
nix build .#checks.aarch64-linux.vm-boot -L      # boot + client render + app_id
nix build .#checks.aarch64-linux.vm-switcher -L  # gesture semantics (MRU, quick-switch)
nix build .#checks.aarch64-linux.vm-dialog -L    # xdg-dialog / CSD child windows
nix build .#checks.aarch64-linux.vm-rotation -L  # rotation / content-type hints
nix build .#checks.aarch64-linux.vm-lock -L      # ext-session-lock (swaylock)
```

The rest follow the same pattern: `vm-arrange` (arrange mode), `vm-icon-menu`
(icon menu, multiple windows), `vm-library` (app library), `vm-capture`
(screencopy, with grim as the oracle), `vm-portal` (file chooser fit),
`vm-pointer` (USB mouse) and `vm-card-mask` (rounded mask on a viewported
client). Each `nix/vm-*-test.nix` opens with a comment saying what it asserts.

- Build the host arch and never cross-build. Cross-building runs the whole
  release tree under qemu-user emulation and SIGSEGVs rustc (`qemu: uncaught
  target signal 11`). Match `nix eval --raw --impure --expr
  builtins.currentSystem`.
- `nix/package.nix` filters `src` to `Cargo.toml`/`Cargo.lock`/`crates/`, so
  editing `nix/`, `tests/`, or `docs/` does not rebuild the compositor.
- Iterate live with `nix build .#checks.<sys>.vm-boot.driverInteractive` and
  drive `result/bin/nixos-test-driver`, which lets you probe a running VM with
  no recompile. It writes screenshots to `$CWD`, so `cd` somewhere writable
  first.
- Don't grep the guest journal for bare `panic`. The kernel cmdline (`panic=1`)
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
springchick ipc action screenshot
```

Verbs: `tap`, `swipe`, `key`, `down`/`move`/`up`, `settle`, `launch APP_ID
[new]`, `action` (run a built-in keybinding action by its `config.toml` name, no
key needed), `reload` (re-read `config.toml` and rescan the app catalog),
`layers` (dump the composited layer surfaces) and `quit`. They work nested, in
the VM, and on-device. Coordinates are in the actual output size, and a nested
host compositor may clamp the window well below the FP5 constants.

Nested iteration is wrapped by `.claude/skills/run-springchick/driver.sh`
(`build` / `up` / `client` / `send` / `shot` / `down`); see that skill's
`SKILL.md` for its full gotcha list.

`tests/integration.sh` is the older nested-winit shell suite (socket creation
and cleanup, client connect and close, multi-client, keybind short/long press,
clean shutdown). Parts are still being ported to the VM checks.

### What can't be tested automatically

Visual correctness (icon rendering, animation smoothness, transform quality) and
gesture feel (spring tuning, dead zones, interruptibility) need human eyes on a
screenshot or the device.

### Test philosophy

- Pure logic gets a unit test: every state transition, layout computation, and
  gesture classification.
- Integration gets a VM check: the compositor boots on real DRM, accepts
  clients, renders, and honours gestures.
- Visual changes get a human review, with screenshots shared inline. There is no
  pixel-diff infra.
- Write the test before or alongside the code. If a module is pure, there's no
  excuse for missing tests.

## Key patterns

### UiState machine (ui_state.rs)

All navigation goes through `transition(&mut state, event) -> Effect`. It is a
pure function whose only side effect is the state mutation. Effects
(`CloseToplevel`, `EnterSwitcher`) are returned for the caller to execute.

States: `Home`, `App`, `AppOpening`, `AppClosing`, `Grabbing`, `Settling`,
`QuickSwitch`, `Switcher`.
Events: `AppMapped`, `RaiseApp`, `ReturnHome`, `ToplevelClosed`, `GrabStart`,
`GrabMove`, `GrabRelease`, `Interrupt`, `Tick`, `EnterSwitcher`,
`OpenSwitcherFromHome`, `OpenSwitcherFromApp`, `SwitcherStep`, `HomeBounce`,
`SwitcherTapCard`, `SwitcherCloseCard`, `SwitcherDismiss`.

The switcher/quick-switch carousel puts the most recent app on the right. A
swipe right goes to the older app and a swipe left to the more recent one.

### Scene computation (scene.rs)

`compute_scene(state, output_size) -> Scene` maps the current UiState to the
`WindowTransform`s (scale, center, corner radius) the renderer applies. It is
pure and has no GPU deps.

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
smithay's `texture.frag` at the pinned rev, plus `corner_radius`/`card_rect`
uniforms and an SDF mask.

### Skia-on-Smithay GLES sharing

Skia and Smithay share one EGL/GLES context.

- Call `context.reset(None)` before any Skia draw (invalidates Skia's GL state
  cache).
- Call `context.flush_and_submit()` after Skia draws (pixels must land before
  swap).
- Cache the Skia `Surface` keyed on `(fboid, width, height)` and recreate it
  only on change.
- On DRM, `SkiaGl::finish_gpu()` (glFinish) must run before the page-flip.
  Otherwise buffers get presented before the GPU has finished, which shows as
  tearing.
- The DRM partial-damage fast path only redraws regions it knows about. A Skia
  overlay missing from its guard silently disappears over a fullscreen app, so
  add new overlays there.
- The GBM scanout buffer is vertically flipped relative to winit. Skia chrome
  needs `flip_y` on DRM while the Wayland app layer keeps `Transform::Normal`,
  because Skia bypasses smithay's output transform.

### Input dispatch

All pointer/touch events are normalized to `Pt` (0..1) and routed on UiState:

- Home: hit-test icons, start a page drag, or begin a long press (arrange mode
  on empty background, the icon menu on an icon)
- App: a touch in the bar zone starts a grab; anything else is forwarded to the
  client
- Grabbing: update tracker
- Settling/Opening/Closing: interrupt

## Smithay specifics

- Smithay is pinned to upstream git (`https://github.com/Smithay/smithay.git`
  rev `7ddcd17`) because crates.io lacks xkbcommon 0.9, which fixes wvkbd keymap
  loading (the xkbcommon 0.8 `size-1` bug). Don't swap back to a release without
  re-checking that. Dispatch goes through the single
  `delegate_dispatch2!(State)` in `handlers.rs`; the per-protocol `delegate_*!`
  macros no longer exist upstream.
- Do not remove the `use_system_lib` feature. It picks libwayland-server over
  the pure-Rust `wayland-backend`, and that choice decides whether two smithay
  bugs are fatal. Several role handlers post a protocol error from a
  `wl_surface` pre-commit hook after the role object is destroyed (the "destroy
  role, attach nil, commit" teardown every Qt/quickshell client does on close).
  The Rust backend delivers that error on the dead object and kills the client;
  libwayland drops it. Two known instances: layer surfaces (Smithay#1979,
  `width 0 requested without setting left and right anchors`, dms panel close)
  and lock surfaces (`Committed before the first ack_configure.`, dms unlock).
  Both reproduce with a ~15-line quickshell client the moment the feature is
  removed. With the feature on, no smithay fork is needed.
- libinput / DRM / GBM / session types are used via `smithay::reexports::*` to
  avoid version skew. calloop 0.14 is declared directly only to turn on its
  `signals` feature (SIGTERM handling), via feature unification.
- Protocols implemented: compositor, xdg-shell, xdg-decoration, xdg-dialog,
  layer-shell, shm, dmabuf, seat, wl_output, xdg-output, viewporter,
  fractional-scale, content-type, text-input, input-method, virtual-keyboard,
  idle-inhibit, idle-notify, data-device, primary-selection, ext-data-control,
  wlr-data-control, xdg-activation, ext-image-capture-source,
  ext-image-copy-capture, wlr-screencopy, wlr-gamma-control,
  wlr-output-power-management, ext-background-effect, session-lock,
  presentation-time, fifo, commit-timing.
- Not implemented (expect the odd client warning): cursor-shape,
  pointer-constraints (the handler exists to satisfy a trait bound, but no
  global is advertised), explicit sync (linux-drm-syncobj), xdg-toplevel-icon.
  The `xwayland` cargo feature is enabled but no XWayland is wired up yet.
- Client-facing timestamps (frame callbacks, input events, presentation
  feedback) are all CLOCK_MONOTONIC. Clients do arithmetic across them, and a
  process-local epoch breaks that silently.
- Top-level apps are configured Maximized. Maximized fills the screen while
  leaving toolkits their normal layout, which keeps a dialog's buttons on
  screen. Fullscreen is set only when a client asks for it (e.g. video).
  `prefer_no_csd` (default true) asks for server-side decoration on toplevels;
  child windows always keep client-side decoration so GTK still draws the header
  bar holding a file chooser's Open/Cancel.

## Configuration

`config.example.toml` documents every option at its compiled-in default. Lookup
order: `SPRINGCHICK_CONFIG`, then `$XDG_CONFIG_HOME/springchick/config.toml`,
then `/etc/springchick/config.toml`. Validation is deliberately lenient: a bad
entry is dropped with a warning and the rest still applies. On a phone, refusing
to start over a config typo means a recovery session, while a skipped binding is
just a dead button.

Persisted state (dock, pages, frecency) is separate: `sc_shell_model::persist`
writes `state.toml`.

Environment: `SPRINGCHICK_BACKEND` (`drm`, else winit), `SPRINGCHICK_CONFIG`,
`SPRINGCHICK_IPC_SOCK`, `SPRINGCHICK_DEBUG_SOCK` (legacy),
`SPRINGCHICK_WINIT_SIZE` (`WxH`). `SPRINGCHICK_OUTPUT` is read only by
`scripts/screenshot.sh` and `scripts/record.sh`.

## NixOS specifics

- App .desktop files: `/run/current-system/sw/share/applications/`,
  `/etc/profiles/per-user/$USER/share/applications/`,
  `~/.local/share/applications/`. springchick scans `XDG_DATA_DIRS` at startup,
  then rescans when `catalog_watch.rs` sees an `applications/` dir change or on
  `springchick ipc reload`.
- Icons: `/run/current-system/sw/share/icons/hicolor/` (not `/usr/share/icons/`).
- `nix/module.nix` exposes `programs.springchick.enable`. It installs the package
  and registers it in `services.displayManager.sessionPackages`, so the greeter
  lists springchick as a Wayland session and hands it a real logind seat, with
  no seatd-over-SSH hack. `bin/springchick-session` (source:
  `nix/springchick-session`) is a shell wrapper that starts the compositor as
  the `springchick.service` user unit, which is what brings up
  `graphical-session.target`. Without systemd it falls back to exec'ing the
  binary with `SPRINGCHICK_BACKEND=drm` and `XDG_SESSION_TYPE=wayland`.
- The service is `Type=notify` (via `sd-notify`): "active" means DRM master
  taken and the first frame rendered. The notify call is a no-op when
  `NOTIFY_SOCKET` is unset, so a bare VT launch still works.
- Skia in the Nix build: `skia-bindings` would fetch prebuilt Skia over the
  network. `docs/RUNBOOK-device.md` explains how that's handled.
- Device deployment and on-device builds: `docs/RUNBOOK-device.md`.

## Screen capture

springchick implements `ext-image-copy-capture-v1` (the wlr-screencopy
successor), so capture is zero-copy into the client's dmabuf. Clients that
allocate shm instead (grim does) get a readback path: the scene is redrawn into
an offscreen texture and copied into their pool.

`zwlr_screencopy_v1` is also implemented (`wlr_screencopy.rs`), shm only, for
wlr-era clients. Both protocols share the buffer plumbing in `capture.rs`
(`shm_target` / `offscreen` / `readback_into_shm`). The draw-and-read-back glue
around it is per-backend (`capture_region_shm` in each of `drm_backend.rs` and
`winit_backend.rs`) because the two draw the scene differently.

- `scripts/screenshot.sh` uses grim. Run this first after touching capture code;
  if it writes a correct PNG the protocol and readback are good.
- `scripts/record.sh` uses wf-recorder over wlr-screencopy, with hardware h264
  (`h264_v4l2m2m`) by default. wl-screenrec speaks ext-image-copy-capture but
  its pipeline is VAAPI-only, which the FP5's Adreno lacks, so it can only do
  software x264 there.

## Common issues

- SVG parse warnings (marker-start/mid/end) are harmless resvg noise from system
  icons using unsupported SVG features.
- "compositor does not provide required interfaces" from a GTK app means a
  missing optional protocol from the list above. Most GTK4/libadwaita apps still
  work.
- An app that renders as a small card in the top-left instead of filling the
  window means the output-scale render path regressed. See `app_scale` in
  `render.rs`. `[main].dpi` (default 3) is what makes clients render at output
  scale.
- Two instances at once: a leftover springchick keeps `springchick-0`, so the
  next instance bumps to `springchick-1` while new clients still connect to the
  stale one. Confirm exactly one socket before testing.
- Never `pkill foot` on a dev box. The developer's own terminal is usually a
  foot window. Kill by recorded PID.

## Milestones

- M1 (done): Foundation. Pure crates + Skia-on-Smithay spike.
- M2 (done): Home screen + app launch + fullscreen compositing.
- M3 (done): Bottom-bar gestures + app transitions (grab/shrink/settle), task
  switcher, quick-switch.
- M4 (done): Device backend + perf validation. DRM/KMS + libinput on the FP5 at
  1224x2700@90, render cost p50 ~4.9ms / p99 ~5.4ms against an 11.1ms budget. M4
  and M5 were swapped on 2026-06-27 to de-risk the animation-perf unknown on
  real hardware first.
- M5 (in progress): Shell features. Arrange mode (long-press wiggle, drag to
  reorder / pin / unpin) and pull-down search have shipped; folders and
  page-reorder are still open.
