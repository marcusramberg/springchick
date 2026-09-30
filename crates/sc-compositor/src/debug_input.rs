//! The control socket's line protocol; `springchick ipc <verb>` is the
//! client. A reader thread parses commands and the render loop injects them
//! through `input_common`.
//!
//! Gesture verbs answer `ok locked` and do nothing under a session lock.
//! `key` still works: that's how a scripted password reaches the lock client.

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
    Key {
        name: String,
        hold_ms: u32,
    },
    /// Press or release a key and return at once, leaving it held, so chords
    /// (`keydown Super_L`, `key Tab`, `keyup Super_L`) can be scripted. Takes no
    /// in-flight slot.
    KeyHold {
        name: String,
        down: bool,
    },
    /// A real tap through `touch::down`/`up`.
    Touch(f32, f32),
    /// Take over a drag from the search app with the finger still down. `at`
    /// (output pixels) is for driving it by hand; a real handoff uses the live
    /// touch.
    Drag {
        app_id: String,
        at: Option<(f32, f32)>,
    },
    /// Fakes an accelerometer report.
    Orientation(crate::rotation::DeviceOrientation),
    /// Re-read `config.toml` (live settings only) and rescan the catalog.
    Reload,
    /// As SIGTERM. The shell's logout goes through this.
    Quit,
    /// A query; see [`State::layers_dump`].
    Layers,
    Home,
    /// Open an app as an icon tap would (raise, else launch); `new_window`
    /// forces a new instance. The search app uses this for attribution and
    /// de-duplication.
    Launch {
        app_id: String,
        new_window: bool,
    },
    /// A built-in action by config name. `command` isn't reachable.
    Action(sc_config::Action),
}

/// `w`/`h` bound the inclusive coordinate check. Errors are short tags
/// (`parse ...`, `range`).
pub fn parse_line(line: &str, w: f32, h: f32) -> Result<DebugCmd, String> {
    let mut tok = line.split_whitespace();
    let verb = tok.next().ok_or_else(|| "parse: empty".to_string())?;

    fn num<'a>(t: &mut impl Iterator<Item = &'a str>) -> Result<f32, String> {
        t.next()
            .ok_or_else(|| "parse: missing arg".to_string())?
            .parse::<f32>()
            .map_err(|_| "parse: not a number".to_string())
    }
    fn opt_u32<'a>(t: &mut impl Iterator<Item = &'a str>, default: u32) -> Result<u32, String> {
        match t.next() {
            None => Ok(default),
            Some(s) => s
                .parse::<u32>()
                .map_err(|_| "parse: not an integer".to_string()),
        }
    }
    let in_bounds = |x: f32, y: f32| x >= 0.0 && x <= w && y >= 0.0 && y <= h;

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
            // Same spelling as iio-sensor-proxy.
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
        "home" => {
            done(tok)?;
            DebugCmd::Home
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
        "drag" => {
            let app_id = tok
                .next()
                .ok_or_else(|| "parse: missing app id".to_string())?
                .to_string();
            let at = match tok.next() {
                None => None,
                Some(sx) => {
                    let x: f32 = sx.parse().map_err(|_| "parse: not a number".to_string())?;
                    let y = num(&mut tok)?;
                    if !in_bounds(x, y) {
                        return Err("range".to_string());
                    }
                    Some((x, y))
                }
            };
            done(tok)?;
            DebugCmd::Drag { app_id, at }
        }
        "action" => {
            let name = tok
                .next()
                .ok_or_else(|| "parse: missing action name".to_string())?;
            done(tok)?;
            let action = sc_config::Action::from_name(name)
                .ok_or_else(|| format!("parse: unknown action {name}"))?;
            DebugCmd::Action(action)
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

/// Newline-terminated.
type Reply = String;

type Job = (DebugCmd, SyncSender<Reply>);

pub struct ActiveGesture {
    start: Instant,
    dur_ms: u32,
    from: (f32, f32),
    to: (f32, f32),
    started: bool,
    reply: SyncSender<Reply>,
}

pub struct ActiveKey {
    keycode: smithay::backend::input::Keycode,
    release_at: Instant,
    reply: SyncSender<Reply>,
}

/// Held until `release_at` so clients see a real tap dwell.
pub struct ActiveTouch {
    slot: smithay::backend::input::TouchSlot,
    release_at: Instant,
    reply: SyncSender<Reply>,
}

pub struct DebugChannel {
    rx: Receiver<Job>,
}

pub fn spawn(path: &str, w: f32, h: f32) -> std::io::Result<DebugChannel> {
    // Remove a stale socket from a crashed run.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let (tx, rx) = std::sync::mpsc::channel::<Job>();

    std::thread::Builder::new()
        .name("debug-input".into())
        .spawn(move || reader_loop(listener, tx, w, h))?;

    Ok(DebugChannel { rx })
}

/// Bind failure is logged and non-fatal.
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
                Err(e) => format!("err {e}\n"),
                // Block on the reply: one command in flight at a time.
                Ok(cmd) => {
                    let (rtx, rrx) = std::sync::mpsc::sync_channel::<Reply>(1);
                    if tx.send((cmd, rtx)).is_err() {
                        break;
                    }
                    rrx.recv().unwrap_or_else(|_| "err loop-gone\n".into())
                }
            };
            if writer.write_all(reply.as_bytes()).is_err() {
                break;
            }
        }
    }
}

/// Call once per tick, before rendering. An in-flight swipe/key/settle holds
/// the slot; otherwise one command is popped.
pub fn drain(state: &mut State, chan: &DebugChannel) {
    if state.active_gesture.is_some() {
        advance_gesture(state);
        return;
    }
    if state.active_key.is_some() {
        advance_key(state);
        return;
    }
    if state.active_touch.is_some() {
        advance_touch(state);
        return;
    }
    if state.pending_settle.is_some() {
        check_settle(state);
        return;
    }
    if let Ok((cmd, reply)) = chan.rx.try_recv() {
        dispatch(state, cmd, reply);
    }
}

fn dispatch(state: &mut State, cmd: DebugCmd, reply: SyncSender<Reply>) {
    // Injected input resets idle like real input; queries and control verbs
    // don't.
    if !matches!(
        cmd,
        DebugCmd::Settle { .. }
            | DebugCmd::Reload
            | DebugCmd::Layers
            | DebugCmd::Home
            | DebugCmd::Quit
    ) {
        state.idle_notify.activity(Instant::now());
    }
    // Gesture verbs bypass `touch::down`, so the lock is checked here. `touch`
    // takes the real path, and `key` must work so a password can be typed.
    if state.session_lock.is_locked()
        && matches!(
            cmd,
            DebugCmd::Down(..)
                | DebugCmd::Move(..)
                | DebugCmd::Up
                | DebugCmd::Tap(..)
                | DebugCmd::Swipe { .. }
                | DebugCmd::Launch { .. }
                | DebugCmd::Drag { .. }
        )
    {
        let _ = reply.send("ok locked\n".into());
        return;
    }
    // Synthetic contacts hide the cursor like a real touch-down.
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
        DebugCmd::Drag { app_id, at } => {
            if state.app_catalog.contains_key(&app_id) {
                state.lift_from_search(app_id, at);
                let _ = reply.send("ok\n".into());
            } else {
                let _ = reply.send("err unknown-app\n".into());
            }
        }
        DebugCmd::Orientation(o) => {
            state.set_device_orientation(o);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Touch(x, y) => {
            // Held ~120ms so clients register a tap.
            let slot = smithay::backend::input::TouchSlot::from(Some(0));
            let time = state.clock.now().as_millis();
            crate::touch::down(state, x, y, slot, time);
            // No libinput frame event behind synthetic input.
            crate::touch::frame(state);
            state.active_touch = Some(ActiveTouch {
                slot,
                release_at: Instant::now() + std::time::Duration::from_millis(120),
                reply,
            });
        }
        DebugCmd::Reload => {
            state.reload_config();
            state.reload_catalog();
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Layers => {
            let _ = reply.send(format!("ok {}\n", state.layers_dump()));
        }
        DebugCmd::Home => {
            let _ = reply.send(format!("ok {}\n", state.home_dump()));
        }
        DebugCmd::Quit => {
            // Answer first: nothing drains the channel once the loop exits.
            let _ = reply.send("ok\n".into());
            tracing::info!("quit requested over ipc");
            state.running = false;
        }
        DebugCmd::Action(action) => {
            // `run_action` applies the lock policy itself.
            crate::keybinds::run_action(state, action);
            let _ = reply.send("ok\n".into());
        }
        DebugCmd::Launch { app_id, new_window } => {
            if !state.app_catalog.contains_key(&app_id) {
                let _ = reply.send(format!("err unknown app {app_id}\n"));
                return;
            }
            // No icon to zoom from; use the centre.
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
    // Locked mid-swipe: abandon it.
    if state.session_lock.is_locked() {
        let _ = g.reply.send("ok locked\n".into());
        return;
    }
    // The press tick emits no motion and restarts the clock. A motion here,
    // right after the press stamped `last_motion`, reads as tens of screens per
    // second, and on a slow output the phantom speed survives to the release as
    // a flick.
    if !g.started {
        input_common::on_motion(state, g.from.0, g.from.1);
        input_common::on_press(state);
        g.started = true;
        g.start = Instant::now();
        state.active_gesture = Some(g);
        return;
    }

    let elapsed = g.start.elapsed().as_millis() as f32;
    let t = swipe_t(elapsed, g.dur_ms);
    let (px, py) = sc_anim::lerp_point(g.from, g.to, t);
    input_common::on_motion(state, px, py);

    if t >= 1.0 {
        input_common::on_release(state);
        let _ = g.reply.send("ok\n".into());
    } else {
        state.active_gesture = Some(g);
    }
}

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

fn idle(state: &State) -> bool {
    // `needs_animation` can't see the grid springs on `State`.
    let grid_settled = state
        .grid_anim
        .values()
        .all(|(x, y)| x.is_settled() && y.is_settled());
    is_idle(
        state.ui.needs_animation()
            || !grid_settled
            // Mid-dip screenshots are black.
            || state.orientation_settle.is_pending()
            || state.rotation_fade.is_active(),
        state.active_gesture.is_some()
            || state.active_key.is_some()
            || state.active_touch.is_some(),
        state.pointer_down,
    )
}

/// Clamped to `[0,1]`; zero duration is `1.0`.
pub fn swipe_t(elapsed_ms: f32, dur_ms: u32) -> f32 {
    if dur_ms == 0 {
        return 1.0;
    }
    (elapsed_ms / dur_ms as f32).clamp(0.0, 1.0)
}

/// Safe to screenshot.
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
        assert_eq!(swipe_t(50.0, 0), 1.0);
    }

    #[test]
    fn idle_only_when_all_quiet() {
        assert!(is_idle(false, false, false));
        assert!(!is_idle(true, false, false));
        assert!(!is_idle(false, true, false));
        assert!(!is_idle(false, false, true));
    }

    #[test]
    fn parses_down() {
        assert_eq!(
            parse_line("down 10 20", W, H),
            Ok(DebugCmd::Down(10.0, 20.0))
        );
    }

    #[test]
    fn parses_drag_with_and_without_position() {
        assert_eq!(
            parse_line("drag foo", W, H),
            Ok(DebugCmd::Drag {
                app_id: "foo".into(),
                at: None
            })
        );
        assert_eq!(
            parse_line("drag foo 10 20", W, H),
            Ok(DebugCmd::Drag {
                app_id: "foo".into(),
                at: Some((10.0, 20.0))
            })
        );
        assert_eq!(
            parse_line("drag", W, H),
            Err("parse: missing app id".into())
        );
        assert_eq!(
            parse_line("drag foo 10", W, H),
            Err("parse: missing arg".into())
        );
        assert_eq!(
            parse_line("drag foo 1 2 3", W, H),
            Err("parse: trailing tokens".into())
        );
        assert_eq!(parse_line("drag foo 10 99999", W, H), Err("range".into()));
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
        assert!(parse_line("touch -1 20", W, H).is_err());
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
        assert!(parse_line("launch", W, H).is_err());
        assert!(parse_line("launch foot copy", W, H).is_err());
    }

    #[test]
    fn parses_action() {
        assert_eq!(
            parse_line("action screenshot", W, H),
            Ok(DebugCmd::Action(sc_config::Action::Screenshot))
        );
        assert!(parse_line("action", W, H).is_err());
        assert!(parse_line("action nope", W, H).is_err());
        assert!(parse_line("action command", W, H).is_err());
    }

    #[test]
    fn parses_layers() {
        assert_eq!(parse_line("layers", W, H), Ok(DebugCmd::Layers));
        assert_eq!(parse_line("home", W, H), Ok(DebugCmd::Home));
        assert!(parse_line("home now", W, H).is_err());
        assert!(parse_line("layers all", W, H).is_err());
    }

    #[test]
    fn parses_quit() {
        assert_eq!(parse_line("quit", W, H), Ok(DebugCmd::Quit));
        assert!(parse_line("quit now", W, H).is_err());
    }

    #[test]
    fn parses_reload() {
        assert_eq!(parse_line("reload", W, H), Ok(DebugCmd::Reload));
        assert!(parse_line("reload now", W, H).is_err());
    }

    #[test]
    fn coord_bounds_are_inclusive() {
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
        assert!(parse_line("swipe 0 0 9999 0", W, H).is_err());
    }

    #[test]
    fn bad_input_rejected() {
        assert!(parse_line("", W, H).unwrap_err().starts_with("parse"));
        assert!(parse_line("wobble 1 2", W, H)
            .unwrap_err()
            .starts_with("parse"));
        assert!(parse_line("down 1", W, H).unwrap_err().starts_with("parse"));
        assert!(parse_line("down x y", W, H)
            .unwrap_err()
            .starts_with("parse"));
        assert!(parse_line("up extra", W, H)
            .unwrap_err()
            .starts_with("parse"));
    }
}
