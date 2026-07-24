# Keybindings and Wayland keyboard input

Date: 2026-07-24
Status: approved for planning

## Problem

springchick has no usable keyboard path. The winit backend forwards key events to
`KeyboardHandle::input` (`crates/sc-compositor/src/main.rs:747`), but nothing ever calls
`set_focus`, so those events go nowhere. The DRM backend has no `InputEvent::Keyboard` arm at
all — on the Fairphone 5 the volume and power buttons do nothing and no client can ever receive
a key.

Two things are needed: the compositor must act on the FP5's physical buttons (short and long
press, running a shell command or an internal action), and unbound keys must reach the focused
Wayland client.

## Goals

- Bindings defined in a TOML file, matched on xkb keysym plus optional modifiers, so hardware
  buttons and ordinary chords share one syntax.
- Distinguish short from long press, with the long action firing while the key is still held.
- Bindings run a shell command or a small set of internal compositor actions.
- Unbound keys forward to the focused client; keyboard focus tracks the UI state machine.
- Screen blanking (DPMS) on the DRM backend, so the power button has something to do.

## Non-goals

- Live config reload. Startup-only; the relogin loop on the device is short.
- Key repeat handling for bindings, chorded sequences, or per-application bindings.
- Lock screen and `allow-when-locked` semantics — springchick has no lock screen.
- Blanking on the winit backend (`toggle-display` is a logged no-op there).

## Configuration

Path: `$XDG_CONFIG_HOME/springchick/keybindings.toml`, falling back to
`~/.config/springchick/keybindings.toml`. `SPRINGCHICK_KEYBINDS=<path>` overrides it for tests.
A missing file means the compiled-in defaults are used; nothing is written to disk implicitly.

```toml
long_press_ms = 500          # optional, global, default 500

[[binding]]
key = "XF86AudioRaiseVolume" # xkb keysym name
press = "short"              # "short" | "long"
command = "wpctl set-volume @DEFAULT_SINK@ 5%+"

[[binding]]
key = "XF86AudioRaiseVolume"
press = "long"
action = "close-app"         # internal action, mutually exclusive with `command`

[[binding]]
key = "Return"
mods = ["Super"]             # optional; exact match on Ctrl/Alt/Shift/Super
press = "short"
command = "foot"
```

`command` is passed to `sh -c`, so pipes, quoting and `&&` behave as written. Exactly one of
`command` or `action` must be present.

Internal actions:

| action | effect |
|---|---|
| `close-app` | closes the front toplevel (existing `Effect::CloseToplevel`) |
| `home` | returns to the home screen |
| `toggle-display` | blanks/unblanks the panel (DRM backend only) |

Modifier matching is exact over Ctrl/Alt/Shift/Super; lock modifiers (Caps, Num) are ignored so
a stuck Caps Lock cannot silently disable every binding.

### Error handling

Config errors are lenient by design: an unresolvable keysym name, an invalid `press` value, a
binding with both or neither of `command`/`action`, or a malformed entry logs a warning and is
skipped. A duplicate `(key, mods, press)` triple logs a warning and the last entry wins. A
config typo must never prevent the compositor from starting — on a phone, a compositor that
refuses to boot is a recovery session, while a skipped binding is a button that does nothing.

If the file itself fails to parse, the compiled-in defaults are used and the error is logged.

### Defaults

| key | short | long |
|---|---|---|
| `XF86AudioRaiseVolume` | `wpctl set-volume @DEFAULT_SINK@ 5%+` | action `close-app` |
| `XF86AudioLowerVolume` | `wpctl set-volume @DEFAULT_SINK@ 5%-` | `pkill -SIGRTMIN -f wvkbd-mobintl` |
| `XF86PowerOff` | action `toggle-display` | `systemctl poweroff` |

These mirror the user's niri bindings, with the two niri-IPC ones (`niri msg action
close-window`, `display.sh`) replaced by the equivalent internal actions.

## Press semantics

```
press ───┬─────────────── 500ms ───┬──────────── release
         │                         │
    short armed              long FIRES here
                             short suppressed
```

- Press of a key that has any binding is swallowed; press of an unbound key forwards.
- Release under the threshold fires the short binding, if one exists.
- Crossing the threshold while held fires the long binding immediately and marks the press
  consumed, so the following release fires nothing.
- A key with only a long binding still swallows its short press. Binding a key for an app to
  see requires an explicit short binding.
- A repeated press for an already-held key is ignored.
- While the display is blanked, the first bound key press wakes the panel and fires nothing.

## Architecture

### `sc-keys` (new crate)

Pure logic, no I/O, mirroring how `sc-input` holds gesture logic.

- `config.rs` — `Binding { key: String, mods: ModMask, press: PressKind, action: Action }` and
  the TOML parse. Key names stay strings here; the compositor resolves them to keysyms.
- `state.rs` — `KeyBindings` (resolved: keyed by `(keysym, mods)`) and `PressTracker`, the
  short/long state machine over an injected clock.

`PressTracker` interface:

| call | returns |
|---|---|
| `on_press(keysym, mods, now)` | `Swallow` if bound, else `Forward` |
| `on_release(keysym, now)` | `Fire(action)` / `Swallow` / `Forward` |
| `poll(now)` | `Some(action)` when a held key crosses the threshold |
| `next_deadline()` | `Option<Instant>` for the backend's timer |

Resolving keysym names needs xkb, which lives on the compositor side, so `sc-keys` stays
dependency-light and unit-testable with a virtual clock.

### `crates/sc-compositor/src/keybinds.rs` (new module)

Owns the resolved `KeyBindings` and the `PressTracker`, resolves keysym names via smithay's
xkb re-export at load time, spawns `sh -c` commands (reusing the spawn-and-log pattern in
`launcher.rs`) and reaps finished children so detached processes do not accumulate as zombies.
Internal actions are translated into `UiEvent`s or backend calls.

### Shared keyboard handle

`add_keyboard` moves out of `run_winit` into `State::new`, so `State` owns the
`KeyboardHandle` and both backends run the same path:

```
libinput / winit key event
  → keyboard.input(..) filter closure
      → keybinds::on_key(keysym, mods, state)
          → bound?  Intercept  (fire now for a short release or an internal action)
          → else    Forward → focused surface
```

The DRM backend gains the `InputEvent::Keyboard` arm it currently lacks.

### Long-press timing

The winit loop polls `PressTracker::poll` once per frame. The DRM backend cannot poll per
frame, because page-flips stop when nothing animates — a frame-polled long press would never
fire on an idle screen. Its calloop already wakes every 2ms to dispatch wayland clients
(`event_loop.run(Some(Duration::from_millis(2)), ..)` in `drm_backend.rs`), so the poll goes in
that callback. `next_deadline()` exists for a future idle-timeout loop; no extra timer source
is needed today.

### Keyboard focus

`ui_state.rs` gains a pure `desired_focus(&UiState) -> Option<ToplevelId>` function, and
`State::sync_keyboard_focus()` maps that id to a surface and calls `KeyboardHandle::set_focus`
when it differs from the current focus. Both backends call it once per frame. Keeping the
policy as a pure function of `UiState` (rather than a `FocusToplevel(WlSurface)` effect) leaves
`ui_state.rs` free of wayland types and makes the table below directly unit-testable:

| UI state | focus |
|---|---|
| Home | `None` |
| Zoom open / close (mid-animation) | `None` |
| App open | the front app's toplevel surface |
| Switcher | `None` |
| Quick-switch | follows the new front app |

Deriving focus from the state machine keeps it correct as gestures move between home and apps,
rather than letting a mapped-but-hidden app eat keys.

### Display blanking

`toggle-display` disables the CRTC on the DRM backend, parking the render loop; re-enabling
forces a full redraw. Blanked state suppresses page-flips so an idle blanked phone does no
rendering work. On winit it logs and does nothing.

## Testing

1. `sc-keys` unit tests, written first, over a virtual clock: short fires on release under the
   threshold; long fires exactly at the threshold with no release; short is suppressed once
   long has fired; release after a long press is silent; an unbound key forwards; a key with
   only a long binding still swallows its short press; a repeat press of a held key is ignored;
   `next_deadline` is correct with two keys held.
2. `sc-keys` config tests: modifier matching, unknown keysym skipped, duplicate override,
   `command`/`action` exclusivity, missing file yields defaults, malformed file yields defaults.
3. A compositor unit test resolving keysym names through xkb, which catches spellings like
   `XF86AudioRaiseVolume` failing to resolve.
4. Headless end-to-end through the existing debug socket: extend `parse_line`
   (`crates/sc-compositor/src/debug_input.rs:30`) with `key <name> [hold_ms]`, bind a test
   command that touches a temp file, and assert the whole path — event, tracker, timer, spawn.
   The same harness asserts that keys forward to a client only in the app-open state.
5. On-device manual pass: volume short presses change volume, long presses close the front app
   and toggle the on-screen keyboard, power short toggles the panel. Power long is first bound
   to `logger` to confirm timing before it is trusted with `poweroff`.

## Risks

- **`poweroff` in the defaults.** A 500ms slip on the power button powers the phone off during
  testing. Mitigated by testing the timing with a harmless command first.
- **Swallowing bound keys.** Volume keys never reach clients, so an app with its own volume UI
  will not see them. Accepted; a short binding can always be removed.
- **Blanking and DRM master.** Re-enabling the CRTC has to restore modeset state correctly, or
  the panel comes back black. Covered by the on-device pass, not by automated tests.
