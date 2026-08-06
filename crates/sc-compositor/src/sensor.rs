//! Device orientation from iio-sensor-proxy, over the system D-Bus.
//!
//! The compositor never blocks on D-Bus: a worker thread owns the connection
//! and posts [`DeviceOrientation`] changes down a channel, which both frame
//! loops drain once a tick ([`crate::State::drain_sensor`]) — the same shape as
//! [`crate::debug_input`], and for the same reason.
//!
//! **The accelerometer is only claimed while it can matter.** iio-sensor-proxy
//! powers the sensor on for as long as anyone holds a claim, so claiming for the
//! whole session would burn battery to answer a question that only a fullscreen
//! app ever asks. The claim follows fullscreen instead, via
//! [`Sensor::set_wanted`].
//!
//! Everything here degrades to "no rotation" rather than failing: no sensor, no
//! proxy, or a polkit refusal all just mean the orientation stays
//! [`DeviceOrientation::Normal`], which is exactly how a device without an
//! accelerometer behaves.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use dbus::arg::{RefArg, Variant};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::Connection;
use dbus::message::MatchRule;

use tracing::{debug, info, warn};

use crate::rotation::DeviceOrientation;

const SERVICE: &str = "net.hadess.SensorProxy";
const PATH: &str = "/net/hadess/SensorProxy";
const IFACE: &str = "net.hadess.SensorProxy";
/// D-Bus call timeout. Generous: the proxy is local and this is off the render
/// thread, but a hung call must not wedge the worker for good.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the worker parks in `process` before re-checking its command queue.
/// Short enough that claim/release follows fullscreen promptly, long enough that
/// an idle worker costs nothing measurable.
const POLL: Duration = Duration::from_millis(200);

/// What the compositor asks the worker to do.
enum Cmd {
    /// Claim or release the accelerometer.
    Wanted(bool),
    /// Shut down (releasing first, if claimed).
    Stop,
}

/// Handle to the sensor worker. Dropping it stops the thread.
pub struct Sensor {
    orientations: Receiver<DeviceOrientation>,
    commands: Sender<Cmd>,
    /// Last value sent to the worker, so repeated fullscreen churn doesn't
    /// bounce the claim.
    wanted: bool,
}

impl Sensor {
    /// Claim the accelerometer (or release it) — called as apps enter and leave
    /// fullscreen. Cheap and idempotent.
    pub fn set_wanted(&mut self, wanted: bool) {
        if wanted == self.wanted {
            return;
        }
        self.wanted = wanted;
        // A dead worker is not an error: it means there is no usable sensor, and
        // the compositor carries on unrotated.
        let _ = self.commands.send(Cmd::Wanted(wanted));
    }

    /// The most recent orientation the sensor reported, if it changed since the
    /// last drain. Older readings in the queue are discarded — only where the
    /// device is *now* matters.
    pub fn latest(&self) -> Option<DeviceOrientation> {
        let mut newest = None;
        loop {
            match self.orientations.try_recv() {
                Ok(o) => newest = Some(o),
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return newest,
            }
        }
    }
}

impl Drop for Sensor {
    fn drop(&mut self) {
        let _ = self.commands.send(Cmd::Stop);
    }
}

/// Start the sensor worker. `None` only when the thread itself cannot start.
///
/// Connecting and probing happen *on the worker*, not here: `State::new` is on
/// the startup path, and a system bus that is slow to answer (or activating
/// iio-sensor-proxy on demand) would stall the compositor coming up. So this
/// returns a handle immediately and the worker decides whether there is anything
/// to report; if there isn't, it exits and the handle goes quiet forever.
pub fn spawn() -> Option<Sensor> {
    let (tx_orientation, orientations) = std::sync::mpsc::channel();
    let (commands, rx_cmd) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("sc-sensor".into())
        .spawn(move || worker(&tx_orientation, &rx_cmd))
        .map_err(|e| warn!(%e, "could not start sensor thread"))
        .ok()?;

    Some(Sensor {
        orientations,
        commands,
        wanted: false,
    })
}

/// Whether the proxy is there and has an accelerometer. Any error — service not
/// running, property missing — reads as "no".
fn has_accelerometer(conn: &Connection) -> bool {
    let proxy = conn.with_proxy(SERVICE, PATH, CALL_TIMEOUT);
    proxy
        .get::<bool>(IFACE, "HasAccelerometer")
        .unwrap_or(false)
}

/// Read `AccelerometerOrientation`. Unreadable reads as `Undefined`, which does
/// not rotate.
fn read_orientation(conn: &Connection) -> DeviceOrientation {
    let proxy = conn.with_proxy(SERVICE, PATH, CALL_TIMEOUT);
    match proxy.get::<String>(IFACE, "AccelerometerOrientation") {
        Ok(s) => DeviceOrientation::from_sensor(&s),
        Err(e) => {
            debug!(%e, "could not read AccelerometerOrientation");
            DeviceOrientation::Undefined
        }
    }
}

/// Claim the accelerometer. Returns whether the claim was granted.
///
/// A refusal is expected rather than exceptional: the polkit action is
/// `allow_active`, so only a session on an active seat may claim. springchick
/// running as the session compositor qualifies; the same binary started over SSH
/// does not, and gets `AccessDenied`. Either way the answer is "carry on without
/// rotation", so this is logged once at info and never retried in a loop.
fn claim(conn: &Connection) -> bool {
    let proxy = conn.with_proxy(SERVICE, PATH, CALL_TIMEOUT);
    match proxy.method_call::<(), _, _, _>(IFACE, "ClaimAccelerometer", ()) {
        Ok(()) => true,
        Err(e) => {
            info!(%e, "accelerometer claim refused; rotation stays portrait");
            false
        }
    }
}

fn release(conn: &Connection) {
    let proxy = conn.with_proxy(SERVICE, PATH, CALL_TIMEOUT);
    if let Err(e) = proxy.method_call::<(), _, _, _>(IFACE, "ReleaseAccelerometer", ()) {
        debug!(%e, "releasing the accelerometer failed");
    }
}

/// The worker: connect, decide whether there is a sensor worth listening to,
/// then hold the claim while it is wanted and forward every orientation change.
///
/// Returning early is the normal path on anything without an accelerometer (a
/// dev box, the VM), so it is logged at debug rather than as a failure.
fn worker(tx: &Sender<DeviceOrientation>, rx: &Receiver<Cmd>) {
    let conn = match Connection::new_system() {
        Ok(c) => c,
        Err(e) => {
            debug!(target: "springchick::debug", %e, "no system bus; device orientation unavailable");
            return;
        }
    };
    if !has_accelerometer(&conn) {
        debug!(target: "springchick::debug", "iio-sensor-proxy reports no accelerometer; rotation stays portrait");
        return;
    }
    info!(target: "springchick::debug", "accelerometer available; rotation follows the device");
    run(conn, tx, rx);
}

/// The claim/listen loop, once there is a sensor to talk to.
fn run(conn: Connection, tx: &Sender<DeviceOrientation>, rx: &Receiver<Cmd>) {
    // PropertiesChanged carries the new orientation without a round trip. The
    // match is added once and left in place; it costs nothing while unclaimed
    // because the proxy only emits changes while someone is listening.
    let rule = MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged")
        .with_path(PATH);
    let tx_signal = tx.clone();
    let matched = conn.add_match(
        rule,
        move |(_, changed, _): (String, PropMap, Vec<String>), _, _| {
            if let Some(v) = changed.get("AccelerometerOrientation") {
                if let Some(s) = v.0.as_str() {
                    let o = DeviceOrientation::from_sensor(s);
                    debug!(target: "springchick::debug", ?o, "sensor reported orientation");
                    let _ = tx_signal.send(o);
                }
            }
            true
        },
    );
    if let Err(e) = matched {
        warn!(%e, "could not subscribe to sensor changes; orientation will not update");
        return;
    }

    let mut claimed = false;
    loop {
        match rx.try_recv() {
            Ok(Cmd::Wanted(true)) if !claimed => {
                claimed = claim(&conn);
                if claimed {
                    // The signal only fires on *changes*, so seed the current
                    // value — the device may already be on its side.
                    let _ = tx.send(read_orientation(&conn));
                }
            }
            Ok(Cmd::Wanted(false)) if claimed => {
                release(&conn);
                claimed = false;
                // Nothing will report orientation now, so stop claiming to know
                // it: an unrotated app is the right answer while unclaimed.
                let _ = tx.send(DeviceOrientation::Normal);
            }
            Ok(Cmd::Wanted(_)) => {}
            Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
        // Blocks up to POLL, dispatching any signals that arrive.
        if let Err(e) = conn.process(POLL) {
            warn!(%e, "sensor connection error; giving up on orientation");
            break;
        }
    }
    if claimed {
        release(&conn);
    }
    debug!("sensor thread stopped");
}

/// The `a{sv}` payload of `PropertiesChanged`.
type PropMap = std::collections::HashMap<String, Variant<Box<dyn RefArg>>>;
