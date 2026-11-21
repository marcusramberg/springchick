---
name: springchick-ipc
description: Drive or inspect a running springchick compositor over its control socket with `springchick ipc <verb>` - synthetic taps, swipes and keys, launching apps, built-in actions, config reload, and queries (layers, busy, home, layout, rotation, shortcuts, record). Use when asked to tap/swipe/press something in springchick, script a gesture in a VM check, find out why the frame loop won't idle, see what layer surfaces are composited, or poke a session on the phone over ssh. Starting a compositor is the run-springchick skill.
---

`springchick ipc <verb> [args]` sends one line to the compositor and prints the
one-line reply. Exit 0 on `ok…`, 1 on `err…` or no socket, 2 with no verb.
Parser and dispatch: `crates/sc-compositor/src/debug_input.rs` (the source of
truth when this file and the code disagree); client: `ipc.rs`.

## Reaching the socket

Default `$XDG_RUNTIME_DIR/springchick-ipc.sock`; `SPRINGCHICK_IPC_SOCK` overrides.

| Where | How |
|---|---|
| Nested (`driver.sh`) | `driver.sh send "tap X Y"`, or `SPRINGCHICK_IPC_SOCK=/tmp/sc-driver/debug.sock target/debug/springchick ipc …` |
| VM check (root in the test driver) | `machine.succeed("SPRINGCHICK_IPC_SOCK=/run/user/1000/springchick-ipc.sock springchick ipc …")` |
| Phone | `ssh dmsmobile springchick ipc …` (the login shell there is fish) |

## Rules that bite

- Coordinates are **physical output pixels** (the `out=WxH` that `layers`
  prints), not logical. Out of bounds answers `err range`.
- One command is in flight at a time, and the reply comes when it has
  *finished*: a 500 ms swipe blocks ~500 ms. Commands are popped one per
  rendered frame, so a hidden nested window (~1 fps) answers seconds late.
- After anything that animates, `settle` before a screenshot or an assertion.
- `key` press and release land frames apart, so a binding always sees a
  **long** press. Test a binding with `action NAME`, or raise `long_press_ms`
  in a temp config.
- Under a session lock the shell-gesture verbs (`down/move/up`, `swipe`,
  `launch`, `drag`) answer `ok locked` and do nothing. `tap`, `tswipe` and
  `key` still reach the lock client.
- Quick-switch: swipe **right** along the bottom edge for the older app,
  **left** for the more recent one.

## Input verbs

| Verb | Does |
|---|---|
| `tap X Y` (alias `touch`) | A real wl_touch contact: the client under it, else the shell |
| `swipe X1 Y1 X2 Y2 [MS=200]` | Shell gesture funnel only (home, switcher, quick-switch); clients don't see it |
| `tswipe X1 Y1 X2 Y2 [MS=200]` | A real finger drag: scrolls the client under it |
| `down X Y` / `move X Y` / `up` | A shell gesture by hand (long press, arrange drag) |
| `key NAME [HOLD_MS=0]` | Press and release an xkb keysym (`XF86PowerOff`, `Tab`, `a`). `err unknown-keysym` / `err unmapped-keysym` |
| `keydown NAME` / `keyup NAME` | Leave a key held, for chords: `keydown Super_L`, `key Tab`, `keyup Super_L` |
| `settle [TIMEOUT_MS=2000]` | Block until nothing animates. `err timeout: REASON` names what still is |
| `launch APP_ID [new]` | As an icon tap (raise, else launch); `new` forces another window. `err unknown-app` |
| `action NAME` | A built-in binding action: `home`, `close-app`, `toggle-display`, `toggle-fullscreen`, `search`, `switcher-next`, `switcher-prev`, `screenshot`, `volume-up`, `volume-down`, `volume-mute` |
| `orientation NAME` | Fake an accelerometer report, iio-sensor-proxy spelling (`normal`, `left-up`, `right-up`, `bottom-up`) |
| `drag APP_ID [X Y]` | Take over an icon drag from the search app |

## Queries and control

| Verb | Answers |
|---|---|
| `layers` | `ok out=WxH dpi=N usable=… regrow=… layers=N layer-popups=N`, then one line per layer surface and layer-rooted popup: `ns`, `layer`, `geo` (logical), `draw` (physical rect composited, slide included), `buf`, `anchor`, `excl`. Nothing is filtered. Run it first when the screen shows something no client admits to |
| `busy` | `ok idle`, or what keeps the frame loop running right now. Run it before restarting a compositor that draws with nothing moving |
| `home` | The home grid as drawn plus catalog resolution; `!` marks an id not in the catalog |
| `layout [N]` | `ok ACTIVE NAME\|NAME\|…`; `N` locks that xkb layout |
| `rotation [enable\|disable]` | `ok enabled\|disabled ROTATION`; sets whole-UI rotation until the next `reload` |
| `shortcuts [APP_ID]` | `ok ID ID …`, the control sheet's app tiles; an id toggles it |
| `reload` | Re-read `config.toml` (live options only, see AGENTS.md) and rescan the app catalog |
| `haptic [EVENT=button-pressed]` | A feedbackd event through the shell's haptics. `err no session bus` |
| `pill` | Blink the home pill once, even under a covering layer |
| `record start PATH [SCALE=1]` / `record stop` | DRM only. `ok WxH DEVICE`, then `ok frames=N dropped=M`. Refuses on a dark or turned screen |
| `quit` | End the session, as SIGTERM |

## Recipes

```bash
S="springchick ipc"

# Open an app, wait for the animation, go home
$S launch org.gnome.Maps && $S settle && $S action home && $S settle

# Scroll the app under the finger (843x1368 output)
$S tswipe 420 1000 420 300 300

# Super+Tab switcher
$S keydown Super_L; $S key Tab; $S keyup Super_L

# Why is it still drawing?
$S settle 3000 || $S busy
```
