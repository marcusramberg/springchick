//! Debug input socket — the line protocol behind the control socket.
//!
//! The compositor (both winit and DRM backends) always listens on
//! [`crate::ipc::socket_path`]; `springchick ipc <verb>` is the client. A reader
//! thread parses newline commands off the unix socket into [`DebugCmd`] values
//! and hands them to the render loop, which injects them through the existing
//! `input_common` funnel. See
//! `docs/superpowers/specs/2026-07-24-debug-input-socket-design.md`.

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
    /// A real touch-down+up tap at `(x, y)` through the surface-routing path
    /// (`touch::down`/`up`), for exercising layer-surface/app input.
    Touch(f32, f32),
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
        "up" => {
            done(tok)?;
            DebugCmd::Up
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
        "settle" => {
            let timeout_ms = opt_u32(&mut tok, 2000)?;
            done(tok)?;
            DebugCmd::Settle { timeout_ms }
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
    match spawn(&path.to_string_lossy(), output_size.0 as f32, output_size.1 as f32) {
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
        DebugCmd::Touch(x, y) => {
            // Real touch tap through the surface-routing path, held ~120ms so
            // clients register a proper tap. Slot 0 = a valid libinput slot;
            // real monotonic time so clients don't see stale events.
            let slot = smithay::backend::input::TouchSlot::from(Some(0));
            let time = state.start_time.elapsed().as_millis() as u32;
            crate::touch::down(state, x, y, slot, time);
            state.active_touch = Some(ActiveTouch {
                slot,
                release_at: Instant::now() + std::time::Duration::from_millis(120),
                reply,
            });
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
    let time = state.start_time.elapsed().as_millis() as u32;
    crate::touch::up(state, t.slot, time);
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
        state.ui.needs_animation() || !grid_settled,
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
