---
name: run-springchick
description: Build, launch, screenshot and drive springchick (the phone-shell wayland compositor) locally or headless. Use when asked to run, start, screenshot, or interactively test springchick / sc-compositor, to boot it in a VM / on a clean machine / in CI, or to confirm a change works in the real running compositor rather than only in tests.
---

springchick is a wayland compositor (phone shell). There are **two** ways to
drive it — pick by whether the machine has a host wayland session:

- **Headless / clean machine / CI (no display): the nix VM test.** Boots
  springchick on its real **DRM** backend inside a NixOS QEMU VM (virtio-gpu +
  llvmpipe software GL), autologins the shipped session, launches a client, and
  screenshots the framebuffer. This is the ONLY path that works with no host
  wayland session. Harness = `nix/vm-test.nix` (committed), driven either as a
  one-shot `nix build` check or live via the interactive driver. **Use this
  path in this container** (and see [[nix-vm-tests]]).
- **Local nested (host wayland session, e.g. niri/sway): `driver.sh`.** Runs
  the **winit** backend as a nested window, screenshots with `grim`, drives
  synthetic input over the debug socket. Faster iteration, but needs a display.

All paths below are relative to the repo root.

## Headless (clean machine / CI) — the nix VM test

Boots springchick end-to-end with no display. **Build for the host arch** —
cross-building the other arch runs the release tree under qemu-user emulation,
which SIGSEGVs rustc. This box is `aarch64`; substitute your `nix eval --impure
--expr builtins.currentSystem` output.

One-shot scripted smoke (boot → launch foot → assert app_id resolves → screenshots):

```bash
nix build .#checks.aarch64-linux.vm-boot -L --no-link
# Screenshots land in the result: springchick-boot.png, springchick-foot.png
```

Drive it **live** (poke the running VM, no springchick recompile between probes):

```bash
nix build .#checks.aarch64-linux.vm-boot.driverInteractive \
  --out-link /tmp/sc-driver-link
# Pipe python to the driver; screenshots land in $CWD as <name>.png.
cd /tmp && /tmp/sc-driver-link/bin/nixos-test-driver <<'PY'
start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_until_succeeds("systemctl --user -M tester@.host is-active springchick.service", timeout=90)
machine.wait_until_succeeds("ls /run/user/1000/springchick-*.lock", timeout=30)
machine.screenshot("sc-home")          # → /tmp/sc-home.png ; then Read it
machine.shutdown()
PY
```

`machine` exposes `succeed`/`fail`/`wait_until_succeeds`/`screenshot`/
`send_key`/`shutdown`. The guest has `foot` installed (its `.desktop` files put
Foot / Foot Server / Foot Client on the home grid). Launch a client from inside
the session and screenshot the result:

```python
sock = machine.succeed("basename $(ls /run/user/1000/springchick-*.lock) .lock").strip()
machine.succeed(f"systemd-run --user -M tester@.host --collect --setenv=WAYLAND_DISPLAY={sock} $(command -v foot) -e sleep 30")
machine.wait_until_succeeds("journalctl -b _SYSTEMD_USER_UNIT=springchick.service | grep -F 'app_id=foot'", timeout=30)
machine.screenshot("sc-foot")
```

Edit the scripted assertions in `nix/vm-test.nix`. The `src` filter in
`nix/package.nix` means editing `nix/`, `tests/`, or `docs/` does NOT rebuild
the compositor — only `crates/`, `Cargo.toml`, `Cargo.lock` do.

### Driving gestures — `springchick ipc`

A running compositor always listens on `$XDG_RUNTIME_DIR/springchick-ipc.sock`
(override with `SPRINGCHICK_IPC_SOCK`). The shipped `springchick ipc <verb>`
client sends one line and prints the reply (exit non-zero on error). Verbs are
the debug-input gestures: `tap X Y`, `swipe X1 Y1 X2 Y2 [MS]`, `key NAME [MS]`,
`keydown NAME` / `keyup NAME`, `down/move/up`, `settle [MS]`, plus the control
verb `reload` (re-read
`config.toml`). Used by `nix/vm-switcher-test.nix`; also works
on-device. From the test driver (root reaching the tester's socket):

```python
IPC = "/run/user/1000/springchick-ipc.sock"
machine.succeed(f"SPRINGCHICK_IPC_SOCK={IPC} springchick ipc swipe 640 788 1080 788 500")
```

Quick-switch handedness follows the carousel (most-recent on the right): swipe
**right** → older app, swipe **left** → more-recent.

## Prerequisites (local nested path only)

The VM path needs only `nix` + KVM (`/dev/kvm`). For `driver.sh`:

- A running host wayland session (niri here). `echo $XDG_RUNTIME_DIR` →
  `/run/user/<uid>`; the host display socket is `wayland-1` (override with
  `SPRINGCHICK_HOST_WL`).
- `foot` (wayland terminal) — the test client. Already on this box.
- `nix` — build and `grim` both go through the flake devshell. No apt needed.

## Local nested (host wayland session) — driver.sh

Needs a host wayland display (won't work in a display-less container — use the
VM path above there). The driver script
`.claude/skills/run-springchick/driver.sh` wraps the whole loop: launch, map a
test client, send synthetic input over the debug socket, screenshot, and a
**PID-safe** teardown.

```bash
# Warm the devshell once (first build is slow; keep the whole thing < timeout).
nix develop --command true

D=.claude/skills/run-springchick/driver.sh
$D build                   # compile ONLY — do this first (see warning below)
$D up                      # launch nested compositor, wait for frame loop
$D client                  # map a foot client filled with a test pattern (prints PID)
$D send "settle 1000"      # drive input over the debug socket; prints "ok"
$D wake                    # clear an idle-faded host screen (shot does this too)
$D shot /tmp/sc.png        # grim springchick's output; then Read /tmp/sc.png
$D down                    # kill compositor + every client it launched, by PID
```

**`up` NEVER returns — it holds the terminal running the compositor.** It is the
foreground compositor process, not a launch-and-exit command. Running it as a
normal foreground Bash call ALWAYS hits the tool timeout — that timeout is NOT a
failure, the compositor is up and fine. Do not wait on it.

- **ALWAYS run `$D up` with `run_in_background: true`.** It stays running there;
  you get the terminal back immediately. Never run it foreground "just to see if
  it comes up".
- **Then poll for readiness** (it's ready in a few seconds on an already-built
  tree): `ls /run/user/$(id -u)/springchick-0` exists AND
  `grep "entering frame loop" /tmp/sc-driver/compositor.log` hits. Once both are
  true, go straight to `client`/`send`/`shot`. The PID / socket / `actual output
  size` line are in `/tmp/sc-driver/compositor.log`.
- **Build first, separately.** A cold compile alone can exceed the ~5-min
  Bash ceiling, and `up` compiles if the tree isn't built. Run `$D build` first
  (background or long timeout); `nix develop --command true` only warms the
  devshell, NOT the crate build. If the log ends mid-compile, it's still
  building — wait and re-check.

`up` prints the compositor PID, the `springchick-0` socket path, and the
`actual output size` line (e.g. `w=1901 h=2088` — niri clamps the window, so
input coords are this ACTUAL size, not the FP5 constants 1224x2700).

`send` accepts one debug-input line: `down X Y` / `move X Y` / `up` /
`tap X Y` / `swipe X1 Y1 X2 Y2 [MS]` / `settle [MS]`. Coords in the actual
output size, inclusive bounds. Reply is `ok` or `err <msg>`.

**Chords** need `keydown` / `keyup`, which return immediately and leave the key
held — `key NAME` auto-releases and takes the one-in-flight slot, so it cannot
hold a modifier across the next verb. Super+Tab is therefore:

```bash
$D send "keydown Super_L"; $D send "key Tab"   # deck opens, focus steps
$D send "keyup Super_L"                        # opens the focused card
```

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

- **VM: build the host arch, never cross-build.** `nix build
  .#checks.x86_64-linux.vm-boot` on this aarch64 box runs the whole release
  tree under qemu-user emulation and SIGSEGVs rustc (`qemu: uncaught target
  signal 11`). Always match `builtins.currentSystem`.
- **VM: the interactive driver writes screenshots to `$CWD`**, not the store —
  `machine.screenshot("foo")` → `./foo.png`. `cd` somewhere writable first.
- **VM: don't grep the journal for bare `panic`.** The kernel cmdline
  (`panic=1`) and virtio-gpu `drm panic` planes both match; assert
  `panicked at|SIGSEGV|SIGABRT|stack backtrace|segfault` instead.
- **VM: greetd needs `default_session`**, not just `initial_session`, or it
  exits with `default_session contains no command` and nothing autologins.
- **VM: DRM `Permission denied` errors at `machine.shutdown()` are benign** —
  logind revokes DRM master as the seat tears down, so the compositor's final
  page-flip/DPMS commit fails. They fire *after* any screenshot and don't match
  the crash-signature grep. Boot-time rendering is unaffected.
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
  `WAYLAND_DISPLAY=wayland-1` for it. `shot` scopes the capture to the output
  springchick is on (resolved via `niri msg windows` → `workspaces`), so the
  window is one tile within that output, not within the whole multi-head desktop.
- **A solid-black screenshot means the host screen idled out, not a render
  regression.** dms covers the output with a black `dms:fade-to-dpms` **Overlay**
  layer before DPMS; grim captures that overlay. Detect it with
  `niri msg -j layers | grep fade-to-dpms` (needs `NIRI_SOCKET=$(ls
  /run/user/$(id -u)/niri.*.sock | head -1)` — it is NOT in the agent's env).
  `niri msg action power-on-monitors` does **not** clear it: the overlay is
  dms's, and dms only retracts it on real seat activity. A synthetic **pointer
  nudge** does — `nix run nixpkgs#wlrctl -- pointer move 1 0` against the host
  display. `$D wake` wraps all of this and `$D shot` calls it first, with a
  capture-size backstop for a blank the layer check misses.
  Note niri's own DPMS-off does **not** break grim — screencopy still returns
  real content — so the dms overlay is the only thing that actually blacks a shot.
- **Never wake with a synthetic KEYSTROKE.** `wake` used to send `wtype -k
  Shift_L`, which looks harmless and is not: wtype uploads its own keymap, the
  host forwards the raw **keycode**, and springchick resolves it through *its*
  keymap. wtype's scratch keycode lands on 9 = `Escape`, which springchick used
  to bind to `home` (it is `Super`+`h` now) — so every `$D shot` silently
  returned the compositor to Home, and a
  screenshot taken after a gesture showed the result of the screenshot, not the
  gesture. Symptom: the journal shows `keybinding fired action="home"` at the
  moment of the shot. This is why `poke_seat` nudges the pointer instead (inert
  with no button held: springchick just records the position).
- **Build/test must run inside `nix develop`.** rust-toolchain pins stable via
  rust-overlay; bare `cargo` tries to download stable and fails in the sandbox.
- **dpi:** `[main].dpi` (default 3) makes apps render at output scale — a foot
  client should FILL the window with chunky 3x glyphs. If it renders as a small
  card in the top-left, the output-scale render path regressed (see
  `render.rs` `app_scale`).

## Troubleshooting

- **VM:** `springchick.service` never reaches active → the DRM/GL stack didn't
  come up. Read the guest journal: `nix log <drv>` from the build output, or
  live via the interactive driver
  `machine.succeed("journalctl -b -u springchick.service")`. Common cause is
  greetd not autologging in (missing `default_session`).
- **VM:** rustc SIGSEGV / `qemu: uncaught target signal 11` during the build →
  you're cross-building. Use the host arch (`aarch64-linux` here).
- **VM:** `app_id=foot` assertion times out → the catalog didn't see
  `foot.desktop` at scan time (springchick scans `XDG_DATA_DIRS` at startup);
  confirm `pkgs.foot` is in the node's `environment.systemPackages`.
- `up` prints `refuse: …/springchick-0 exists` → a prior instance is alive.
  `$D down`, confirm `ls /run/user/$(id -u)/springchick-*` is empty, retry.
- `up` times out on `entering frame loop` → read `/tmp/sc-driver/compositor.log`;
  usually a first-build compile still running (re-run after `nix develop
  --command true`) or no host `WAYLAND_DISPLAY` (`export SPRINGCHICK_HOST_WL`).
- `send` errors with connection refused → compositor isn't up, or
  `SPRINGCHICK_DEBUG_SOCK` differs; the driver uses `/tmp/sc-driver/debug.sock`.
