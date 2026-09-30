//! Device orientation from iio-sensor-proxy over D-Bus, on a worker thread
//! drained once a tick. The accelerometer is only claimed while an app is
//! fullscreen: a claim keeps the sensor powered. Any failure means no rotation.

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
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Worker park time between command-queue checks.
const POLL: Duration = Duration::from_millis(200);

enum Cmd {
    Wanted(bool),
    /// Release first, if claimed.
    Stop,
}

pub struct Sensor {
    orientations: Receiver<DeviceOrientation>,
    commands: Sender<Cmd>,
    /// Last value sent, so fullscreen churn doesn't bounce the claim.
    wanted: bool,
}

impl Sensor {
    /// Idempotent.
    pub fn set_wanted(&mut self, wanted: bool) {
        if wanted == self.wanted {
            return;
        }
        self.wanted = wanted;
        // A dead worker just means no sensor.
        let _ = self.commands.send(Cmd::Wanted(wanted));
    }

    /// Newest orientation since the last drain; older readings are dropped.
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

/// Connects on the worker: a slow system bus (or on-demand activation of
/// iio-sensor-proxy) must not stall startup.
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

fn has_accelerometer(conn: &Connection) -> bool {
    let proxy = conn.with_proxy(SERVICE, PATH, CALL_TIMEOUT);
    proxy
        .get::<bool>(IFACE, "HasAccelerometer")
        .unwrap_or(false)
}

/// Unreadable reads as `Undefined`, which does not rotate.
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

/// The polkit action is `allow_active`: an SSH-started compositor gets
/// `AccessDenied`. Logged once, never retried.
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

fn run(conn: Connection, tx: &Sender<DeviceOrientation>, rx: &Receiver<Cmd>) {
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
                    // The signal only fires on changes; seed the current value.
                    let _ = tx.send(read_orientation(&conn));
                }
            }
            Ok(Cmd::Wanted(false)) if claimed => {
                release(&conn);
                claimed = false;
                // Unclaimed: report unrotated.
                let _ = tx.send(DeviceOrientation::Normal);
            }
            Ok(Cmd::Wanted(_)) => {}
            Ok(Cmd::Stop) | Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {}
        }
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

type PropMap = std::collections::HashMap<String, Variant<Box<dyn RefArg>>>;
