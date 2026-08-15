# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

springchick is an iOS-Springboard-style Wayland compositor for mobile Linux (target device: Fairphone 5 on Mobile NixOS). It is a **fused** compositor: the shell UI (home grid, dock, gestures, animations, switcher) is drawn by Skia into the *same* EGL/GLES context Smithay uses to composite clients. There is no separate shell process.

## Build / test

Everything must run inside the nix devshell — `rust-toolchain.toml` pins stable via rust-overlay, so a bare `cargo` tries to rustup-download a toolchain and fails; `sc-compositor` also needs native libs (libudev, libseat, pkg-config, libclang for skia bindgen) that only the devshell provides.

```bash
nix develop --command bash -c 'cargo build -p sc-compositor'
nix develop --command bash -c 'cargo test --workspace'
nix develop --command bash -c 'cargo test -p sc-layout'          # single crate
nix develop --command bash -c 'cargo test -p sc-compositor ui_state::'  # single module/test
nix develop --command bash -c 'cargo check --tests'              # fast compile check
```

`nix develop --command true` warms the devshell but does **not** build the crates. A cold skia build is long — build in the background or with a large timeout.

Binary: `target/debug/springchick`. Backend chosen by `SPRINGCHICK_BACKEND` (`drm`, else winit).

### VM tests (headless, real DRM path)

```bash
nix build .#checks.aarch64-linux.vm-boot -L      # also: vm-switcher, vm-dialog, vm-rotation, vm-arrange, vm-lock
```

Always build the check matching `builtins.currentSystem` — cross-building runs the release tree under qemu-user emulation and SIGSEGVs rustc. `nix/package.nix` filters `src` to `Cargo.toml`/`Cargo.lock`/`crates/`, so edits under `nix/`, `tests/`, `docs/` don't rebuild the compositor.

### Running / driving it

Use the `run-springchick` skill (`.claude/skills/run-springchick/SKILL.md`) — it covers the nested-winit driver (`driver.sh`: build/up/client/send/shot/down) and the interactive VM driver, plus a long list of gotchas. Key one: **never `pkill foot`** — the user's own terminal is a foot window; kill by recorded PID only.

A running compositor always listens on `$XDG_RUNTIME_DIR/springchick-ipc.sock`; drive it with `springchick ipc <verb>` (`tap X Y`, `swipe X1 Y1 X2 Y2 [MS]`, `key NAME [MS]`, `down/move/up`, `settle [MS]`, `launch APP_ID [new]`, `reload`). Works nested, in the VM, and on-device.

`springchick ipc reload` re-reads `config.toml` live: keybinds, `card_radius`, `show_touches`, `prefer_no_csd` (next window to negotiate decorations), `idle_blank_secs` (countdown restarts). `dpi` and `uclamp_min` are ignored on reload — they need a restart.

`tests/integration.sh` is an older nested-winit smoke suite (sockets, multi-client, clean shutdown, keybinds); parts are being ported to the VM checks.

## Architecture

**Core principle: push logic into pure crates (no GPU, no Wayland deps) and unit-test it there. `sc-compositor` does integration and wiring only.**

```
crates/
  sc-anim/         Spring physics (critically damped, interruptible)
  sc-input/        Gesture recognizer (Tracker/Pt) + nav state machine (NavTarget)
  sc-shell-model/  Grid/dock data model (4x6 pages, dock) + persist (state.toml)
  sc-config/       config.toml parsing: [main] + [keybinds]. Lenient — bad entry dropped, rest applies
  sc-keys/         Short/long key-press timing rules (types live in sc-config)
  sc-catalog/      .desktop scan/parse + field-code stripping + search ranking
  sc-layout/       Pure geometry: (size, page, model) → icon rects, dock, dots, bar zone, icon-menu panel; + hit-testing
  sc-icons/        Icon theme lookup + resvg → raw RGBA (no Skia dep)
  sc-search/       Standalone pull-down search app (eframe/winit client, not part of the compositor)
  sc-compositor/   The `springchick` binary
```

All pure crates are `#![forbid(unsafe_code)]`.

### sc-compositor module map

`main.rs` is a thin entry point (arg dispatch → `ipc::run_client` or a backend). The real structure:

- `state.rs` — the central `State`: every protocol state object, shell model, input bookkeeping, and `FramePrep` (the backend-agnostic render snapshot). Behaviour lives in sibling `impl State` modules.
- `handlers.rs` — smithay protocol handler impls + `delegate_*` glue.
- `toplevel.rs` — app window lifecycle, focus, decoration, rotation.
- `ui_state.rs` — the pure state machine: `transition(&mut state, event) -> Effect`. States `Home`/`App`/`AppOpening`/`AppClosing`/`Grabbing`/`Settling`/`QuickSwitch`/`Switcher`; side effects are *returned*, never performed here.
- `scene.rs` — pure `compute_scene(state, output_size) -> Scene` (window transforms: scale, center, corner radius). No GPU deps.
- `input_dispatch.rs` / `input_common.rs` / `touch.rs` / `keybinds.rs` — input normalization to `Pt` (0..1) and routing by `UiState`.
- `arrange.rs` — home-grid reflow springs + arrange-mode drag. Arrange is entered by a long press on empty home background; a long press on an *icon* opens `icon_menu.rs` instead (Open — or one row per window, by title, when the app has several — / New window / Close / Remove).
- `provenance.rs` — which launch a mapped window belongs to (xdg-activation token, then process ancestry). Window identity comes from the launch, not the client-reported `app_id`, so a `Terminal=true` entry isn't tagged `foot` and each PWA keeps its own id.
- `frame.rs` — per-frame shell advance, popups, animation gating → `FramePrep`.
- `render.rs` — the shared render path both backends use (clear, two-pass transformed app composite, Skia home/bar overlay, blur regions, rounded-rect texture shader).
- `skia_gl.rs` — Skia-on-Smithay-GLES context sharing.
- `winit_backend.rs` / `drm_backend.rs` — the two ways to present; `session.rs` is the Wayland display/socket plumbing they share.
- `debug_input.rs` + `ipc.rs` — synthetic-input socket and its CLI client.
- Protocol extras: `layer_shell.rs`, `popups.rs`, `idle_notify.rs`, `idle_inhibit.rs`, `gamma_control.rs`, `background_effect.rs`, `content_type.rs`, `session_lock.rs`, `rotation.rs`, `blank.rs`, `osd.rs`, `touch_viz.rs`, `switcher.rs`, `frame_stats.rs`.

### Render pipeline (`render.rs`)

1. Tick animations → 2. compute scene → 3. Smithay pass 1: clear + fullscreen app (if not transitioning) → 4. Skia: home screen (if `scene.show_home`) → 5. Smithay pass 2: scaled app elements via `RescaleRenderElement` + `RelocateRenderElement` (if transitioning) → 6. Skia: bar overlay → 7. frame callbacks → 8. submit.

### Skia/Smithay GLES sharing rules

- `context.reset(None)` **before** any Skia draw (invalidates Skia's GL state cache).
- `context.flush_and_submit()` **after** Skia draws (pixels must land before swap).
- Cache the Skia `Surface` keyed on `(fboid, width, height)`; recreate only on change.
- The DRM `report_partial` damage fast path drops Skia overlays not listed in its guard — add new overlays there or they vanish over fullscreen apps.

### Smithay dependency

Pinned to a **fork** (`code.bas.es/marcus/smithay.git`, rev `ed8f054`), not crates.io. It carries xkbcommon 0.9 (fixes wvkbd keymap loading) plus a one-commit fix for the layer-surface destroy crash (Smithay#1979). Do not swap it back to a release without re-checking both. libinput/DRM/GBM/session types are used via `smithay::reexports::*` to avoid version skew.

## Configuration

`config.example.toml` documents every option at its compiled-in default. Lookup order: `$SPRINGCHICK_CONFIG` → `$XDG_CONFIG_HOME/springchick/config.toml` → `/etc/springchick/config.toml`. Persisted *state* (dock, pages, frecency) is separate: `sc_shell_model::persist` → `state.toml`.

Notable: `dpi` (default 3 — advertised via `wp_fractional_scale`; the FP5 panel is illegible at 1:1), `idle_blank_secs`, `card_radius`, `show_touches`, `prefer_no_csd`, `uclamp_min` (default `"auto"` — scheduler `util_min` floor held on the render thread while drawing, derived from CPU topology; see `uclamp.rs`).

Env vars: `SPRINGCHICK_BACKEND`, `SPRINGCHICK_CONFIG`, `SPRINGCHICK_IPC_SOCK`, `SPRINGCHICK_DEBUG_SOCK` (legacy), `SPRINGCHICK_WINIT_SIZE` (`WxH`), `SPRINGCHICK_OUTPUT`.

## Deployment

`nix/module.nix` exposes `programs.springchick.enable`, adds the package to `sessionPackages` so the greeter lists springchick as a wayland session (real logind seat, no seatd hack). `bin/springchick-session` is the same binary with `SPRINGCHICK_BACKEND=drm`. Device runbook: `docs/RUNBOOK-device.md` (build on-device over `ssh dmsmobile`).

Screen capture: `ext-image-copy-capture-v1` (dmabuf fast path on DRM, shm readback otherwise) plus `zwlr_screencopy_v1` (`wlr_screencopy.rs`, shm only) for wlr-era clients. Shared buffer plumbing in `capture.rs`; each backend has its own draw-and-read-back glue. `scripts/screenshot.sh` (grim), `scripts/record.sh` (wl-screenrec — `wf-recorder` will **not** work).

## Testing expectations

Pure logic gets a unit test — every state transition, layout computation, and gesture classification. Integration is smoke-level (compositor starts, accepts clients, doesn't crash) via the VM checks. Visual correctness and gesture feel are human-reviewed; there is no pixel-diff infra.

## Further reading

`CONTRIBUTING.md` covers the same ground at more length, plus Smithay/NixOS specifics, key patterns, common client warnings, and milestone status. `docs/RUNBOOK-device.md` covers on-device deployment.
