# AGENTS.md

## What this is

springchick is an iOS-Springboard-style Wayland compositor for mainline Linux
phones. It is a fused compositor: the shell UI (home grid, dock, gestures,
animations, switcher) is drawn by Skia into the same EGL/GLES context Smithay
uses to composite clients. There is no separate shell process.

## Build / test

Everything must run inside the nix devshell. `rust-toolchain.toml` pins stable
via rust-overlay, so a bare `cargo` tries to rustup-download a toolchain and
fails. `sc-compositor` also needs native libs (libudev, libseat, pkg-config,
libclang for skia bindgen).

```bash
nix develop --command bash -c 'cargo build -p sc-compositor'
nix develop --command bash -c 'cargo test --workspace'
nix develop --command bash -c 'cargo test -p sc-layout'          # single crate
nix develop --command bash -c 'cargo test -p sc-compositor ui_state::'  # single module/test
nix develop --command bash -c 'cargo check --tests'              # fast compile check
```

`nix develop --command true` warms the devshell. A cold skia build is long, so build in the background or with a large timeout.

Binary: `target/debug/springchick`. Backend chosen by `SPRINGCHICK_BACKEND` (`drm`, else winit).

### VM tests (headless, real DRM path)

```bash
nix build .#checks.aarch64-linux.vm-boot -L
```

The other checks are `vm-switcher`, `vm-dialog`, `vm-rotation`, `vm-arrange`, `vm-icon-menu`, `vm-library`, `vm-lock`, `vm-capture`, `vm-portal`, `vm-pointer` and `vm-card-mask` (listed in `flake.nix`).

Build the check matching `builtins.currentSystem`. Cross-building runs the release tree under qemu-user emulation, which SIGSEGVs rustc. `nix/package.nix` filters `src` to `Cargo.toml`/`Cargo.lock`/`crates/`, so edits under `nix/`, `tests/`, `docs/` don't rebuild the compositor.

### Running / driving it

Use the `run-springchick` skill (`.claude/skills/run-springchick/SKILL.md`). It covers the nested-winit driver (`driver.sh`: build/up/client/send/shot/down), the interactive VM driver, and a long list of gotchas. Never `pkill foot`: the user's own terminal is a foot window, so kill by recorded PID only.

A running compositor always listens on `$XDG_RUNTIME_DIR/springchick-ipc.sock`; drive it with `springchick ipc <verb>` (`tap X Y`, `swipe X1 Y1 X2 Y2 [MS]`, `key NAME [MS]`, `down/move/up`, `settle [MS]`, `launch APP_ID [new]`, `action NAME`, `reload`, `layers`, `quit`). Works nested, in the VM, and on-device.

`springchick ipc layers` dumps every layer surface and layer-rooted popup being composited (namespace, layer, logical geometry, the physical rect, buffer size, pending-map/slide state, anchor and exclusive zone), plus the usable area and regrow-guard state. Run it first when the screen shows something no client admits to, e.g. two on-screen keyboards from one wvkbd process.

`springchick ipc reload` re-reads `config.toml` live: keybinds, `card_radius`, `show_touches`, `prefer_no_csd` (next window to negotiate decorations), `natural_scroll` (next touchpad added), `idle_blank_secs` (countdown restarts), `rotation_settle_ms`/`rotation_fade_ms` (next turn). `dpi`, `uclamp_min` and `vrr` are ignored on reload and need a restart.

`reload` also rescans the app catalog (`.desktop` files + icons): newly installed apps land on Home, uninstalled ones leave pages/dock/hidden and lose their frecency stats, and the renderer's uploaded icon textures are dropped so re-themed icons repaint. The catalog rescans itself too: `catalog_watch.rs` inotify-watches every XDG `applications/` dir, so a package-manager install/remove lands without an explicit reload. The watch has a 750 ms debounce, and the rescan runs on the compositor thread at the next event-loop wake, so a blanked screen costs nothing.

`tests/integration.sh` is an older nested-winit smoke suite (sockets, multi-client, clean shutdown, keybinds); parts are being ported to the VM checks.

## Architecture

Push logic into pure crates (no GPU, no Wayland deps) and unit-test it there. `sc-compositor` does integration and wiring only.

```
crates/
  sc-anim/         Spring physics (critically damped, interruptible)
  sc-catalog/      .desktop scan/parse + field-code stripping + search ranking
  sc-compositor/   The `springchick` binary
  sc-config/       config.toml parsing: [main] + [keybinds]. Lenient — bad entry dropped, rest applies
  sc-icons/        Icon theme lookup + resvg → raw RGBA (no Skia dep)
  sc-input/        Gesture recognizer (Tracker/Pt) + nav state machine (NavTarget)
  sc-keys/         Short/long key-press timing rules (types live in sc-config)
  sc-layout/       Pure geometry: (size, page, model) → icon rects, dock, dots, bar zone, icon-menu panel; + hit-testing
  sc-search/       Standalone pull-down search app (eframe/winit client, not part of the compositor)
  sc-shell-model/  Grid/dock data model (4x6 pages, dock) + persist (state.toml)
```

Every crate except `sc-compositor` and `sc-search` is `#![forbid(unsafe_code)]`.

### sc-compositor module map

`main.rs` is a thin entry point (arg dispatch to `ipc::run_client` or a backend). The real structure:

- `state.rs`: the central `State`, holding every protocol state object, the shell model, input bookkeeping, and `FramePrep` (the backend-agnostic render snapshot). Behaviour lives in sibling `impl State` modules.
- `handlers.rs`: smithay protocol handler impls and the `delegate_dispatch2!` call.
- `toplevel.rs`: app window lifecycle, focus, decoration, rotation.
- `ui_state.rs`: the pure state machine, `transition(&mut state, event) -> Effect`. States are `Home`/`App`/`AppOpening`/`AppClosing`/`Grabbing`/`Settling`/`QuickSwitch`/`Switcher`. Side effects are returned, never performed here.
- `scene.rs`: pure `compute_scene(state, output_size) -> Scene` (window transforms: scale, center, corner radius). No GPU deps.
- `input_dispatch.rs` / `input_common.rs` / `touch.rs` / `keybinds.rs`: input normalization to `Pt` (0..1) and routing by `UiState`. `kbd_switch.rs` is held-modifier (Super+Tab) switching.
- `arrange.rs`: home-grid reflow springs and arrange-mode drag. A long press on empty home background enters arrange. A long press on an icon opens `icon_menu.rs` instead: Open (or one row per window, by title, when the app has several), New window, Close, Remove.
- `library.rs`: the app library, a derived last home page with one tile per category folder.
- `launcher.rs` / `provenance.rs` / `resources.rs`: spawning apps (each in its own systemd scope), working out which launch a mapped window belongs to (xdg-activation token, then process ancestry), and per-app cgroup resource tiers. Window identity comes from the launch, not the client-reported `app_id`, so a `Terminal=true` entry isn't tagged `foot` and each PWA keeps its own id.
- `frame.rs`: per-frame shell advance, popups, animation gating, producing `FramePrep`.
- `render.rs`: the shared render path both backends use (clear, two-pass transformed app composite, Skia home/bar overlay, blur regions, rounded-rect texture shader).
- `skia_gl.rs`: Skia-on-Smithay-GLES context sharing.
- `winit_backend.rs` / `drm_backend.rs`: the two ways to present. `session.rs` is the Wayland display/socket plumbing they share, and `mirror.rs` is external-display mirroring on DRM.
- `debug_input.rs` + `ipc.rs`: synthetic-input socket and its CLI client.
- Protocol extras: `layer_shell.rs`, `popups.rs`, `idle_notify.rs`, `idle_inhibit.rs`, `gamma_control.rs`, `output_power.rs`, `background_effect.rs`, `content_type.rs`, `session_lock.rs`, `wlr_screencopy.rs`.
- Shell and device bits: `switcher.rs`, `bar_hint.rs`, `osd.rs`, `touch_viz.rs`, `app_history.rs`, `rotation.rs` + `sensor.rs` (iio-sensor-proxy), `blank.rs`, `sleep.rs` (blank before suspend), `screenshot.rs`, `capture.rs`, `catalog_watch.rs`, `uclamp.rs`, `frame_stats.rs`.
- Client pacing: `presentation.rs` (wp_presentation, with feedback answered from the DRM vblank, or after the swap on winit) and `pacing.rs` (wp_fifo + wp_commit_timing). Both are driven from `render.rs`'s frame-callback walk and hand the backend a `FrameSinks`. Its two halves are obligations: unanswered feedback hangs a client, and a signalled blocker still needs `pacing::clear_blockers` to apply the commit waiting on it.

### Render pipeline (`render.rs`)

1. Tick animations.
2. Compute scene.
3. Smithay pass 1: clear + fullscreen app (if not transitioning).
4. Skia: home screen (if `scene.show_home`).
5. Smithay pass 2: scaled app elements via `RescaleRenderElement` + `RelocateRenderElement` (if transitioning).
6. Skia: bar overlay.
7. Frame callbacks.
8. Submit.

### Skia/Smithay GLES sharing rules

- `context.reset(None)` before any Skia draw (invalidates Skia's GL state cache).
- `context.flush_and_submit()` after Skia draws (pixels must land before swap).
- Cache the Skia `Surface` keyed on `(fboid, width, height)`; recreate only on change.
- The DRM `report_partial` damage fast path drops Skia overlays not listed in its guard. Add new overlays there or they vanish over fullscreen apps.

### Smithay dependency

Pinned to upstream git (`github.com/Smithay/smithay.git`, rev `7ddcd17`) because crates.io lacks xkbcommon 0.9, which fixes wvkbd keymap loading. Protocol dispatch is one `smithay::delegate_dispatch2!(State)` in `handlers.rs`; upstream removed the per-protocol `delegate_*!` macros.

Do not remove the `use_system_lib` feature. It selects libwayland-server over the pure-Rust `wayland-backend`. Several smithay role handlers post a protocol error from a surface pre-commit hook after the role object is gone (the "destroy role, attach nil, commit" teardown every Qt/quickshell client does). The Rust backend delivers that on the dead object and kills the client, which hit layer surfaces (Smithay#1979, dms panel close) and lock surfaces (`Committed before the first ack_configure.`, dms unlock). libwayland drops the error.

libinput/DRM/GBM/session types are used via `smithay::reexports::*` to avoid version skew.

## Configuration

`config.example.toml` documents every option at its compiled-in default. Lookup order: `$SPRINGCHICK_CONFIG`, then `$XDG_CONFIG_HOME/springchick/config.toml`, then `/etc/springchick/config.toml`. Persisted state (dock, pages, frecency) is separate: `sc_shell_model::persist` writes `state.toml`.

Notable options:

- `dpi` (default 3): advertised via `wp_fractional_scale`. The FP5 panel is illegible at 1:1.
- `idle_blank_secs`, `card_radius`, `show_touches`, `prefer_no_csd`, `natural_scroll`.
- `rotation_settle_ms` (default 400): accelerometer debounce.
- `rotation_fade_ms` (default 130): half the dip-to-black that covers a turn.
- `vrr` (default `true`): asks the panel for variable refresh. DRM backend only, no-op unless the connector reports `vrr_capable`, startup-only.
- `uclamp_min` (default `"auto"`): scheduler `util_min` floor held on the render thread while drawing, derived from CPU topology. See `uclamp.rs`.

Env vars: `SPRINGCHICK_BACKEND`, `SPRINGCHICK_CONFIG`, `SPRINGCHICK_IPC_SOCK`, `SPRINGCHICK_DEBUG_SOCK` (legacy), `SPRINGCHICK_WINIT_SIZE` (`WxH`). `SPRINGCHICK_OUTPUT` is read only by `scripts/screenshot.sh` and `scripts/record.sh`.

## Deployment

`nix/module.nix` exposes `programs.springchick.enable` and adds the package to `sessionPackages`, so the greeter lists springchick as a wayland session (real logind seat, no seatd hack). `bin/springchick-session` (source: `nix/springchick-session`) is a shell wrapper that starts the compositor as the `springchick.service` user unit so `graphical-session.target` comes up; without systemd it falls back to exec'ing the binary with `SPRINGCHICK_BACKEND=drm`. Device runbook: `docs/RUNBOOK-device.md` (build on-device over `ssh dmsmobile`).

Screen capture: `ext-image-copy-capture-v1` (dmabuf fast path on DRM, shm readback otherwise) plus `zwlr_screencopy_v1` (`wlr_screencopy.rs`, shm only) for wlr-era clients. Shared buffer plumbing is in `capture.rs`; each backend has its own draw-and-read-back glue. `scripts/screenshot.sh` uses grim. `scripts/record.sh` uses wf-recorder over wlr-screencopy, because it is the only recorder that reaches the FP5's `h264_v4l2m2m` hardware encoder. wl-screenrec is VAAPI-only and falls back to software x264 there.

## Testing expectations

Pure logic gets a unit test: every state transition, layout computation, and gesture classification. Integration is smoke-level (compositor starts, accepts clients, doesn't crash) via the VM checks. Visual correctness and gesture feel are human-reviewed; there is no pixel-diff infra.

## Further reading

`CONTRIBUTING.md` covers the same ground at more length, plus Smithay/NixOS specifics, key patterns, common client warnings, and milestone status. `docs/RUNBOOK-device.md` covers on-device deployment.
