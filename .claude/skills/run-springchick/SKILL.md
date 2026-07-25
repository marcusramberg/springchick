---
name: run-springchick
description: Build, launch, screenshot and drive springchick (the phone-shell wayland compositor) locally. Use when asked to run, start, screenshot, or interactively test springchick / sc-compositor, or to confirm a change works in the real running compositor rather than only in tests.
---

springchick is a wayland compositor (phone shell). Locally it runs with the
**winit** backend as a nested window inside a host wayland session (niri/sway),
so it can be screenshotted with `grim` and driven headlessly. The driver script
`.claude/skills/run-springchick/driver.sh` wraps the whole loop: launch, map a
test client, send synthetic input over the debug socket, screenshot, and a
**PID-safe** teardown.

All paths below are relative to the repo root.

## Prerequisites

- A running host wayland session (niri here). `echo $XDG_RUNTIME_DIR` →
  `/run/user/<uid>`; the host display socket is `wayland-1` (override with
  `SPRINGCHICK_HOST_WL`).
- `foot` (wayland terminal) — the test client. Already on this box.
- `nix` — build and `grim` both go through the flake devshell. No apt needed.

## Build + run (agent path) — use the driver

```bash
# Warm the devshell once (first build is slow; keep the whole thing < timeout).
nix develop --command true

D=.claude/skills/run-springchick/driver.sh
$D up                      # build + launch nested compositor, wait for frame loop
$D client                  # map a foot client filled with a test pattern (prints PID)
$D send "settle 1000"      # drive input over the debug socket; prints "ok"
$D shot /tmp/sc.png        # grim the host output; then Read /tmp/sc.png
$D down                    # kill compositor + every client it launched, by PID
```

`up` prints the compositor PID, the `springchick-0` socket path, and the
`actual output size` line (e.g. `w=1901 h=2088` — niri clamps the window, so
input coords are this ACTUAL size, not the FP5 constants 1224x2700).

`send` accepts one debug-input line: `down X Y` / `move X Y` / `up` /
`tap X Y` / `swipe X1 Y1 X2 Y2 [MS]` / `settle [MS]`. Coords in the actual
output size, inclusive bounds. Reply is `ok` or `err <msg>`.

To launch a different client: `$D client 'CMD…'` (runs under `foot --hold sh -c`).

## Run (human path)

`nix develop --command cargo run -p sc-compositor` with
`WAYLAND_DISPLAY=wayland-1` opens the nested window; Ctrl-C to quit. Useless
headless (no screenshot, no scripted input) — use the driver instead.

## Test

```bash
nix develop --command cargo test         # unit tests; bare cargo can't (see Gotchas)
```

## Gotchas (battle scars)

- **Never `pkill foot`.** The user's own terminal is usually a foot window; a
  broad kill takes down their session. The driver only ever kills PIDs it
  recorded. If you kill by hand, kill by PID.
- **One instance at a time.** A leftover springchick keeps `springchick-0`, so
  the next instance auto-bumps to `springchick-1` and a new `foot` (which
  connects to `springchick-0`) silently talks to the STALE binary — you end up
  screenshotting the old build. `up` refuses if the socket exists; always `down`
  first and confirm exactly one `springchick-0` socket.
- **`down` is async.** `kill -9` propagates to the compositor a beat after its
  launcher dies; a `pgrep` immediately after can still see it briefly. The
  sockets being gone is the real "it's down" signal.
- **`settle` returns on idle, not on visually-finished.** It resolves when
  `needs_animation()` is false + no gesture + pointer up. A shot right after can
  catch a stale frame mid-transition; re-`settle` + re-`shot` if it looks off.
- **Swipes route through the real velocity path.** Start point matters — a swipe
  beginning on an app icon can classify as a tap and launch it. Enter the
  switcher only from an App (swipe up from an app), not from Home.
- **`grim` needs the HOST display**, not springchick's: the driver sets
  `WAYLAND_DISPLAY=wayland-1` for it. It captures the whole host output; the
  springchick window is one tile within it.
- **Build/test must run inside `nix develop`.** rust-toolchain pins stable via
  rust-overlay; bare `cargo` tries to download stable and fails in the sandbox.
- **dpi:** `[main].dpi` (default 3) makes apps render at output scale — a foot
  client should FILL the window with chunky 3x glyphs. If it renders as a small
  card in the top-left, the output-scale render path regressed (see
  `render.rs` `app_scale`).

## Troubleshooting

- `up` prints `refuse: …/springchick-0 exists` → a prior instance is alive.
  `$D down`, confirm `ls /run/user/$(id -u)/springchick-*` is empty, retry.
- `up` times out on `entering frame loop` → read `/tmp/sc-driver/compositor.log`;
  usually a first-build compile still running (re-run after `nix develop
  --command true`) or no host `WAYLAND_DISPLAY` (`export SPRINGCHICK_HOST_WL`).
- `send` errors with connection refused → compositor isn't up, or
  `SPRINGCHICK_DEBUG_SOCK` differs; the driver uses `/tmp/sc-driver/debug.sock`.
