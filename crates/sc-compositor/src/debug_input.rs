//! Debug input socket — the line protocol behind the control socket.
//!
//! The compositor (both winit and DRM backends) always listens on
//! [`crate::ipc::socket_path`]; `springchick ipc <verb>` is the client. A reader
//! thread parses newline commands off the unix socket into [`DebugCmd`] values
//! and hands them to the render loop, which injects them through the existing
//! `input_common` funnel. See
//! `docs/superpowers/specs/2026-07-24-debug-input-socket-design.md`.
//!
//! The gesture verbs answer `ok locked` and do nothing while an
//! `ext-session-lock` client holds the session (see [`crate::session_lock`]) —
//! the harness gets exactly as far as a finger would. `key` still goes through:
//! that is how a scripted password reaches the lock client.

/// A parsed command from the debug socket.
#[derive(Clone, Debug, PartialEq)]
pub enum DebugCmd {
    Down(f32, f32),
    Move(f32, f32),
    Up,
    Tap(f32, f32),
    Swipe {
        from: (f32, f32),
        to: (f32, f32),
        dur_ms: u32,
    },
    Settle {
        timeout_ms: u32,
    },
    /// Press a key by xkb keysym name, holding it `hold_ms` before release.
    Key {
        name: String,
        hold_ms: u32,
    },
    /// Press (`down = true`) or release a key by xkb keysym name and return
    /// immediately, leaving it held across later commands.
    ///
    /// This is how a chord is scripted: `keydown Super_L`, `key Tab`,
    /// `keyup Super_L`. Unlike [`DebugCmd::Key`] it takes no in-flight slot, so
    /// the held modifier is still down while the next verb runs — which is the
    /// whole point for held-modifier bindings like Super+Tab.
    KeyHold {
        name: String,
        down: bool,
    },
    /// A real touch-down+up tap at `(x, y)` through the surface-routing path
    /// (`touch::down`/`up`), for exercising layer-surface/app input.
    Touch(f32, f32),
    /// Pretend the device was turned. Drives the same path the accelerometer
    /// will, so rotation policy is testable with no sensor.
    Orientation(crate::rotation::DeviceOrientation),
    /// Re-read `config.toml` and apply the live-changeable settings.
    Reload,
    /// Shut the compositor down, as SIGTERM does. This is how a shell's logout
    /// action ends the session — springchick has no other exit path.
    Quit,
    /// Report every layer surface and layer-rooted popup being composited, with
    /// geometry, buffer and map state. A query, not input — see
    /// [`State::layers_dump`].
    Layers,
    /// Open a catalog app by id, exactly as an icon tap would: raise its most
    /// recent window if it has one, else launch it. `new_window` forces a fresh
    /// instance instead. This is how the search app opens things, so its
    /// launches get the same attribution and de-duplication as icon taps.
    Launch {
        app_id: String,
        new_window: bool,
    },
}

/// Parse one command line. `w`/`h` are the logical output bounds used for the
/// inclusive coordinate range check `[0,w] x [0,h]`.
///
/// Returns a short error tag on failure (`parse ...`, `range`).
pub fn parse_line(line: &str, w: f32, h: f32) -> Result<DebugCmd, String> {
    let mut tok = line.split_whitespace();
    let verb = tok.next().ok_or_else(|| "parse: empty".to_string())?;

    // Parse the next token as f32.
    fn num<'a>(t: &mut impl Iterator<Item = &'a str>) -> Result<f32, String> {
        t.next()
            .ok_or_else(|| "parse: missing arg".to_string())?
            .parse::<f32>()
            .map_err(|_| "parse: not a number".to_string())
    }
    // Parse an optional trailing u32, defaulting if absent.
    fn opt_u32<'a>(t: &mut impl Iterator<Item = &'a str>, default: u32) -> Result<u32, String> {
        match t.next() {
            None => Ok(default),
            Some(s) => s
                .parse::<u32>()
                .map_err(|_| "parse: not an integer".to_string()),
        }
    }
    // Inclusive range check.
    let in_bounds = |x: f32, y: f32| x >= 0.0 && x <= w && y >= 0.0 && y <= h;

    // Ensure no trailing tokens remain.
    fn done<'a>(mut t: impl Iterator<Item = &'a str>) -> Result<(), String> {
        match t.next() {
            None => Ok(()),
            Some(_) => Err("parse: trailing tokens".to_string()),
        }
    }

    let cmd = match verb {
        "down" | "move" | "tap" => {
            let x = num(&mut tok)?;
            let y = num(&mut tok)?;
            done(tok)?;
            if !in_bounds(x, y) {
                return Err("range".to_string());
            }
            match verb {
                "down" => DebugCmd::Down(x, y),
                "move" => DebugCmd::Move(x, y),
                _ => DebugCmd::Tap(x, y),
            }
        }
        "touch" => {
            let x = num(&mut tok)?;
            let y = num(&mut tok)?;
            done(tok)?;
            if !in_bounds(x, y) {
                return Err("range".to_string());
            }
            DebugCmd::Touch(x, y)
        }
        "orientation" => {
            let name = tok
                .next()
                .ok_or_else(|| "parse: missing orientation".to_string())?;
            done(tok)?;
            // Same spelling as iio-sensor-proxy's AccelerometerOrientation, so
            // a test drives exactly what the sensor would report.
            let o = crate::rotation::DeviceOrientation::from_sensor(name);
            if o == crate::rotation::DeviceOrientation::Undefined && name != "undefined" {
                return Err(format!("parse: unknown orientation {name}"));
            }
            DebugCmd::Orientation(o)
        }
        "up" => {
            done(tok)?;
            DebugCmd::Up
        }
        "reload" => {
            done(tok)?;
            DebugCmd::Reload
        }
        "layers" => {
            done(tok)?;
            DebugCmd::Layers
        }
        "quit" => {
            done(tok)?;
            DebugCmd::Quit
        }
        "swipe" => {
            let x1 = num(&mut tok)?;
            let y1 = num(&mut tok)?;
            let x2 = num(&mut tok)?;
            let y2 = num(&mut tok)?;
            let dur_ms = opt_u32(&mut tok, 200)?;
            done(tok)?;
            if !in_bounds(x1, y1) || !in_bounds(x2, y2) {
                return Err("range".to_string());
            }
            DebugCmd::Swipe {
                from: (x1, y1),
                to: (x2, y2),
                dur_ms,
            }
        }
        "key" => {
            let name = tok
                .next()
                .ok_or_else(|| "parse: missing key name".to_string())?
                .to_string();
            let hold_ms = opt_u32(&mut tok, 0)?;
            done(tok)?;
            DebugCmd::Key { name, hold_ms }
        }
        "keydown" | "keyup" => {
            let name = tok
                .next()
                .ok_or_else(|| "parse: missing key name".to_string())?
                .to_string();
            done(tok)?;
            DebugCmd::KeyHold {
                name,
                down: verb == "keydown",
            }
        }
        "settle" => {
            let timeout_ms = opt_u32(&mut tok, 2000)?;
            done(tok)?;
            DebugCmd::Settle { timeout_ms }
        }
        "launch" => {
            let app_id = tok
                .next()
                .ok_or_else(|| "parse: missing app id".to_string())?
                .to_string();
            let new_window = match tok.next() {
                None => false,
                Some("new") => true,
                Some(other) => return Err(format!("parse: unknown launch flag {other}")),
            };
            done(tok)?;
            DebugCmd::Launch { app_id, new_window }
        }
        other => return Err(format!("parse: unknown verb {other}")),
    };
    Ok(cmd)
}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::time::Instant;

use crate::input_common;
use crate::State;

/// Reply written back to the socket client (already newline-terminated).
type Reply = String;

/// A command paired with the channel to answer it on.
type Job = (DebugCmd, SyncSender<Reply>);

/// An in-flight swipe advanced across render-loop ticks.
pub struct ActiveGesture {
    start: Instant,
    dur_ms: u32,
    from: (f32, f32),
    to: (f32, f32),
    started: bool,
    reply: SyncSender<Reply>,
}

/// A key held down across render-loop ticks, released once `hold_ms` elapses.
pub struct ActiveKey {
    keycode: smithay::backend::input::Keycode,
    release_at: Instant,
    reply: SyncSender<Reply>,
}

/// A touch held down across render-loop ticks, lifted once `release_at` passes,
/// so clients see a realistic tap dwell rather than an instant down+up.
pub struct ActiveTouch {
    slot: smithay::backend::input::TouchSlot,
    release_at: Instant,
    reply: SyncSender<Reply>,
}

/// Handle held by the render loop: the receiving end of the command channel.
pub struct DebugChannel {
    rx: Receiver<Job>,
}

/// Bind the socket at `path` and spawn a detached reader thread. Returns the
/// channel the render loop drains, or an error string if the socket can't bind.
///
/// `w`/`h` are the logical output bounds for the coordinate range check.
pub fn spawn(path: &str, w: f32, h: f32) -> std::io::Result<DebugChannel> {
    // Unlink any stale socket from a previous (crashed) run before binding.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let (tx, rx) = std::sync::mpsc::channel::<Job>();

    std::thread::Builder::new()
        .name("debug-input".into())
        .spawn(move || reader_loop(listener, tx, w, h))?;

    Ok(DebugChannel { rx })
}

/// Bind the control socket at [`crate::ipc::socket_path`] and start its reader
/// thread. Shared by both backends so the listener setup lives in one place;
/// returns `None` (logged) if the bind fails, which is non-fatal.
pub fn spawn_listener(output_size: (i32, i32)) -> Option<DebugChannel> {
    let path = crate::ipc::socket_path();
    match spawn(
        &path.to_string_lossy(),
        output_size.0 as f32,
        output_size.1 as f32,
    ) {
        Ok(chan) => {
            tracing::info!(path = %path.display(), "ipc socket listening");
            Some(chan)
        }
        Err(e) => {
            tracing::warn!(%e, "failed to bind ipc socket");
            None
        }
    }
}

/// Accept clients one at a time; parse each line and forward valid commands.
fn reader_loop(listener: UnixListener, tx: Sender<Job>, w: f32, h: f32) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let reply = match parse_line(&line, w, h) {
                // Parse/range errors are answered directly by the reader.
                Err(e) => format!("err {e}\n"),
                // Valid commands go to the render loop; block on its reply so
                // only one command is ever in flight.
                Ok(cmd) => {
                    let (rtx, rrx) = std::sync::mpsc::sync_channel::<Reply>(1);
                    if tx.send((cmd, rtx)).is_err() {
                        break; // render loop gone
                    }
                    rrx.recv().unwrap_or_else(|_| "err loop-gone\n".into())
                }
            };
            if writer.write_all(reply.as_bytes()).is_err() {
                break; // client disconnected
            }
        }
    }
}

/// Drain and dispatch debug commands. Call once per render-loop tick, before
/// rendering. Advances any in-flight swipe/settle first, else pops one command.
pub fn drain(state: &mut State, chan: &DebugChannel) {
    // A swipe is stateful across ticks; advance it and take no new command.
    if state.active_gesture.is_some() {
        advance_gesture(state);
        return;
    }
    // A held key likewise holds the one-in-flight slot until it is released.
    if state.active_key.is_some() {
        advance_key(state);
        return;
    }
    if state.active_touch.is_some() {
        advance_touch(state);
        return;
    }
    // A pending settle likewise holds the one-in-flight slot.
    if state.pending_settle.is_some() {
        check_settle(state);
        return;
    }
    if let Ok((cmd, reply)) = chan.rx.try_recv() {
        dispatch(state, cmd, reply);
    }
}

fn dispatch(state: &mut State, cmd: DebugCmd, reply: SyncSender<Reply>) {
    // Injected input counts as user activity, so ext-idle-notify clients see a
    // resume from a scripted tap the same way they would from a real one.
    // `settle`/`layers` are queries and `reload` a control verb, not input: none
    // of them must reset the countdown.
    if !matches!(
        cmd,
        DebugCmd::Settle { .. } | DebugCmd::Reload | DebugCmd::Layers | DebugCmd::Quit
    ) {
        state.idle_notify.activity(Instant::now());
    }
    // The gesture verbs (`down`/`move`/`up`/`tap`/`swipe`) feed the shell's
    // gesture funnel directly rather than going through `touch::down`, so the
    // session lock has to be honoured here too: a locked session takes no shell
    // input from a finger, and the harness must not be able to drive the UI
    // behind a lock screen either. `touch` routes through the real path (which
    // checks the lock itself), and `key` must keep working — that is how a
    // password reaches the lock client.
    if state.session_lock.is_locked()
        && matches!(
            cmd,
            DebugCmd::Down(..)
                | DebugCmd::Move(..)
                | DebugCmd::Up
                | DebugCmd::Tap(..)
                | DebugCmd::Swipe { .. }
                | DebugCmd::Launch { .. }
        )
    {
        let _ = reply.send("ok locked\n".into());
        return;
    }
    // Synthetic contacts stand in for a finger, so they park the mouse cursor
    // exactly as a real touch-down does (see `touch::down`).
    if matches!(
        cmd,
        DebugCmd::Down(..) | DebugCmd::Tap(..) | DebugCmd::Swipe { .. }
    ) && state.cursor_visible
    {
        state.cursor_visible = false;
        state.needs_render = true;
    }
    match cmd {
        DebugCmd::Down(x, y) => {
            input_common::on_motion(state, x, y); // seed last_pointer_pos first
            input_common::on_press(state);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Move(x, y) => {
            input_common::on_motion(state, x, y);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Up => {
            input_common::on_release(state);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Tap(x, y) => {
            input_common::on_motion(state, x, y);
            input_common::on_press(state);
            input_common::on_release(state);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Swipe { from, to, dur_ms } => {
            state.active_gesture = Some(ActiveGesture {
                start: Instant::now(),
                dur_ms,
                from,
                to,
                started: false,
                reply,
            });
        }
        DebugCmd::Key { name, hold_ms } => {
            let Some(keysym) = crate::keybinds::resolve_keysym(&name) else {
                let _ = reply.send("err unknown-keysym\n".into());
                return;
            };
            let Some(keycode) = crate::keybinds::keycode_for_keysym(state, keysym) else {
                let _ = reply.send("err unmapped-keysym\n".into());
                return;
            };
            crate::keybinds::on_key_event(
                state,
                keycode,
                smithay::backend::input::KeyState::Pressed,
                0,
            );
            state.active_key = Some(ActiveKey {
                keycode,
                release_at: Instant::now() + std::time::Duration::from_millis(hold_ms as u64),
                reply,
            });
        }
        DebugCmd::KeyHold { name, down } => {
            let Some(keysym) = crate::keybinds::resolve_keysym(&name) else {
                let _ = reply.send("err unknown-keysym\n".into());
                return;
            };
            let Some(keycode) = crate::keybinds::keycode_for_keysym(state, keysym) else {
                let _ = reply.send("err unmapped-keysym\n".into());
                return;
            };
            let key_state = if down {
                smithay::backend::input::KeyState::Pressed
            } else {
                smithay::backend::input::KeyState::Released
            };
            crate::keybinds::on_key_event(state, keycode, key_state, 0);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Orientation(o) => {
            state.set_device_orientation(o);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Touch(x, y) => {
            // Real touch tap through the surface-routing path, held ~120ms so
            // clients register a proper tap. Slot 0 = a valid libinput slot;
            // real monotonic time so clients don't see stale events.
            let slot = smithay::backend::input::TouchSlot::from(Some(0));
            let time = state.clock.now().as_millis();
            crate::touch::down(state, x, y, slot, time);
            // Synthetic input has no libinput frame event behind it, so the
            // one-finger batch is closed by hand.
            crate::touch::frame(state);
            state.active_touch = Some(ActiveTouch {
                slot,
                release_at: Instant::now() + std::time::Duration::from_millis(120),
                reply,
            });
        }
        DebugCmd::Reload => {
            // Allowed while locked: it takes no shell input, and a config reload
            // is exactly the kind of thing a session may need behind the lock.
            state.reload_config();
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Layers => {
            // Allowed while locked: it reads state and drives nothing.
            let _ = reply.send(format!("ok {}\n", state.layers_dump()));
        }
        DebugCmd::Quit => {
            // Answer before stopping: once the loop exits nothing drains this
            // channel, and the client would block until the socket closed.
            let _ = reply.send("ok\n".into());
            tracing::info!("quit requested over ipc");
            state.running = false;
        }
        DebugCmd::Launch { app_id, new_window } => {
            if !state.app_catalog.contains_key(&app_id) {
                let _ = reply.send(format!("err unknown app {app_id}\n"));
                return;
            }
            // Centered origin: there is no icon to zoom from when the request
            // came over the socket (or from the search app's list).
            let (w, h) = state.output_size_f();
            let origin = crate::ui_state::ZoomOrigin::icon((w / 2.0, h / 2.0));
            if new_window {
                state.spawn_instance(&app_id, origin);
            } else {
                state.launch_or_raise(&app_id, origin);
            }
            state.needs_render = true;
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Settle { timeout_ms } => {
            if idle(state) {
                let _ = reply.send("ok\n".into());
            } else {
                let deadline = Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
                state.pending_settle = Some((reply, deadline));
            }
        }
    }
}

fn advance_gesture(state: &mut State) {
    let mut g = state.active_gesture.take().expect("advance with gesture");
    // The session locked mid-swipe: abandon it rather than keep feeding motion
    // into a shell the user can no longer see. `cancel_gestures` already dropped
    // whatever the press had started.
    if state.session_lock.is_locked() {
        let _ = g.reply.send("ok locked\n".into());
        return;
    }
    let elapsed = g.start.elapsed().as_millis() as f32;
    let t = swipe_t(elapsed, g.dur_ms);

    if !g.started {
        input_common::on_motion(state, g.from.0, g.from.1); // seed before press
        input_common::on_press(state);
        g.started = true;
    }

    let (px, py) = sc_anim::lerp_point(g.from, g.to, t);
    input_common::on_motion(state, px, py);

    if t >= 1.0 {
        // Final on_motion(to) has landed; release now carries correct velocity.
        input_common::on_release(state);
        let _ = g.reply.send("ok\n".into());
        // Dropped: gesture complete.
    } else {
        state.active_gesture = Some(g);
    }
}

/// Release a held debug key once its hold time has passed. Long-press bindings
/// fire from the normal per-frame poll while it is still held, exactly as they
/// do for a real key.
fn advance_touch(state: &mut State) {
    let t = state.active_touch.take().expect("advance with touch");
    if Instant::now() < t.release_at {
        state.active_touch = Some(t);
        return;
    }
    let time = state.clock.now().as_millis();
    crate::touch::up(state, t.slot, time);
    crate::touch::frame(state);
    let _ = t.reply.send("ok\n".into());
}

fn advance_key(state: &mut State) {
    let key = state.active_key.take().expect("advance with key");
    if Instant::now() < key.release_at {
        state.active_key = Some(key);
        return;
    }
    crate::keybinds::on_key_event(
        state,
        key.keycode,
        smithay::backend::input::KeyState::Released,
        0,
    );
    let _ = key.reply.send("ok\n".into());
}

fn check_settle(state: &mut State) {
    let (reply, deadline) = state.pending_settle.take().expect("settle pending");
    if idle(state) {
        let _ = reply.send("ok\n".into());
    } else if Instant::now() >= deadline {
        let _ = reply.send("err timeout\n".into());
    } else {
        state.pending_settle = Some((reply, deadline));
    }
}

/// Compositor idle: no springs animating, no gesture, pointer up.
fn idle(state: &State) -> bool {
    // Grid-reflow springs live on `State`, not `UiState`, so `needs_animation`
    // can't see them — include them here so `settle` waits out a reflow.
    let grid_settled = state
        .grid_anim
        .values()
        .all(|(x, y)| x.is_settled() && y.is_settled());
    is_idle(
        state.ui.needs_animation()
            || !grid_settled
            // A turn waiting out its debounce, or the fade covering one: a
            // screenshot taken mid-dip is black, and one taken before the turn
            // lands shows the old orientation.
            || state.orientation_settle.is_pending()
            || state.rotation_fade.is_active(),
        state.active_gesture.is_some()
            || state.active_key.is_some()
            || state.active_touch.is_some(),
        state.pointer_down,
    )
}

/// Normalized swipe progress for `elapsed_ms` of a `dur_ms` gesture. Clamped to
/// `[0,1]`; a zero-duration swipe is immediately complete (`1.0`).
pub fn swipe_t(elapsed_ms: f32, dur_ms: u32) -> f32 {
    if dur_ms == 0 {
        return 1.0;
    }
    (elapsed_ms / dur_ms as f32).clamp(0.0, 1.0)
}

/// The compositor is idle (safe to screenshot) when no springs are animating,
/// no synthetic gesture is in flight, and no pointer is held down.
pub fn is_idle(needs_animation: bool, gesture_active: bool, pointer_down: bool) -> bool {
    !needs_animation && !gesture_active && !pointer_down
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1224.0;
    const H: f32 = 2700.0;

    #[test]
    fn swipe_t_progresses_and_clamps() {
        assert_eq!(swipe_t(0.0, 200), 0.0);
        assert_eq!(swipe_t(100.0, 200), 0.5);
        assert_eq!(swipe_t(200.0, 200), 1.0);
        assert_eq!(swipe_t(999.0, 200), 1.0);
        assert_eq!(swipe_t(50.0, 0), 1.0); // zero-duration completes immediately
    }

    #[test]
    fn idle_only_when_all_quiet() {
        assert!(is_idle(false, false, false));
        assert!(!is_idle(true, false, false)); // animating
        assert!(!is_idle(false, true, false)); // gesture in flight
        assert!(!is_idle(false, false, true)); // pointer held
    }

    #[test]
    fn parses_down() {
        assert_eq!(
            parse_line("down 10 20", W, H),
            Ok(DebugCmd::Down(10.0, 20.0))
        );
    }

    #[test]
    fn parses_move_up_tap() {
        assert_eq!(parse_line("move 5 6", W, H), Ok(DebugCmd::Move(5.0, 6.0)));
        assert_eq!(parse_line("up", W, H), Ok(DebugCmd::Up));
        assert_eq!(
            parse_line("tap 100 200", W, H),
            Ok(DebugCmd::Tap(100.0, 200.0))
        );
    }

    #[test]
    fn parses_swipe_with_and_without_duration() {
        assert_eq!(
            parse_line("swipe 1 2 3 4 500", W, H),
            Ok(DebugCmd::Swipe {
                from: (1.0, 2.0),
                to: (3.0, 4.0),
                dur_ms: 500
            })
        );
        assert_eq!(
            parse_line("swipe 1 2 3 4", W, H),
            Ok(DebugCmd::Swipe {
                from: (1.0, 2.0),
                to: (3.0, 4.0),
                dur_ms: 200
            })
        );
    }

    #[test]
    fn parses_touch() {
        assert_eq!(
            parse_line("touch 10 20", W, H),
            Ok(DebugCmd::Touch(10.0, 20.0))
        );
        assert!(parse_line("touch 10", W, H).is_err());
        assert!(parse_line("touch -1 20", W, H).is_err()); // out of bounds
        assert!(parse_line("touch 10 20 30", W, H).is_err());
    }

    #[test]
    fn parses_held_key_verbs() {
        assert_eq!(
            parse_line("keydown Super_L", W, H).unwrap(),
            DebugCmd::KeyHold {
                name: "Super_L".into(),
                down: true
            }
        );
        assert_eq!(
            parse_line("keyup Super_L", W, H).unwrap(),
            DebugCmd::KeyHold {
                name: "Super_L".into(),
                down: false
            }
        );
        assert!(parse_line("keydown", W, H).is_err());
        assert!(parse_line("keyup Super_L extra", W, H).is_err());
    }

    #[test]
    fn parses_key_with_and_without_hold() {
        assert_eq!(
            parse_line("key XF86PowerOff", W, H),
            Ok(DebugCmd::Key {
                name: "XF86PowerOff".into(),
                hold_ms: 0
            })
        );
        assert_eq!(
            parse_line("key XF86PowerOff 600", W, H),
            Ok(DebugCmd::Key {
                name: "XF86PowerOff".into(),
                hold_ms: 600
            })
        );
        assert!(parse_line("key", W, H).is_err());
        assert!(parse_line("key A 600 extra", W, H).is_err());
    }

    #[test]
    fn parses_settle_with_and_without_timeout() {
        assert_eq!(
            parse_line("settle 1000", W, H),
            Ok(DebugCmd::Settle { timeout_ms: 1000 })
        );
        assert_eq!(
            parse_line("settle", W, H),
            Ok(DebugCmd::Settle { timeout_ms: 2000 })
        );
    }

    #[test]
    fn parses_launch() {
        assert_eq!(
            parse_line("launch org.gnome.Maps", W, H),
            Ok(DebugCmd::Launch {
                app_id: "org.gnome.Maps".into(),
                new_window: false,
            })
        );
        assert_eq!(
            parse_line("launch foot new", W, H),
            Ok(DebugCmd::Launch {
                app_id: "foot".into(),
                new_window: true,
            })
        );
        assert!(parse_line("launch", W, H).is_err()); // needs an app id
        assert!(parse_line("launch foot copy", W, H).is_err()); // unknown flag
    }

    #[test]
    fn parses_layers() {
        assert_eq!(parse_line("layers", W, H), Ok(DebugCmd::Layers));
        assert!(parse_line("layers all", W, H).is_err()); // no args
    }

    #[test]
    fn parses_quit() {
        assert_eq!(parse_line("quit", W, H), Ok(DebugCmd::Quit));
        assert!(parse_line("quit now", W, H).is_err()); // no args
    }

    #[test]
    fn parses_reload() {
        assert_eq!(parse_line("reload", W, H), Ok(DebugCmd::Reload));
        assert!(parse_line("reload now", W, H).is_err()); // no args
    }

    #[test]
    fn coord_bounds_are_inclusive() {
        // Exactly on the boundary is accepted (edge gestures).
        assert_eq!(parse_line("down 0 0", W, H), Ok(DebugCmd::Down(0.0, 0.0)));
        assert_eq!(
            parse_line("down 1224 2700", W, H),
            Ok(DebugCmd::Down(1224.0, 2700.0))
        );
    }

    #[test]
    fn out_of_range_coords_rejected() {
        assert_eq!(parse_line("down -1 0", W, H), Err("range".into()));
        assert_eq!(parse_line("down 1225 0", W, H), Err("range".into()));
        assert_eq!(parse_line("down 0 2701", W, H), Err("range".into()));
        // Swipe endpoints checked too.
        assert!(parse_line("swipe 0 0 9999 0", W, H).is_err());
    }

    #[test]
    fn bad_input_rejected() {
        assert!(parse_line("", W, H).unwrap_err().starts_with("parse"));
        assert!(parse_line("wobble 1 2", W, H)
            .unwrap_err()
            .starts_with("parse"));
        assert!(parse_line("down 1", W, H).unwrap_err().starts_with("parse")); // arity
        assert!(parse_line("down x y", W, H)
            .unwrap_err()
            .starts_with("parse")); // non-numeric
        assert!(parse_line("up extra", W, H)
            .unwrap_err()
            .starts_with("parse")); // trailing
    }
}
