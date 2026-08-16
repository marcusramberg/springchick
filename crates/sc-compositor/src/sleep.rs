//! Blank the panel before the system suspends, over the system D-Bus.
//!
//! A worker thread owns the connection and posts [`Event`]s into the event loop
//! through a calloop channel — the same shape as [`crate::sensor`], except the
//! channel is a loop *source* rather than a once-a-tick drain: a suspend can
//! arrive while nothing is rendering, and a `std::sync::mpsc` receiver would sit
//! unread until something else happened to wake the loop.
//!
//! **Why blank at all.** [`crate::blank::Blank`] is the compositor's idea of
//! whether the panel is lit, and suspending does not change it. Sleep with the
//! panel on and it stays `false` across the resume, so the press that wakes the
//! machine is not seen as a wake: [`crate::blank::Blank::on_key_press`] returns
//! `Normal` and the press falls through to its binding, which for the power
//! button is `toggle-display`. The screen the user just woke goes black, and it
//! takes a second press to get it back. Blanking on the way down keeps the state
//! honest, and is what the panel should be doing while the machine is asleep
//! anyway.
//!
//! **Why a delay inhibitor.** logind emits `PrepareForSleep(true)` and then
//! suspends; without holding it off, the DRM commit races the suspend and may
//! land after the panel has already lost power. A `delay` inhibitor makes logind
//! wait for the fd to close, so the blank is applied before the system goes
//! down. logind caps that wait (`InhibitDelayMaxSec`, 5s by default) and we ack
//! in a single ioctl, so this cannot hold up a suspend for long — and
//! [`ACK_TIMEOUT`] drops the inhibitor even if the compositor never answers.
//!
//! Everything here degrades to "the old behaviour": no system bus, no logind, or
//! a refused inhibit all just mean the panel is not blanked before sleep.

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

/// D-Bus call timeout, matching [`crate::sensor`]: local service, off the render
/// thread, but a hung call must not wedge the worker for good.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the worker parks in `process` before re-checking its own state.
const POLL: Duration = Duration::from_millis(200);
/// How long to hold up the suspend waiting for the compositor to confirm the
/// blank. Well under logind's `InhibitDelayMaxSec` so a wedged compositor
/// delays the suspend rather than having logind override us.
const ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// What the worker tells the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The system is about to suspend. Blank, then ack: the suspend is being
    /// held off until the ack lands or [`ACK_TIMEOUT`] passes.
    AboutToSleep,
}

/// Handle to the sleep worker. Dropping it stops the thread.
pub struct Sleep {
    /// Loop source carrying [`Event`]s. Taken once, when inserting into the
    /// event loop.
    pub events: calloop::channel::Channel<Event>,
    /// Sender the compositor acks on, releasing the inhibitor.
    pub acks: Sender<()>,
}

/// Start the sleep worker. `None` only when the thread itself cannot start.
///
/// Connecting happens *on the worker*: `State::new` is on the startup path and a
/// slow system bus must not stall the compositor coming up.
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

/// Take a `delay` inhibitor on sleep. `None` on any refusal, which just means
/// the blank races the suspend instead of preceding it.
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

/// The worker: hold a delay inhibitor, and on every `PrepareForSleep(true)` ask
/// the compositor to blank before letting the suspend proceed.
fn worker(tx: &calloop::channel::Sender<Event>, rx_ack: &Receiver<()>) {
    let conn = match Connection::new_system() {
        Ok(c) => c,
        Err(e) => {
            debug!(target: "springchick::debug", %e, "no system bus; the panel will not blank before sleep");
            return;
        }
    };

    // The signal is handled out here rather than in the match callback: acking
    // blocks, and re-arming the inhibitor is a method call on the same
    // connection we would still be dispatching on.
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
                // The loop is gone, so the compositor is on its way out; there
                // is nothing left to blank and nothing to hold the suspend for.
                return;
            }
            match rx_ack.recv_timeout(ACK_TIMEOUT) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    warn!("compositor did not blank in time; suspending anyway");
                }
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Closing the fd is what lets logind proceed, so this drop is the
            // point of the whole exchange rather than mere tidying.
            drop(inhibitor.take());
        } else {
            // Resumed. The panel stays blanked and `Blank` agrees, so the first
            // press wakes the screen instead of toggling it off. Re-arm for the
            // next suspend: the inhibitor was consumed by the last one.
            debug!(target: "springchick::debug", "resumed; re-arming the sleep inhibitor");
            inhibitor = inhibit(&conn);
        }
    }
}
