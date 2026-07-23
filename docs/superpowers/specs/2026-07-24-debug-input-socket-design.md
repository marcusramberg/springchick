# Debug Input Socket — Design

**Date:** 2026-07-24
**Status:** Approved design, pre-implementation
**Scope:** `sc-compositor` only. Dev/test harness. Winit backend only.

## Problem

Springchick is a touch-driven mobile compositor. To iterate on it from a
headless/agent context we need to *see* and *drive* the running compositor
without a physical phone or a real touchscreen.

Seeing is already solved: the winit backend renders a nested window in the host
Wayland session, and `grim` (via `nix run nixpkgs#grim`) captures it.

Driving is the gap. Available host tooling covers keyboard (`wtype`) but not
pointer/touch: `/dev/uinput` is root-only (no `ydotool`), and `wlrctl`'s
virtual-pointer protocol is not implemented by the smithay stack. More
fundamentally, the shell's navigation gestures depend on **pointer velocity over
time**, which external discrete injectors cannot reliably produce.

## Goal

An in-process debug input channel that injects synthetic pointer/touch/keyboard
events straight into the existing input pipeline, exercising the *real*
`sc-input` gesture path — including velocity — and giving a driver a deterministic
`command → wait for reply → screenshot` loop.

## Non-Goals

- Not available on the DRM device backend (winit only, for now).
- Not compiled-out; it is inert unless an env var is set.
- No named shell-intent shortcuts (`home`, `switcher`) — those would bypass the
  real input path and test less than they appear to.
- **No keyboard command in v1.** The host already covers keys via `wtype`, and
  the compositor's real key path takes a raw `Keycode` (not an xkb name) plus the
  `KeyboardHandle<State>` local — a name→keycode resolver and handle plumbing not
  worth it for the pointer/touch/velocity goal. Deferred; can be added later by
  threading the handle into the drain and adding a lookup.
- Not part of CI. The integration/smoke test is run manually (or by an agent)
  against a live nested window.

## Injection Point

All shell gestures already funnel through a small set of functions:

- `input_common::on_press(&mut State)` — pointer/touch down
- `input_common::on_motion(&mut State, x: f32, y: f32)` — movement
- `input_common::on_release(&mut State)` — up
- `input_common::on_escape(&mut State) -> bool` — escape handling

**Critical ordering constraint:** `on_press` takes no coordinates — it reads
`state.last_pointer_pos` and *early-returns if it is `None`* (`input_common.rs:90-93`).
Therefore every synthetic press MUST be preceded by an `on_motion(x, y)` in the
same operation to seed the position. This applies to `down`, `tap`, and the first
step of `swipe`. Order is always: `on_motion(x,y)` first, then `on_press`.

Calling these with synthetic values drives the full UI state machine (Home,
AppOpening, Settling, Switcher, …) exactly as real winit input does. The debug
channel is a second producer into this same funnel.

Idle detection reuses the existing `UiState::needs_animation()`
(`ui_state.rs:108`), which reports whether springs are still settling.

## Architecture

New module: `crates/sc-compositor/src/debug_input.rs`.
Compiled always; **active only when `SPRINGCHICK_DEBUG_SOCK=<path>` is set**, and
only on the winit path.

Channel bounds: the command channel is an unbounded `mpsc::channel::<(DebugCmd,
SyncSender<Reply>)>()`. Each command carries its own `sync_channel::<Reply>(1)`
reply sender; the reader thread holds the `Receiver` and blocks on it after
sending. Reply sends from the main loop always ignore errors (a disconnected
client means the reader dropped its `Receiver`; a failed send is harmless).

Chosen approach: **std thread + `std::sync::mpsc`, drained once per pump tick.**
This matches the existing manual `pump_events` loop in `run_winit` (which is not
yet on full calloop dispatch — there is a standing TODO to migrate it) and adds
no new runtime. Rejected alternatives: `calloop::channel` (requires refactoring
the loop onto calloop dispatch first — off-goal), and async/tokio (a whole
runtime for a dev socket — overkill).

Two halves that meet at a `DebugCmd` enum + a channel:

### Reader thread (owns transport + parsing only)

- Unlinks any stale socket file, binds a `UnixListener` at the env path.
- Accepts one client at a time; parks in `accept()` when idle (no CPU spin).
- Reads newline-delimited commands. For each: `parse_line(&str) -> Result<DebugCmd, String>`.
- Sends `(DebugCmd, reply: SyncSender<Reply>)` to the main loop over `mpsc`,
  then **blocks** reading the reply and writes it back to the client socket.
- Knows nothing about compositor state. `parse_line` is pure and unit-testable.

### Main-loop drain (owns compositor dispatch)

Called once per iteration of the `run_winit` pump loop, after winit event
dispatch and before render:

- If an `ActiveGesture` **or** a `pending_settle` is in flight, advance it (see
  Swipe / Settle) and do **not** pop a new command this tick — preserves the
  one-in-flight invariant independent of reader-thread timing.
- Otherwise `try_recv()` the next `(DebugCmd, reply)` and dispatch:
  - `Down/Move/Up/Tap` — dispatch immediately via the injection-point
    functions, then send the reply.
  - `Swipe` — construct an `ActiveGesture`, stash the reply in it, dispatch the
    first point; reply is sent when the gesture completes.
  - `Settle` — if idle now, reply `ok`; else stash a pending deadline and check
    each tick.

`State` gains:
- `active_gesture: Option<ActiveGesture>`
- `pending_settle: Option<(reply: SyncSender<Reply>, deadline: Instant)>`

## Protocol

UTF-8, newline-terminated, space-separated tokens. One command in flight at a
time (the driver blocks on the reply, which serializes naturally).

Coordinates are in **logical output space** (`FP5_WIDTH`=1224 × `FP5_HEIGHT`=2700),
parsed as `f32`. Range is **inclusive** `[0,W] × [0,H]` → outside → `err range`.
Inclusive because edge gestures (bottom-bar drag, edge swipes) legitimately begin
or end exactly on the boundary (`y = H`).

| Command | Effect |
|---------|--------|
| `down X Y` | `on_motion(X,Y)` then `on_press` (see ordering constraint) |
| `move X Y` | `on_motion(X,Y)` |
| `up` | `on_release` |
| `tap X Y` | `on_motion(X,Y)`, `on_press`, `on_release` — all in one tick, zero motion; dispatched immediately then reply |
| `swipe X1 Y1 X2 Y2 [MS]` | timed interpolated motion, `MS` default 200 |
| `settle [TIMEOUT_MS]` | block until idle; `TIMEOUT_MS` default 2000 |

Replies:
- `ok\n` — command completed (for swipe: final motion + release dispatched).
- `err <msg>\n` — parse failure, out-of-range coord, unknown key, or settle timeout.

`ok` for swipe means *input dispatched*, not *animation settled*. To screenshot a
stable frame, the driver issues `settle` afterward.

### Swipe state machine

Swipe must not block the render loop — the loop has to keep drawing frames so
that velocity and springs evolve. So swipe is stateful across ticks:

`ActiveGesture { start: Instant, dur_ms: u32, from: (f32,f32), to: (f32,f32), started: bool, reply: SyncSender<Reply> }`

Each tick while present:
1. `elapsed = now - start`; `t = clamp(elapsed / dur_ms, 0.0, 1.0)`.
2. First tick (`!started`): `on_motion(from)` then `on_press` (ordering
   constraint); set `started`.
3. Interpolate `p = lerp(from, to, t)`; `on_motion(p)`.
4. If `t >= 1.0`: within this same tick, after step 3's `on_motion(to)`, call
   `on_release`; send `ok`; clear `active_gesture`. The final `on_motion(to)`
   must precede `on_release` so the release-time velocity is correct — the nav
   classifier keys off it.

Interpolation is driven by **wall-clock elapsed / MS**, not tick counting,
because the loop sleeps a flat 1ms and per-frame time varies. This routes the
swipe through real `on_motion` across real frames → genuine velocity → the
`sc-input` nav thresholds fire.

If the client disconnects mid-gesture, the drain issues `on_release` (avoid a
stuck-down pointer), drops the `ActiveGesture` (dropping its stashed reply sender;
any pending reply-send is ignored), and clears it. Same for a `pending_settle`:
dropping it drops its reply sender. The reader thread returns to `accept()`.

### Settle

Idle predicate:
`!state.ui.needs_animation() && state.active_gesture.is_none() && !state.pointer_down`

On `settle`: if idle now → `ok`. Else store `pending_settle`; each tick, if idle →
`ok` and clear; if `now > deadline` → `err timeout` and clear.

## Error Handling

Every failure path returns `err <msg>\n`; none may panic the loop.

- Unknown verb / wrong arity / non-numeric arg → `err parse ...`
- Coord out of range → `err range`
- Settle timeout → `err timeout`
- Client disconnect: drop active gesture (with `on_release`), reader re-`accept()`s.

## Lifecycle

- Startup (winit path, env set): unlink stale socket path, bind, spawn reader thread.
- Env unset: thread never spawned; compositor behaves exactly as today.
- Single client at a time.
- Reader thread is **detached**, not joined. It parks in a blocking `accept()`;
  there is no separate shutdown signal. On compositor exit the process teardown
  reaps the thread and the socket fd. `run_winit` removes the socket file on its
  normal exit path (best-effort `unlink`); a stale file left by a crash is
  unlinked on next startup before bind.

## Testing

**Unit (pure, no compositor):**
- `parse_line` — table test every verb, bad arity, non-numeric args, unknown verb.
- Coord range check (inclusive bounds; `W`/`H` accepted, `W+1`/`-1` rejected).
- Swipe interpolation `lerp` — endpoints (t=0, t=1) and midpoint (t=0.5).
- Idle predicate truth table over `needs_animation` × gesture × pointer_down.

**Integration (manual / agent-driven smoke test, not CI):**
1. `SPRINGCHICK_DEBUG_SOCK=/tmp/sc.sock cargo run -p sc-compositor`
2. Driver connects, `settle`, `grim` → capture Home.
3. `swipe` an app-switch gesture, `settle`, `grim` → verify Switcher/next app.
4. `tap` an icon, `settle`, `grim` → verify app opened.

Documented in this spec as the acceptance procedure.

## Files Touched

- **New:** `crates/sc-compositor/src/debug_input.rs` (socket, parser, `DebugCmd`,
  `ActiveGesture`, drain helper).
- **Modified:** `crates/sc-compositor/src/main.rs`
  - `mod debug_input;`
  - `run_winit`: if env set, unlink+bind+spawn reader thread, hold the `Receiver`.
  - Per-tick: call the drain helper before render.
  - `State`: add `active_gesture`, `pending_settle`.
- No changes to other crates.
