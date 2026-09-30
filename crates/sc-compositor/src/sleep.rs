//! Blank the panel before suspend, via a logind `delay` inhibitor.
//!
//! Without it `Blank` stays unblanked across the suspend, so the waking power
//! press falls through to `toggle-display` and turns the screen off. The
//! inhibitor keeps the DRM commit from racing the suspend.
//!
//! The channel is a calloop source, not an mpsc drain: a suspend can arrive
//! while nothing is rendering. Any D-Bus failure just skips the blank.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dbus::arg::OwnedFd;
use dbus::blocking::Connection;
use dbus::message::MatchRule;

use tracing::{debug, info, warn};

const SERVICE: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";

const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Idle wakeup only; `PrepareForSleep` wakes `process` early. The thread must
/// keep running while the panel is dark.
const POLL: Duration = Duration::from_secs(30);
/// Wait for the compositor's ack. Under logind's `InhibitDelayMaxSec`.
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Blank, then ack; the suspend waits for the ack or [`ACK_TIMEOUT`].
    AboutToSleep,
}

pub struct Sleep {
    pub events: calloop::channel::Channel<Event>,
    pub acks: Sender<()>,
}

/// Connects on the worker so a slow system bus can't stall startup.
pub fn spawn() -> Option<Sleep> {
    let (tx_event, events) = calloop::channel::channel();
    let (acks, rx_ack) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("sc-sleep".into())
        .spawn(move || worker(&tx_event, &rx_ack))
        .map_err(|e| warn!(%e, "could not start sleep thread"))
        .ok()?;

    Some(Sleep { events, acks })
}

fn inhibit(conn: &Connection) -> Option<OwnedFd> {
    let proxy = conn.with_proxy(SERVICE, PATH, CALL_TIMEOUT);
    match proxy.method_call::<(OwnedFd,), _, _, _>(
        MANAGER,
        "Inhibit",
        (
            "sleep",
            "springchick",
            "Blank the panel before the system sleeps",
            "delay",
        ),
    ) {
        Ok((fd,)) => Some(fd),
        Err(e) => {
            info!(%e, "sleep inhibitor refused; the panel may not blank before suspend");
            None
        }
    }
}

fn worker(tx: &calloop::channel::Sender<Event>, rx_ack: &Receiver<()>) {
    let conn = match Connection::new_system() {
        Ok(c) => c,
        Err(e) => {
            debug!(target: "springchick::debug", %e, "no system bus; the panel will not blank before sleep");
            return;
        }
    };

    // Handled outside the match callback: acking blocks, and re-arming calls on
    // the connection still being dispatched.
    let pending: Arc<Mutex<Option<bool>>> = Arc::default();
    let rule = MatchRule::new_signal(MANAGER, "PrepareForSleep").with_path(PATH);
    let seen = Arc::clone(&pending);
    if let Err(e) = conn.add_match(rule, move |(start,): (bool,), _, _| {
        *seen.lock().expect("sleep signal mutex") = Some(start);
        true
    }) {
        warn!(%e, "could not subscribe to PrepareForSleep; the panel will not blank before sleep");
        return;
    }

    let mut inhibitor = inhibit(&conn);
    info!(target: "springchick::debug", held = inhibitor.is_some(), "watching for suspend");

    loop {
        if let Err(e) = conn.process(POLL) {
            warn!(%e, "logind connection error; the panel will not blank before sleep");
            return;
        }
        let Some(start) = pending.lock().expect("sleep signal mutex").take() else {
            continue;
        };
        if start {
            debug!(target: "springchick::debug", "suspend imminent; blanking");
            if tx.send(Event::AboutToSleep).is_err() {
                return;
            }
            match rx_ack.recv_timeout(ACK_TIMEOUT) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    warn!("compositor did not blank in time; suspending anyway");
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Closing the fd is what lets logind proceed.
            drop(inhibitor.take());
        } else {
            // Resumed. The inhibitor was consumed; re-arm for the next suspend.
            debug!(target: "springchick::debug", "resumed; re-arming the sleep inhibitor");
            inhibitor = inhibit(&conn);
        }
    }
}
