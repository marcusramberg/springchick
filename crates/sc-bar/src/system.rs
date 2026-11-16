//! What the bar and the sheet show, read and changed the way Phosh does:
//! UPower for the battery, NetworkManager and ModemManager for the radios,
//! BlueZ, logind and sysfs for the backlight and the torch, feedbackd for
//! silent mode, power-profiles-daemon, MPRIS, `wpctl` for the volume,
//! `gsettings` for dark mode, and springchick's IPC for rotation and the
//! app shortcuts. A setting
//! whose service isn't there is left out.
//!
//! Airplane mode has no service of its own: it turns the Wi-Fi, mobile and
//! Bluetooth radios off, and is on while all of them are.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use dbus::arg::{prop_cast, PropMap, RefArg};
use dbus::blocking::stdintf::org_freedesktop_dbus::Properties;
use dbus::blocking::{Connection, Proxy};
use dbus::message::MatchRule;
use dbus::Path;

use crate::panel::{Change, Controls, Media, MediaAction, PowerAction, Slider, Toggle};

/// Read anyway this often, for services that started or stopped meanwhile.
const REREAD: Duration = Duration::from_secs(60);
/// Changes come in bursts; this collects one before reading.
const SETTLE: Duration = Duration::from_millis(50);
const STARTUP: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(2);

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_DEVICE_TYPE_WIFI: u32 = 2;
const NM_DEVICE_TYPE_MODEM: u32 = 8;
const MM: &str = "org.freedesktop.ModemManager1";
const MM_PATH: &str = "/org/freedesktop/ModemManager1";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const POWER_PROFILES: &str = "net.hadess.PowerProfiles";
const POWER_PROFILES_PATH: &str = "/net/hadess/PowerProfiles";
const FEEDBACK: &str = "org.sigxcpu.Feedback";
const FEEDBACK_PATH: &str = "/org/sigxcpu/Feedback";
const MPRIS: &str = "org.mpris.MediaPlayer2.";
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const OBJECT_MANAGER: &str = "org.freedesktop.DBus.ObjectManager";
const COLOR_SCHEME: [&str; 2] = ["org.gnome.desktop.interface", "color-scheme"];
const SINK: &str = "@DEFAULT_AUDIO_SINK@";

type Objects = HashMap<Path<'static>, HashMap<String, PropMap>>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct System {
    /// Percent, and whether it is plugged in. `None` without a battery.
    pub battery: Option<(f32, bool)>,
    /// Signal strength 0..100 while connected.
    pub wifi: Option<u8>,
    /// Signal quality 0..100 of a modem with an unlocked SIM.
    pub cellular: Option<u8>,
    /// App ids with a tile on the sheet, as the shell keeps them.
    pub shortcuts: Vec<String>,
    pub controls: Controls,
}

pub enum Msg {
    /// Something on a bus changed.
    Changed,
    /// The sheet is coming down.
    Opened,
    Do(Change),
}

/// Calls `send` with the system's state now and whenever it changes.
pub fn watch(send: impl Fn(System) + Send + 'static) -> mpsc::Sender<Msg> {
    let (tx, rx) = mpsc::channel();
    let changed = tx.clone();
    thread::spawn(move || {
        let buses = match (Connection::new_system(), Connection::new_session()) {
            (Ok(system), Ok(session)) => Buses { system, session },
            (Err(e), _) | (_, Err(e)) => {
                return eprintln!("sc-bar: no D-Bus ({e}); the bar shows only the time")
            }
        };
        let system = [
            UPOWER_PATH,
            NM_PATH,
            MM_PATH,
            "/org/bluez",
            POWER_PROFILES_PATH,
        ];
        for (bus, paths) in [
            (Connection::new_system(), &system[..]),
            (Connection::new_session(), &[FEEDBACK_PATH, MPRIS_PATH][..]),
        ] {
            if let Ok(bus) = bus {
                let changed = changed.clone();
                let paths = paths.to_vec();
                thread::spawn(move || forward(bus, &paths, changed));
            }
        }
        run(&buses, rx, send)
    });
    tx
}

/// Calls `send` when logind asks this session to lock (`true`, as `loginctl
/// lock-session` and the compositor's `lock_on_blank` do) or unlock.
pub fn watch_lock(send: impl Fn(bool) + Send + 'static) {
    const LOGIN: &str = "org.freedesktop.login1";
    thread::spawn(move || {
        let Ok(bus) = Connection::new_system() else {
            return;
        };
        let proxy = |path| bus.with_proxy(LOGIN, path, Duration::from_secs(5));
        // The signals come from the session's own path, not `auto`.
        let path = proxy("/org/freedesktop/login1/session/auto")
            .get::<String>("org.freedesktop.login1.Session", "Id")
            .and_then(|id| {
                proxy("/org/freedesktop/login1").method_call::<(dbus::Path,), _, _, _>(
                    "org.freedesktop.login1.Manager",
                    "GetSession",
                    (id,),
                )
            });
        let path = match path {
            Ok((path,)) => path,
            Err(e) => return eprintln!("sc-bar: no logind session ({e}); nothing asks to lock"),
        };
        let rule = MatchRule::new()
            .with_type(dbus::MessageType::Signal)
            // Anyone on the bus may send a signal: only logind's count.
            .with_sender(LOGIN)
            .with_interface("org.freedesktop.login1.Session")
            .with_path(path);
        let _ = bus.add_match(rule, move |_: (), _, msg| {
            match msg.member().as_deref() {
                Some("Lock") => send(true),
                Some("Unlock") => send(false),
                _ => {}
            }
            true
        });
        while bus.process(Duration::from_secs(3600)).is_ok() {}
        eprintln!("sc-bar: lost the system bus; nothing asks to lock");
    });
}

/// Signals `changed` for every property change under `paths`, and for names
/// coming and going (media players).
fn forward(bus: Connection, paths: &[&'static str], changed: mpsc::Sender<Msg>) {
    let mut rules = vec![MatchRule::new_signal(
        "org.freedesktop.DBus",
        "NameOwnerChanged",
    )];
    for &path in paths {
        let mut rule =
            MatchRule::new_signal("org.freedesktop.DBus.Properties", "PropertiesChanged");
        rule.path = Some(path.into());
        rule.path_is_namespace = true;
        rules.push(rule);
    }
    for rule in rules {
        let changed = changed.clone();
        let _ = bus.add_match(rule, move |_: (), _, msg| {
            // Every client of the bus comes and goes; only players matter.
            let other_name = msg.member().is_some_and(|m| &*m == "NameOwnerChanged")
                && !msg
                    .get1::<&str>()
                    .is_some_and(|n| n.starts_with("org.mpris."));
            other_name || changed.send(Msg::Changed).is_ok()
        });
    }
    while bus.process(Duration::from_secs(3600)).is_ok() {}
}

struct Buses {
    system: Connection,
    session: Connection,
}

impl Buses {
    fn sys<'a>(&'a self, service: &'a str, path: &'a str) -> Proxy<'a, &'a Connection> {
        self.system.with_proxy(service, path, TIMEOUT)
    }
    fn user<'a>(&'a self, service: &'a str, path: &'a str) -> Proxy<'a, &'a Connection> {
        self.session.with_proxy(service, path, TIMEOUT)
    }
}

fn run(buses: &Buses, rx: mpsc::Receiver<Msg>, send: impl Fn(System)) {
    let mut last = None;
    // Sends what there is, when it is news.
    let publish = |tools: &Tools, last: &mut Option<System>| {
        let system = read(buses, tools);
        if last.as_ref() != Some(&system) {
            send(system.clone());
            *last = Some(system);
        }
    };
    // Read when the sheet comes down and after a change of ours, never on a
    // timer: it forks, and the sheet may stay open.
    let mut tools = Tools::default();
    tools.read_shell();
    // The bar is waiting too.
    publish(&tools, &mut last);
    tools.read_forked();
    let mut reread = true;
    // Once more soon: the compositor may not have been answering yet.
    let mut wait = STARTUP;
    loop {
        if reread {
            publish(&tools, &mut last);
        }
        let first = match rx.recv_timeout(wait) {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
                wait = REREAD;
                tools.read_shell();
                reread = true;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        thread::sleep(SETTLE);
        let batch: Vec<Msg> = std::iter::once(first).chain(rx.try_iter()).collect();
        // A slider's values that a newer one replaced are skipped.
        let latest = |s: Slider| {
            batch
                .iter()
                .rposition(|m| matches!(m, Msg::Do(Change::Slider(x, _)) if *x == s))
        };
        reread = false;
        let mut refresh = false;
        for (i, msg) in batch.iter().enumerate() {
            let change = match *msg {
                Msg::Changed => {
                    reread = true;
                    continue;
                }
                Msg::Opened => {
                    refresh = true;
                    continue;
                }
                Msg::Do(Change::Slider(s, _)) if latest(s) != Some(i) => continue,
                Msg::Do(change) => change,
            };
            if let Err(e) = change_one(buses, change) {
                eprintln!("sc-bar: {change:?} failed: {e}");
            }
            match change {
                // The sheet shows the slider where the finger left it; reading
                // it back for every step of a drag is what made dragging slow.
                Change::Slider(Slider::Volume, value) => tools.volume = Some(value),
                Change::Slider(Slider::Brightness, _) => {}
                _ => {
                    refresh = true;
                    // Answer even when nothing came of it: the sheet already shows it done.
                    last = None;
                }
            }
        }
        if refresh {
            // The sheet is waiting: what is quick goes out before what forks.
            tools.read_shell();
            publish(&tools, &mut last);
            tools.read_forked();
            reread = true;
        }
    }
}

#[derive(Default)]
struct Tools {
    volume: Option<f32>,
    dark_mode: Option<bool>,
    rotation_lock: Option<bool>,
    shortcuts: Vec<String>,
}

impl Tools {
    /// What springchick's IPC answers: quick, so it goes first.
    fn read_shell(&mut self) {
        self.rotation_lock = crate::ipc("rotation")
            .map(|r| r.starts_with("disabled"))
            .or(self.rotation_lock);
        // A slow answer is not "none".
        if let Some(ids) = crate::ipc("shortcuts") {
            self.shortcuts = ids.split_whitespace().map(str::to_string).collect();
        }
    }

    /// What takes a process each to ask; seconds, where the service is down.
    fn read_forked(&mut self) {
        self.volume = volume();
        self.dark_mode = tool("gsettings", &["get", COLOR_SCHEME[0], COLOR_SCHEME[1]])
            .map(|scheme| scheme.trim() == "'prefer-dark'");
    }
}

fn read(buses: &Buses, tools: &Tools) -> System {
    let (wifi_on, cellular_on) = radios(buses).unwrap_or_default();
    let bluetooth = bluetooth(buses).ok().flatten().map(|(_, powered)| powered);
    let any_radio = wifi_on.is_some() || cellular_on.is_some();
    let airplane = any_radio
        .then(|| wifi_on != Some(true) && cellular_on != Some(true) && bluetooth != Some(true));
    let mut controls = Controls {
        brightness: Light::backlight().and_then(|l| l.value()),
        volume: tools.volume,
        media: media(buses).ok().flatten(),
        shortcuts: tools.shortcuts.len(),
        ..Controls::default()
    };
    for (toggle, on) in [
        (Toggle::Airplane, airplane),
        (Toggle::Wifi, wifi_on),
        (Toggle::Cellular, cellular_on),
        (Toggle::Bluetooth, bluetooth),
        (Toggle::RotationLock, tools.rotation_lock),
        (
            Toggle::Silent,
            buses
                .user(FEEDBACK, FEEDBACK_PATH)
                .get::<String>(FEEDBACK, "Profile")
                .ok()
                .map(|p| p == "silent"),
        ),
        (Toggle::DarkMode, tools.dark_mode),
        (
            Toggle::LowPower,
            buses
                .sys(POWER_PROFILES, POWER_PROFILES_PATH)
                .get::<String>(POWER_PROFILES, "ActiveProfile")
                .ok()
                .map(|p| p == "power-saver"),
        ),
        (Toggle::Torch, Light::torch().map(|l| l.level() > 0)),
    ] {
        controls.toggles[toggle as usize] = on;
    }
    System {
        battery: battery(buses).ok().flatten(),
        wifi: wifi(buses).ok().flatten(),
        cellular: cellular(buses).ok().flatten(),
        shortcuts: tools.shortcuts.clone(),
        controls,
    }
}

fn battery(buses: &Buses) -> Result<Option<(f32, bool)>, dbus::Error> {
    const DEVICE: &str = "org.freedesktop.UPower.Device";
    let device = buses.sys(
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower/devices/DisplayDevice",
    );
    if !device.get::<bool>(DEVICE, "IsPresent")? {
        return Ok(None);
    }
    let percent = device.get::<f64>(DEVICE, "Percentage")? as f32;
    // Charging, fully charged, or plugged in but not charging.
    let plugged_in = matches!(device.get::<u32>(DEVICE, "State")?, 1 | 4 | 5);
    Ok(Some((percent, plugged_in)))
}

fn nm_devices(buses: &Buses) -> Result<Vec<(Path<'static>, u32)>, dbus::Error> {
    let (paths,): (Vec<Path<'static>>,) =
        buses.sys(NM, NM_PATH).method_call(NM, "GetDevices", ())?;
    paths
        .into_iter()
        .map(|path| {
            let kind = buses
                .sys(NM, &path)
                .get("org.freedesktop.NetworkManager.Device", "DeviceType")?;
            Ok((path, kind))
        })
        .collect()
}

/// Whether the Wi-Fi and mobile radios are on, for the kinds of device there are.
fn radios(buses: &Buses) -> Result<(Option<bool>, Option<bool>), dbus::Error> {
    let devices = nm_devices(buses)?;
    let radio = |kind: u32, property: &str| -> Result<Option<bool>, dbus::Error> {
        if devices.iter().any(|(_, k)| *k == kind) {
            Ok(Some(buses.sys(NM, NM_PATH).get(NM, property)?))
        } else {
            Ok(None)
        }
    };
    Ok((
        radio(NM_DEVICE_TYPE_WIFI, "WirelessEnabled")?,
        radio(NM_DEVICE_TYPE_MODEM, "WwanEnabled")?,
    ))
}

/// The connected access point's strength.
fn wifi(buses: &Buses) -> Result<Option<u8>, dbus::Error> {
    for (path, kind) in nm_devices(buses)? {
        if kind != NM_DEVICE_TYPE_WIFI {
            continue;
        }
        let ap: Path = buses.sys(NM, &path).get(
            "org.freedesktop.NetworkManager.Device.Wireless",
            "ActiveAccessPoint",
        )?;
        if &*ap != "/" {
            return buses
                .sys(NM, &ap)
                .get("org.freedesktop.NetworkManager.AccessPoint", "Strength")
                .map(Some);
        }
    }
    Ok(None)
}

fn cellular(buses: &Buses) -> Result<Option<u8>, dbus::Error> {
    const MODEM: &str = "org.freedesktop.ModemManager1.Modem";
    /// MM_MODEM_LOCK_NONE; higher values ask for a PIN or PUK.
    const LOCK_NONE: u32 = 1;
    let (objects,): (Objects,) =
        buses
            .sys(MM, MM_PATH)
            .method_call(OBJECT_MANAGER, "GetManagedObjects", ())?;
    // The strongest: a second, failed modem (the unused SIM slot) reports 0,
    // and the map's order changes from read to read.
    let quality = |path: &Path| -> Result<Option<u8>, dbus::Error> {
        let modem = buses.sys(MM, path);
        let sim: Path = modem.get(MODEM, "Sim")?;
        if &*sim == "/" || modem.get::<u32>(MODEM, "UnlockRequired")? > LOCK_NONE {
            return Ok(None);
        }
        let (quality, _recent): (u32, bool) = modem.get(MODEM, "SignalQuality")?;
        Ok(Some(quality.min(100) as u8))
    };
    Ok(objects
        .keys()
        .filter_map(|path| quality(path).ok().flatten())
        .max())
}

/// BlueZ's first adapter, and whether it's powered.
fn bluetooth(buses: &Buses) -> Result<Option<(Path<'static>, bool)>, dbus::Error> {
    let (objects,): (Objects,) =
        buses
            .sys("org.bluez", "/")
            .method_call(OBJECT_MANAGER, "GetManagedObjects", ())?;
    Ok(objects.into_iter().find_map(|(path, interfaces)| {
        let powered = prop_cast::<bool>(interfaces.get("org.bluez.Adapter1")?, "Powered")?;
        Some((path, *powered))
    }))
}

fn player(buses: &Buses) -> Result<Option<String>, dbus::Error> {
    let (names,): (Vec<String>,) = buses
        .user("org.freedesktop.DBus", "/org/freedesktop/DBus")
        .method_call("org.freedesktop.DBus", "ListNames", ())?;
    Ok(names.into_iter().find(|name| name.starts_with(MPRIS)))
}

/// The first MPRIS player on the bus, and what it's playing.
fn media(buses: &Buses) -> Result<Option<Media>, dbus::Error> {
    let Some(name) = player(buses)? else {
        return Ok(None);
    };
    let player = buses.user(&name, MPRIS_PATH);
    let status: String = player.get(MPRIS_PLAYER, "PlaybackStatus")?;
    let metadata: PropMap = player.get(MPRIS_PLAYER, "Metadata")?;
    let artist = metadata
        .get("xesam:artist")
        .and_then(|v| v.0.as_iter())
        .map(|artists| {
            let names: Vec<&str> = artists.filter_map(|a| a.as_str()).collect();
            names.join(", ")
        })
        .unwrap_or_default();
    Ok(Some(Media {
        title: prop_cast::<String>(&metadata, "xesam:title")
            .cloned()
            .unwrap_or_default(),
        artist,
        playing: status == "Playing",
    }))
}

fn tool(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The default sink's volume, from `wpctl get-volume`'s `Volume: 0.40`.
fn volume() -> Option<f32> {
    let text = tool("wpctl", &["get-volume", SINK])?;
    let value = text
        .trim()
        .strip_prefix("Volume:")?
        .split_whitespace()
        .next()?;
    value.parse::<f32>().ok().map(|v| v.clamp(0.0, 1.0))
}

/// A backlight or LED in sysfs: read there, set through logind, which lets
/// the session's user.
struct Light {
    dir: PathBuf,
    max: u32,
    min: u32,
    /// The slider maps logarithmically onto the levels, as perceived
    /// brightness does, unless the driver says its scale already is (Phosh's
    /// `brightness-manager.c`).
    logarithmic: bool,
}

impl Light {
    fn backlight() -> Option<Light> {
        let dir = std::fs::read_dir("/sys/class/backlight")
            .ok()?
            .flatten()
            .next()?
            .path();
        let max: u32 = number(&dir.join("max_brightness"))?;
        let raw = std::fs::read_to_string(dir.join("type")).is_ok_and(|t| t.trim() == "raw");
        let min = if raw && max < 99 {
            0
        } else {
            (max / 100).max(1)
        };
        let scale = std::fs::read_to_string(dir.join("scale")).unwrap_or_default();
        Some(Light {
            dir,
            max,
            min,
            logarithmic: scale.trim() != "non-linear",
        })
    }

    /// The camera flash, where the kernel names it so.
    fn torch() -> Option<Light> {
        let dir = std::fs::read_dir("/sys/class/leds")
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.to_str()
                    .is_some_and(|n| n.contains("torch") || n.contains("flash"))
            })?;
        let max = number(&dir.join("max_brightness"))?;
        Some(Light {
            dir,
            max,
            min: 0,
            logarithmic: false,
        })
    }

    fn level(&self) -> u32 {
        number(&self.dir.join("brightness")).unwrap_or(0)
    }

    fn value(&self) -> Option<f32> {
        let level = number(&self.dir.join("brightness"))?.clamp(self.min, self.max) as f32;
        let (min, max) = (self.min.max(1) as f32, self.max as f32);
        let value = if self.logarithmic {
            (level.max(1.0).log10() - min.log10()) / (max.log10() - min.log10())
        } else {
            (level - self.min as f32) / (max - self.min as f32)
        };
        // A one-step backlight has no range to be anywhere in.
        Some(if value.is_nan() {
            1.0
        } else {
            value.clamp(0.0, 1.0)
        })
    }

    fn level_for(&self, value: f32) -> u32 {
        let (min, max) = (self.min.max(1) as f32, self.max as f32);
        let level = if self.logarithmic {
            10f32.powf(min.log10() + (max.log10() - min.log10()) * value)
        } else {
            self.min as f32 + (max - self.min as f32) * value
        };
        (level.round() as u32).clamp(self.min, self.max)
    }

    fn set(&self, buses: &Buses, subsystem: &str, level: u32) -> Result<(), dbus::Error> {
        let Some(name) = self.dir.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        buses
            .sys(
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/auto",
            )
            .method_call(
                "org.freedesktop.login1.Session",
                "SetBrightness",
                (subsystem, name, level),
            )
    }
}

fn number(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn change_one(buses: &Buses, change: Change) -> Result<(), dbus::Error> {
    let nm = buses.sys(NM, NM_PATH);
    let set_bluetooth = |on: bool| -> Result<(), dbus::Error> {
        match bluetooth(buses)? {
            Some((path, _)) => {
                buses
                    .sys("org.bluez", &path)
                    .set("org.bluez.Adapter1", "Powered", on)
            }
            None => Ok(()),
        }
    };
    let run = |program: &str, args: &[&str]| {
        if tool(program, args).is_none() {
            eprintln!("sc-bar: {program} {args:?} failed");
        }
    };
    match change {
        Change::Toggle(Toggle::Wifi, on) => nm.set(NM, "WirelessEnabled", on)?,
        Change::Toggle(Toggle::Cellular, on) => nm.set(NM, "WwanEnabled", on)?,
        Change::Toggle(Toggle::Bluetooth, on) => set_bluetooth(on)?,
        Change::Toggle(Toggle::Airplane, on) => {
            nm.set(NM, "WirelessEnabled", !on)?;
            nm.set(NM, "WwanEnabled", !on)?;
            if on {
                set_bluetooth(false)?;
            }
        }
        Change::Toggle(Toggle::RotationLock, on) => {
            crate::ipc(if on {
                "rotation disable"
            } else {
                "rotation enable"
            });
        }
        Change::Toggle(Toggle::Silent, on) => {
            let profile = if on { "silent" } else { "full" };
            buses
                .user(FEEDBACK, FEEDBACK_PATH)
                .set(FEEDBACK, "Profile", profile)?;
        }
        Change::Toggle(Toggle::DarkMode, on) => {
            let scheme = if on { "prefer-dark" } else { "default" };
            run(
                "gsettings",
                &["set", COLOR_SCHEME[0], COLOR_SCHEME[1], scheme],
            );
        }
        Change::Toggle(Toggle::LowPower, on) => {
            let profile = if on { "power-saver" } else { "balanced" };
            buses.sys(POWER_PROFILES, POWER_PROFILES_PATH).set(
                POWER_PROFILES,
                "ActiveProfile",
                profile,
            )?;
        }
        Change::Toggle(Toggle::Torch, on) => {
            if let Some(torch) = Light::torch() {
                torch.set(buses, "leds", if on { torch.max } else { 0 })?;
            }
        }
        Change::Slider(Slider::Brightness, value) => {
            if let Some(backlight) = Light::backlight() {
                backlight.set(buses, "backlight", backlight.level_for(value))?;
            }
        }
        Change::Slider(Slider::Volume, value) => {
            run("wpctl", &["set-volume", SINK, &format!("{value:.3}")]);
        }
        Change::Media(action) => {
            let Some(name) = player(buses)? else {
                return Ok(());
            };
            let method = match action {
                MediaAction::Previous => "Previous",
                MediaAction::PlayPause => "PlayPause",
                MediaAction::Next => "Next",
            };
            buses
                .user(&name, MPRIS_PATH)
                .method_call::<(), _, _, _>(MPRIS_PLAYER, method, ())?;
        }
        Change::Power(action) => {
            let method = match action {
                PowerAction::Sleep => "Suspend",
                PowerAction::Restart => "Reboot",
                PowerAction::PowerOff => "PowerOff",
                // The compositor's, through `main`.
                PowerAction::LogOut => return Ok(()),
            };
            // Not interactive: no polkit prompt has anywhere to show.
            buses
                .sys("org.freedesktop.login1", "/org/freedesktop/login1")
                .method_call::<(), _, _, _>("org.freedesktop.login1.Manager", method, (false,))?;
        }
        // `main`'s.
        Change::Launch(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backlight_maps_the_slider_logarithmically_and_back() {
        let light = Light {
            dir: PathBuf::new(),
            max: 3628,
            min: 36,
            logarithmic: true,
        };
        assert_eq!(light.level_for(0.0), 36);
        assert_eq!(light.level_for(1.0), 3628);
        let middle = light.level_for(0.5);
        assert!(middle > 300 && middle < 400, "{middle}");
    }
}
