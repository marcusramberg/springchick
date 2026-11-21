# AGENTS.md

springchick is an iOS-Springboard-style Wayland compositor for mainline Linux
phones. It is a fused compositor: the shell UI is drawn by Skia into the same
EGL/GLES context Smithay uses to composite clients. The bar, OSK and search
are separate clients (`sc-bar`, `sc-osk`, `sc-search`).

## Build / test

Everything runs inside the nix devshell; a bare `cargo` fails.

```bash
nix develop --command bash -c 'cargo build -p sc-compositor'
nix develop --command bash -c 'cargo test --workspace'
nix develop --command bash -c 'cargo test -p sc-compositor ui_state::'  # single module/test
nix develop --command bash -c 'cargo check --tests'              # fast compile check
```

A cold skia build is long, so build in the background or with a large timeout.

VM checks exercise the real DRM path headless. They are listed in `flake.nix`:

```bash
nix build .#checks.aarch64-linux.vm-boot -L
```

Build the check matching `builtins.currentSystem`: cross-building runs rustc under qemu-user, which SIGSEGVs. `nix/package.nix` filters `src`, so edits under `nix/`, `tests/`, `docs/` don't rebuild the compositor.

## Running / driving it

- To start one (nested winit or VM), use the `run-springchick` skill.
- To drive or inspect a running one, use `springchick ipc <verb>` (the `springchick-ipc` skill; verbs are in `ipc.rs`). `ipc layers` and `ipc busy` are the first things to run when the screen shows something unexplained or the frame loop won't idle.
- Never kill by name (`pkill foot`, `pkill niri`): the user's terminal is a foot window and their desktop is niri. Kill by recorded PID only.

## Architecture

Push logic into pure crates (no GPU, no Wayland deps) and unit-test it there. `sc-compositor` does integration and wiring only. The crates are `crates/sc-*`; the names say what they hold.

In `sc-compositor`:

- `state.rs` holds the central `State`; behaviour lives in sibling `impl State` modules.
- `ui_state.rs` is a pure state machine: `transition(&mut state, event) -> Effect`. Side effects are returned, never performed there.
- `scene.rs` is pure: `compute_scene(state, output_size) -> Scene`.
- `render.rs` is the one render path both backends (`winit_backend.rs`, `drm_backend.rs`) share.

Every crate is `#![forbid(unsafe_code)]` unless it already has a reason not to be.

## Invariants the code won't tell you

- Skia shares Smithay's GL context: `context.reset(None)` before any Skia draw, `context.flush_and_submit()` after. Keep every Skia draw going through `ensure_surface`, which is what turns the DRM partial-damage fast path off for that frame. A new non-Skia pass over a fullscreen app goes in `flip_damage`'s `only_app`.
- `FrameSinks` (`presentation.rs`, `pacing.rs`) are obligations: unanswered presentation feedback hangs a client, and a signalled blocker still needs `pacing::clear_blockers`.
- Do not remove smithay's `use_system_lib` feature. With the pure-Rust wayland backend, the "destroy role, attach nil, commit" teardown every Qt client does gets the client killed.
- Smithay is pinned to a git rev (see `Cargo.toml`). Use libinput/DRM/GBM/session types via `smithay::reexports::*` to avoid version skew.
- Window identity comes from the launch (`provenance.rs`), not the client-reported `app_id`.
- GTK4 binds `xdg_dialog_v1` to every window; only `set_parent` or the modal hint means dialog (`toplevel.rs`, `is_dialog`).

## Configuration

`config.example.toml` documents every option at its default; keep it in step when adding one. Parsing is `sc-config` and is lenient: a bad entry is dropped and the rest applies. Draw with `sc_config::Theme` colours and `sc_config::style` names, not new literals. Persisted shell state (`state.toml`) is separate, in `sc_shell_model::persist`.

## Testing expectations

Pure logic gets a unit test: every state transition, layout computation, and gesture classification. Integration is smoke-level via the VM checks. Visual correctness and gesture feel are human-reviewed; there is no pixel-diff infra.

## Further reading

`CONTRIBUTING.md` goes into more depth (key patterns, Smithay/NixOS specifics, common client warnings). `docs/RUNBOOK-device.md` covers on-device deployment.
