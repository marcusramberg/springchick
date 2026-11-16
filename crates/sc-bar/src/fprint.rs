//! Fingerprint unlock for the lock screen, through fprintd
//! (`net.reactivated.Fprint`, tee-fprintd too). The sensor is claimed only
//! between [`Cmd::Start`] and [`Cmd::Stop`]: while the lock screen shows on a
//! lit panel.

use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::Duration;

use dbus::blocking::Connection;
use dbus::message::MatchRule;
use dbus::Path;

const FPRINT: &str = "net.reactivated.Fprint";
const DEVICE: &str = "net.reactivated.Fprint.Device";
/// Wrong fingers before the sensor is left alone until the next showing.
const MAX_MISSES: u32 = 5;

pub enum Cmd {
    /// Read fingers for this showing of the lock screen.
    Start(u32),
    Stop,
}

/// Each with the showing it was started for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The sensor is reading.
    Ready(u32),
    Matched(u32),
    /// Not this finger, or not read well: it reads on.
    Retry(u32),
    /// No sensor, no enrolled finger, or too many misses.
    Off(u32),
}

/// What a `VerifyStatus` means for a read in progress.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Matched,
    /// Read again; `restart` when fprintd ended the read to say so.
    Retry {
        restart: bool,
    },
    GiveUp,
}

fn step(result: &str, done: bool, misses: u32) -> Step {
    match result {
        "verify-match" => Step::Matched,
        "verify-no-match" if misses + 1 >= MAX_MISSES => Step::GiveUp,
        "verify-no-match" => Step::Retry { restart: true },
        // Disconnected, or an error of the sensor's own.
        _ if done => Step::GiveUp,
        // Too short, off centre: fprintd reads on by itself.
        _ => Step::Retry { restart: false },
    }
}

pub fn watch(send: impl Fn(Event) + Send + 'static) -> mpsc::Sender<Cmd> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Ok(bus) = Connection::new_system() else {
            return;
        };
        let (status_tx, status) = mpsc::channel::<(String, bool)>();
        let mut device: Option<Path<'static>> = None;
        // The showing being read for, and its misses.
        let mut on: Option<(u32, u32)> = None;
        let call = |device: &Path<'static>, method: &str| {
            bus.with_proxy(FPRINT, device, Duration::from_secs(5))
                .method_call::<(), _, _, _>(DEVICE, method, ())
        };
        let begin = |device: &Path<'static>, claim: bool| -> Result<(), dbus::Error> {
            let proxy = bus.with_proxy(FPRINT, device, Duration::from_secs(5));
            if claim {
                // "": the caller's own fingers.
                proxy.method_call::<(), _, _, _>(DEVICE, "Claim", ("",))?;
            }
            proxy
                .method_call::<(), _, _, _>(DEVICE, "VerifyStart", ("any",))
                .inspect_err(|_| {
                    let _ = call(device, "Release");
                })
        };
        let end = |device: &Path<'static>| {
            let _ = call(device, "VerifyStop");
            let _ = call(device, "Release");
        };
        loop {
            // Idle, it sleeps on the channel; reading, it looks every 100 ms.
            let cmd = if on.is_some() {
                let _ = bus.process(Duration::from_millis(100));
                match rx.try_recv() {
                    Ok(cmd) => Some(cmd),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return,
                }
            } else {
                match rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return,
                }
            };
            if cmd.is_some() {
                if let (Some(_), Some(device)) = (on.take(), &device) {
                    end(device);
                }
            }
            if let Some(Cmd::Start(showing)) = cmd {
                if device.is_none() {
                    device = find(&bus, status_tx.clone());
                }
                // Answers to an earlier read are not this one's.
                while status.try_recv().is_ok() {}
                let began = device.as_ref().is_some_and(|d| begin(d, true).is_ok());
                on = began.then_some((showing, 0));
                send(if began {
                    Event::Ready(showing)
                } else {
                    Event::Off(showing)
                });
            }
            while let Ok((result, done)) = status.try_recv() {
                let (Some((showing, misses)), Some(device)) = (on, &device) else {
                    continue;
                };
                match step(&result, done, misses) {
                    Step::Matched => {
                        end(device);
                        on = None;
                        send(Event::Matched(showing));
                    }
                    Step::Retry { restart } => {
                        let again = !restart
                            || (call(device, "VerifyStop").is_ok() && begin(device, false).is_ok());
                        if again {
                            on = Some((showing, misses + restart as u32));
                            send(Event::Retry(showing));
                        } else {
                            end(device);
                            on = None;
                            send(Event::Off(showing));
                        }
                    }
                    Step::GiveUp => {
                        end(device);
                        on = None;
                        send(Event::Off(showing));
                    }
                }
            }
        }
    });
    tx
}

/// The sensor, with its `VerifyStatus` signals sent on to `status`.
fn find(bus: &Connection, status: mpsc::Sender<(String, bool)>) -> Option<Path<'static>> {
    let (device,): (Path,) = bus
        .with_proxy(
            FPRINT,
            "/net/reactivated/Fprint/Manager",
            Duration::from_secs(5),
        )
        .method_call("net.reactivated.Fprint.Manager", "GetDefaultDevice", ())
        .ok()?;
    let rule = MatchRule::new_signal(DEVICE, "VerifyStatus")
        // Anyone on the bus may send a signal: only fprintd's count.
        .with_sender(FPRINT)
        .with_path(device.clone());
    bus.add_match(rule, move |answer: (String, bool), _, _| {
        status.send(answer).is_ok()
    })
    .ok()?;
    Some(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_match_unlocks_and_misses_run_out() {
        assert_eq!(step("verify-match", true, 0), Step::Matched);
        assert_eq!(
            step("verify-no-match", true, 0),
            Step::Retry { restart: true }
        );
        assert_eq!(step("verify-no-match", true, MAX_MISSES - 1), Step::GiveUp);
    }

    #[test]
    fn a_poor_read_goes_on_and_a_dead_sensor_gives_up() {
        assert_eq!(
            step("verify-retry-scan", false, 0),
            Step::Retry { restart: false }
        );
        assert_eq!(step("verify-disconnected", true, 0), Step::GiveUp);
        assert_eq!(step("verify-unknown-error", true, 0), Step::GiveUp);
    }
}
