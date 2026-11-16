//! springchick's status bar and the sheet pulled down from it: the controls
//! on the right half, Notification Center on the left. Layer surfaces: the
//! bar holds its strip at the top, the sheet covers the screen only while it
//! is pulled or open, and a banner's strip is there while one shows. It is
//! the notification daemon too, when no other is, and the lock screen, when
//! logind asks for one. Standalone: any other bar can replace it.

mod center;
mod draw;
mod fprint;
mod lock;
mod notify;
mod pam;
mod panel;
mod symbols;
mod system;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Instant;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_dispatch2, delegate_registry,
    dispatch2::Dispatch2,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        touch::TouchHandler,
        Capability, SeatHandler, SeatState,
    },
    session_lock::{
        SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
        SessionLockSurfaceConfigure,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface, wl_touch},
    Connection, Proxy, QueueHandle,
};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1,
    ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use center::Center;
use panel::{Change, Page, Panel, BAR_H};
use system::{Msg, System};

/// A surface's fractional scale and the viewport that maps its buffer back.
struct Hidpi(WpViewport, WpFractionalScaleV1);

impl Drop for Hidpi {
    fn drop(&mut self) {
        self.0.destroy();
        self.1.destroy();
    }
}

struct Sheet {
    layer: LayerSurface,
    hidpi: Option<Hidpi>,
    blur: Option<ExtBackgroundEffectSurfaceV1>,
    /// Logical px; 0 until configured.
    w: u32,
    h: u32,
    frame_pending: bool,
    dirty: bool,
    /// More than the sheet's edge changed since the last draw.
    repaint: bool,
    /// The sheet all the way down, as last drawn; empty when out of date.
    open: Vec<u8>,
    /// Buffer px.
    drawn_edge: i32,
    /// The last draw had the modules swung past the bottom.
    bounced: bool,
    /// Down over the whole screen, and the compositor told so.
    full: bool,
}

/// A banner's own strip under the bar, there while one shows.
struct BannerSurface {
    layer: LayerSurface,
    hidpi: Option<Hidpi>,
    /// Logical px; `w` is 0 until configured.
    w: u32,
    h: u32,
    frame_pending: bool,
    dirty: bool,
}

/// The lock screen, there from the request until the PIN is right.
struct Locked {
    hidpi: Option<Hidpi>,
    surface: SessionLockSurface,
    session: SessionLock,
    /// Logical px; 0 until configured.
    w: u32,
    h: u32,
    frame_pending: bool,
    dirty: bool,
    pad: lock::Lock,
    /// Unlocked before the compositor said `locked`: it goes then.
    leaving: bool,
    sensor: Sensor,
}

/// The fingerprint sensor, for one showing of the lock screen.
#[derive(Clone, Copy, PartialEq)]
enum Sensor {
    /// Not asked yet: the panel may be dark.
    Idle,
    Asked,
    Reading,
    /// A finger it did not know, or could not read.
    Missed,
    /// None to read, or too many misses.
    Off,
}

/// There while locked: a bar restarted under its own lock takes it up again.
fn lock_marker() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("sc-bar.locked"))
}

/// What reaches the main loop from the other threads.
enum Update {
    System(System),
    /// logind asks the session to lock, or unlock.
    Lock(bool),
    /// What PAM made of the PIN, and which showing of the lock screen took it.
    Auth(u32, bool),
    Fprint(fprint::Event),
    Notify(notify::Event),
    /// The icons of these shortcuts, in their order.
    Shortcuts(Vec<String>, Vec<sc_icons::IconPixels>),
    NoteIcon(String, sc_icons::IconPixels),
}

/// What the icon thread is asked to find.
enum Icons {
    Shortcuts(Vec<String>),
    /// A notification's, by `draw::icon_key`: its app's, else the one named.
    Note {
        key: String,
        app: Option<String>,
        icon: String,
        app_name: String,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum Target {
    Bar,
    Sheet,
    Banner,
    Lock,
}

/// The one finger followed: where it went down, where and when it last
/// moved, and how fast (px/s).
struct Finger {
    id: i32,
    target: Target,
    at: f64,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    qh: QueueHandle<App>,
    pool: Option<SlotPool>,
    blur: Option<ExtBackgroundEffectManagerV1>,
    /// With the viewporter: springchick sends its scale no other way.
    fractional: Option<(WpFractionalScaleManagerV1, WpViewporter)>,
    bar: LayerSurface,
    bar_hidpi: Option<Hidpi>,
    /// Logical px; 0 until configured.
    bar_w: u32,
    sheet: Option<Sheet>,
    panel: Option<Panel>,
    scale: f32,
    font: skia_safe::Font,
    theme: sc_config::Theme,
    /// One per `system.shortcuts`.
    icons: Vec<skia_safe::Image>,
    /// `[bar.long_press]`.
    long_press: Vec<(panel::Toggle, String)>,
    long_press_secs: f64,
    /// When the finger resting on the sheet has been there long enough.
    hold: Option<f64>,
    system: System,
    worker: mpsc::Sender<Msg>,
    clock: String,
    /// `Monday 16 November`, over Notification Center's clock.
    date: String,
    finger: Option<Finger>,
    epoch: Instant,
    last_tick: f64,
    center: Center,
    /// `None` when another notification daemon holds the name.
    notify: Option<notify::Handle>,
    haptics: Option<sc_haptics::Haptics>,
    banner: Option<BannerSurface>,
    /// When the banner showing goes back up by itself.
    banner_hide_at: Option<f64>,
    banner_tick: f64,
    /// Notifications' icons, by [`draw::icon_key`].
    /// `None` while the icon thread looks.
    note_icons: HashMap<String, Option<skia_safe::Image>>,
    /// Read once, for the icons of apps that notify.
    icon_jobs: mpsc::Sender<Icons>,
    session_lock: SessionLockState,
    locked: Option<Locked>,
    /// Counts the lock screen's showings: a PIN answers for its own only.
    showing: u32,
    /// The main loop's own ends, for the thread that asks PAM.
    updates: mpsc::Sender<Update>,
    wake: std::sync::Arc<UnixStream>,
    /// `[bar] lock`.
    lock_screen: bool,
    /// The thread that reads fingers, once the lock screen has shown.
    fprint: Option<mpsc::Sender<fprint::Cmd>>,
}

fn main() {
    let conn = Connection::connect_to_env().expect("no wayland compositor");
    let (globals, mut queue) = registry_queue_init(&conn).expect("registry");
    let qh = queue.handle();

    let (updates_tx, updates) = mpsc::channel();
    let (mut wake_rx, wake_tx) = UnixStream::pair().expect("socketpair");
    wake_rx.set_nonblocking(true).expect("nonblocking");
    // With the worker gone its end would read as hung up, forever.
    let (notes_tx, wake_open) = (updates_tx.clone(), wake_tx.try_clone().expect("dup"));
    let (lock_tx, lock_wake) = (updates_tx.clone(), wake_tx.try_clone().expect("dup"));
    let (auth_tx, auth_wake) = (updates_tx.clone(), wake_tx.try_clone().expect("dup"));
    let worker = system::watch(move |system| {
        let _ = updates_tx.send(Update::System(system));
        let _ = (&wake_tx).write(&[0]);
    });
    let (icon_jobs, jobs) = mpsc::channel();
    let (icons_tx, icons_wake) = (notes_tx.clone(), wake_open.try_clone().expect("dup"));
    std::thread::spawn(move || {
        find_icons(jobs, |update| {
            let _ = icons_tx.send(update);
            let _ = (&icons_wake).write(&[0]);
        })
    });
    let notify = notify::serve(move |event| {
        let _ = notes_tx.send(Update::Notify(event));
    })
    .map_err(|e| eprintln!("sc-bar: no Notification Center: {e}"))
    .ok();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell");
    // Bottom: a fullscreen app covers the bar.
    let bar = layer_shell.create_layer_surface(
        &qh,
        compositor.create_surface(&qh),
        Layer::Bottom,
        Some("bar"),
        None,
    );
    bar.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    bar.set_size(0, BAR_H as u32);
    bar.set_exclusive_zone(BAR_H as i32);
    bar.set_keyboard_interactivity(KeyboardInteractivity::None);

    let config = sc_config::load();
    if config.bar.lock {
        system::watch_lock(move |lock| {
            let _ = lock_tx.send(Update::Lock(lock));
            let _ = (&lock_wake).write(&[0]);
        });
    }
    let long_press = config
        .bar
        .long_press
        .iter()
        .filter_map(|(name, command)| match panel::Toggle::named(name) {
            Some(toggle) => Some((toggle, command.clone())),
            None => {
                eprintln!("sc-bar: [bar.long_press] has no toggle {name}");
                None
            }
        })
        .collect();

    let session_lock = SessionLockState::new(&globals, &qh);
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm: Shm::bind(&globals, &qh).expect("wl_shm"),
        pool: None,
        blur: globals.bind(&qh, 1..=1, BlurData).ok(),
        fractional: globals
            .bind(&qh, 1..=1, ScaleData)
            .ok()
            .zip(globals.bind(&qh, 1..=1, ScaleData).ok()),
        qh,
        bar,
        bar_hidpi: None,
        bar_w: 0,
        sheet: None,
        panel: None,
        scale: 1.0,
        font: draw::font(),
        theme: config.theme,
        icons: Vec::new(),
        long_press,
        long_press_secs: config.long_press_ms as f64 / 1000.0,
        hold: None,
        system: System::default(),
        worker,
        clock: String::new(),
        date: String::new(),
        finger: None,
        epoch: Instant::now(),
        last_tick: 0.0,
        center: Center::default(),
        notify,
        haptics: sc_haptics::Haptics::new("sc-bar").ok(),
        banner: None,
        banner_hide_at: None,
        banner_tick: 0.0,
        note_icons: HashMap::new(),
        icon_jobs,
        session_lock,
        locked: None,
        showing: 0,
        lock_screen: config.bar.lock,
        fprint: None,
        updates: auth_tx,
        wake: auth_wake.into(),
    };
    app.bar_hidpi = app.hidpi(app.bar.wl_surface());
    app.bar.commit();

    loop {
        // The compositor going away ends us cleanly: a panic would trip
        // Restart=on-failure while there is nothing to connect to.
        if let Err(e) = queue.dispatch_pending(&mut app) {
            eprintln!("sc-bar: wayland dispatch: {e}");
            return;
        }
        if let Some(notify) = &app.notify {
            notify.process();
        }
        let mut changed = false;
        for update in updates.try_iter() {
            match update {
                Update::System(system) => {
                    app.update(system);
                    changed = true;
                }
                Update::Notify(event) => app.notified(event),
                Update::Lock(true) => app.lock(),
                Update::Lock(false) => app.unlock(),
                // The screen went dark since: that PIN was for another showing.
                Update::Auth(showing, _) if showing != app.showing => {}
                Update::Auth(_, true) => app.unlock(),
                Update::Fprint(event) => app.fingerprint(event),
                Update::Auth(_, false) => {
                    if let Some(locked) = &mut app.locked {
                        locked.pad.failed();
                    }
                    app.refused();
                }
                Update::Shortcuts(ids, pixels) => {
                    if ids == app.system.shortcuts {
                        app.icons = pixels.iter().filter_map(draw::icon).collect();
                        changed = true;
                    }
                }
                Update::NoteIcon(key, pixels) => {
                    app.note_icons.insert(key, draw::icon(&pixels));
                    app.draw_banner();
                    app.redraw_list();
                    app.redraw_lock(true);
                }
            }
        }
        let (clock, date, next_minute) = clock();
        if changed || clock != app.clock {
            (app.clock, app.date) = (clock, date);
            app.draw_bar();
            app.redraw(true);
            app.redraw_lock(true);
        }
        if app.banner_hide_at.is_some_and(|at| app.now() >= at) {
            app.banner_hide_at = None;
            app.center.dismiss_banner();
            app.banner_tick = app.now();
            app.draw_banner();
        }
        if app.hold.is_some_and(|at| app.now() >= at) {
            app.hold = None;
            app.long_press();
        }
        if let Err(e) = queue.flush() {
            eprintln!("sc-bar: wayland flush: {e}");
            return;
        }
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let wait = [app.hold, app.banner_hide_at]
            .into_iter()
            .flatten()
            .map(|at| (at - app.now()).max(0.0))
            .fold(next_minute as f64, f64::min);
        let timeout = rustix::event::Timespec {
            tv_sec: wait as i64,
            tv_nsec: (wait.fract() * 1e9) as i64,
        };
        let readable = {
            let fd = guard.connection_fd();
            // SAFETY: the bus outlives the poll; without one, the wake end
            // stands in twice.
            let notes = app.notify.as_ref().map_or(wake_rx.as_fd(), |n| unsafe {
                std::os::fd::BorrowedFd::borrow_raw(n.fd())
            });
            let mut fds = [
                rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&wake_rx, rustix::event::PollFlags::IN),
                rustix::event::PollFd::new(&notes, rustix::event::PollFlags::IN),
            ];
            let _ = rustix::event::poll(&mut fds, Some(&timeout));
            !fds[0].revents().is_empty()
        };
        if readable {
            if let Err(e) = guard.read() {
                if !matches!(e, wayland_client::backend::WaylandError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock)
                {
                    eprintln!("sc-bar: wayland read: {e}");
                    return;
                }
            }
        } else {
            drop(guard);
        }
        let _ = wake_rx.read(&mut [0; 64]);
    }
}

fn buffer<'a>(
    pool: &'a mut Option<SlotPool>,
    shm: &Shm,
    w: i32,
    h: i32,
) -> Option<(smithay_client_toolkit::shm::slot::Buffer, &'a mut [u8])> {
    let pool =
        pool.get_or_insert_with(|| SlotPool::new((w * h * 4) as usize, shm).expect("shm pool"));
    pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
        .ok()
}

/// Scans the apps and decodes icons off the thread that draws: either can
/// take a good part of a second.
fn find_icons(jobs: mpsc::Receiver<Icons>, send: impl Fn(Update)) {
    let mut catalog = sc_catalog::scan_apps();
    let dirs = sc_icons::theme_dirs(&sc_catalog::xdg_data_dirs());
    for job in jobs {
        match job {
            Icons::Shortcuts(ids) => {
                catalog = sc_catalog::scan_apps();
                // A letter tile for one the catalog doesn't know.
                let pixels = ids
                    .iter()
                    .map(|id| match catalog.iter().find(|a| &a.id == id) {
                        Some(app) => sc_icons::resolve_with_dirs(&app.icon, &dirs),
                        None => sc_icons::placeholder(id),
                    })
                    .collect();
                send(Update::Shortcuts(ids, pixels));
            }
            Icons::Note {
                key,
                app,
                icon,
                app_name,
            } => {
                let listed = |catalog: &[sc_catalog::AppEntry]| {
                    let app = app.as_ref()?;
                    Some(catalog.iter().find(|a| &a.id == app)?.icon.clone())
                };
                // Not there: installed since the last look, perhaps.
                if app.is_some() && listed(&catalog).is_none() {
                    catalog = sc_catalog::scan_apps();
                }
                let name = listed(&catalog).unwrap_or(icon);
                let name = name.strip_prefix("file://").unwrap_or(&name);
                // A path is the sender's word: no pipes, nothing huge,
                // nothing relative to the theme dirs.
                let plain = |p: &str| {
                    p.starts_with('/')
                        && std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() <= 2 << 20)
                };
                let pixels = if name.is_empty() || (name.contains('/') && !plain(name)) {
                    // Its initial on a tile.
                    sc_icons::placeholder(&app_name)
                } else {
                    sc_icons::resolve_with_dirs(name, &dirs)
                };
                send(Update::NoteIcon(key, pixels));
            }
        }
    }
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// `HH:MM` and `Monday 16 November` in local time, and the seconds until the
/// first changes.
fn clock() -> (String, String, i64) {
    // SAFETY: plain libc calls on a zeroed out-struct.
    let tm = unsafe {
        let mut tm = std::mem::zeroed::<libc::tm>();
        // localtime_r alone never notices the zone changing.
        extern "C" {
            fn tzset();
        }
        tzset();
        libc::localtime_r(&libc::time(std::ptr::null_mut()), &mut tm);
        tm
    };
    (
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        format!(
            "{} {} {}",
            WEEKDAYS[tm.tm_wday.clamp(0, 6) as usize],
            tm.tm_mday,
            MONTHS[tm.tm_mon.clamp(0, 11) as usize]
        ),
        60 - tm.tm_sec.min(59) as i64,
    )
}

/// springchick's IPC. Elsewhere rotation and the shortcuts are left out.
fn ipc(line: &str) -> Option<String> {
    let path = std::env::var_os("SPRINGCHICK_IPC_SOCK")
        .map(Into::into)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(|d| std::path::PathBuf::from(d).join("springchick-ipc.sock"))
        })?;
    // Each line is a verb of its own.
    if line.contains(['\n', '\r']) {
        return None;
    }
    let mut stream = UnixStream::connect(path).ok()?;
    let timeout = Some(std::time::Duration::from_millis(500));
    stream.set_read_timeout(timeout).ok()?;
    stream.set_write_timeout(timeout).ok()?;
    writeln!(stream, "{line}").ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    // A verb with nothing to say answers a bare `ok`.
    reply
        .trim_end()
        .strip_prefix("ok")
        .map(|r| r.trim_start().to_string())
}

impl App {
    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    /// The lock screen's or the sheet's size, or the output's before either
    /// is configured.
    fn screen(&self) -> (f32, f32) {
        match (&self.locked, &self.sheet) {
            (Some(l), _) if l.w > 0 => (l.w as f32, l.h as f32),
            (_, Some(s)) if s.w > 0 => (s.w as f32, s.h as f32),
            _ => {
                let (w, h) = self
                    .output_state
                    .outputs()
                    .next()
                    .and_then(|o| self.output_state.info(&o)?.logical_size)
                    .unwrap_or((400, 800));
                (w as f32, h as f32)
            }
        }
    }

    fn hidpi(&self, surface: &wl_surface::WlSurface) -> Option<Hidpi> {
        let (manager, viewporter) = self.fractional.as_ref()?;
        Some(Hidpi(
            viewporter.get_viewport(surface, &self.qh, ScaleData),
            manager.get_fractional_scale(surface, &self.qh, ScaleData),
        ))
    }

    /// Buffer px for a surface `w`×`h` logical, which the surface is told to
    /// show at that size.
    fn sized(
        &self,
        surface: &wl_surface::WlSurface,
        hidpi: &Option<Hidpi>,
        w: u32,
        h: u32,
    ) -> (i32, i32) {
        match hidpi {
            Some(Hidpi(viewport, _)) => viewport.set_destination(w as i32, h as i32),
            None => surface.set_buffer_scale(self.scale as i32),
        }
        let px = |n: u32| (n as f32 * self.scale).round() as i32;
        (px(w), px(h))
    }

    fn update(&mut self, mut system: System) {
        // The slider under the finger is ahead of the system's answer.
        if let Some(slider) = self.panel.as_ref().and_then(Panel::dragged_slider) {
            let value = slider.get(&self.system.controls);
            panel::apply(
                &mut system.controls,
                Change::Slider(slider, value.unwrap_or(0.0)),
            );
        }
        if system.shortcuts != self.system.shortcuts {
            self.icons.clear();
            let _ = self
                .icon_jobs
                .send(Icons::Shortcuts(system.shortcuts.clone()));
        }
        self.system = system;
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// An app sent a notification, or took one back.
    fn notified(&mut self, event: notify::Event) {
        match event {
            notify::Event::Notify(mut n) => {
                n.lines = draw::wrap(&mut self.font, &n.body);
                self.cache_icon(&n);
                // Silent mode still collects them.
                let silent = panel::Toggle::Silent.get(&self.system.controls) == Some(true);
                let alert = self.sheet.is_none() && !silent;
                // The lock screen has it in its list.
                let banner = alert && self.locked.is_none();
                self.center.add(n, banner);
                if alert {
                    if let Some(haptics) = &self.haptics {
                        haptics.trigger("message-new-instant");
                    }
                }
                if banner {
                    self.banner_hide_at = Some(self.now() + center::BANNER_SHOWN);
                    self.show_banner();
                }
            }
            notify::Event::Close(id) => self.center.remove(id),
        }
        self.banner_tick = self.now();
        self.draw_banner();
        self.redraw_list();
        self.redraw_lock(true);
    }

    fn cache_icon(&mut self, n: &center::Notification) {
        let key = draw::icon_key(n);
        if key.is_empty() || self.note_icons.contains_key(key) {
            return;
        }
        if self.note_icons.len() >= 64 {
            let center = &self.center;
            self.note_icons
                .retain(|k, _| center.notifications().any(|n| draw::icon_key(n) == k));
        }
        self.note_icons.insert(key.to_string(), None);
        let _ = self.icon_jobs.send(Icons::Note {
            key: key.to_string(),
            app: n.app.clone(),
            icon: n.icon.clone(),
            app_name: n.app_name.clone(),
        });
    }

    /// Tells the apps which of theirs the user cleared.
    fn flush_outcomes(&mut self) {
        for (id, outcome) in self.center.take_outcomes() {
            if let (center::Outcome::Cleared, Some(notify)) = (outcome, &self.notify) {
                notify.cleared(id);
            }
        }
    }

    /// The user tapped a notification: its app hears, and comes to the front.
    /// A tap on one of its buttons is the app's alone to answer.
    fn open_notification(&mut self, id: u32, action: Option<String>) {
        let Some(n) = self.center.open(id) else {
            return;
        };
        if let Some(notify) = &self.notify {
            notify.opened(&n, action.as_deref());
        }
        if action.is_some() {
            return;
        }
        self.close_sheet();
        if let Some(app) = &n.app {
            if ipc(&format!("launch {app}")).is_none() {
                eprintln!("sc-bar: springchick would not open {app}");
            }
        }
    }

    fn show_banner(&mut self) {
        if self.banner.is_some() {
            return;
        }
        let (w, _) = self.screen();
        let h = Center::banner_height(w).ceil() as u32;
        let surface = self.compositor.create_surface(&self.qh);
        let hidpi = self.hidpi(&surface);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("bar-banner"),
            None,
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, h);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.banner = Some(BannerSurface {
            layer,
            hidpi,
            w: 0,
            h,
            frame_pending: false,
            dirty: true,
        });
    }

    /// A finger on it may never be seen lifting.
    fn drop_banner(&mut self) {
        self.banner = None;
        if self
            .finger
            .take_if(|f| f.target == Target::Banner)
            .is_some()
        {
            self.center.cancel_touch();
        }
    }

    /// Draws the banner now, or once the frame in flight is shown; puts its
    /// surface away when it has gone back up.
    fn draw_banner(&mut self) {
        if self.center.banner_gone() {
            self.drop_banner();
            self.banner_hide_at = None;
            return;
        }
        let Some(banner) = &mut self.banner else {
            return;
        };
        banner.dirty = true;
        if banner.w == 0 || banner.frame_pending {
            return;
        }
        let scale = self.scale;
        let banner = self.banner.as_ref().unwrap();
        let (w, h) = self.sized(banner.layer.wl_surface(), &banner.hidpi, banner.w, banner.h);
        let Some((n, rect)) = self.center.banner(banner.w as f32) else {
            return;
        };
        let Some((buffer, pixels)) = buffer(&mut self.pool, &self.shm, w, h) else {
            return;
        };
        let mut scene = draw::Scene {
            font: &mut self.font,
            theme: &self.theme,
            system: &self.system,
            clock: &self.clock,
            icons: &self.icons,
        };
        let notes = draw::Notes {
            center: &self.center,
            date: &self.date,
            now: Self::unix_now(),
            icons: &self.note_icons,
            serving: self.notify.is_some(),
        };
        draw::banner(pixels, w, h, scale, &mut scene, &notes, n, rect);
        let banner = self.banner.as_mut().unwrap();
        let surface = banner.layer.wl_surface();
        // Only the card takes touches: the strip beside it is the app's.
        if let Ok(region) = Region::new(&self.compositor) {
            region.add(
                rect.x as i32,
                0,
                rect.w as i32,
                (rect.y + rect.h).max(0.0) as i32,
            );
            surface.set_input_region(Some(region.wl_region()));
        }
        surface.damage_buffer(0, 0, w, h);
        banner.dirty = false;
        banner.frame_pending = true;
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let _ = buffer.attach_to(surface);
        banner.layer.commit();
    }

    fn banner_frame(&mut self) {
        let now = self.now();
        let dt = (now - self.banner_tick).min(0.05) as f32;
        self.banner_tick = now;
        self.center.step_banner(dt);
        let Some(banner) = &mut self.banner else {
            return;
        };
        banner.frame_pending = false;
        if self.center.banner_gone() || self.center.banner_animating() || banner.dirty {
            self.draw_banner();
        }
    }

    fn draw_bar(&mut self) {
        if self.bar_w == 0 {
            return;
        }
        let scale = self.scale;
        let (w, h) = self.sized(
            self.bar.wl_surface(),
            &self.bar_hidpi,
            self.bar_w,
            BAR_H as u32,
        );
        let Some((buffer, pixels)) = buffer(&mut self.pool, &self.shm, w, h) else {
            return;
        };
        let mut scene = draw::Scene {
            font: &mut self.font,
            theme: &self.theme,
            system: &self.system,
            clock: &self.clock,
            icons: &self.icons,
        };
        draw::bar(pixels, w, h, scale, &mut scene);
        let surface = self.bar.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        let _ = buffer.attach_to(surface);
        self.bar.commit();
    }

    fn open_sheet(&mut self, y: f32, page: Page) {
        let surface = self.compositor.create_surface(&self.qh);
        let hidpi = self.hidpi(&surface);
        let blur = self
            .blur
            .as_ref()
            .map(|m| m.get_background_effect(&surface, &self.qh, BlurData));
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("bar-sheet"),
            None,
        );
        layer.set_anchor(Anchor::all());
        // Over the bar's own strip too.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.sheet = Some(Sheet {
            layer,
            hidpi,
            blur,
            w: 0,
            h: 0,
            frame_pending: false,
            dirty: true,
            repaint: true,
            open: Vec::new(),
            drawn_edge: 0,
            bounced: false,
            full: false,
        });
        self.panel = Some(Panel::pulled(y, page));
        let _ = self.worker.send(Msg::Opened);
        // The sheet covers it, and has it in its list.
        self.center.dismiss_banner();
        self.banner_hide_at = None;
        self.banner_tick = self.now();
        self.draw_banner();
    }

    fn close_sheet(&mut self) {
        if let Some(blur) = self.sheet.take().and_then(|s| s.blur) {
            blur.destroy();
        }
        self.panel = None;
        self.finger = None;
        self.center.rest();
    }

    /// Draws the sheet for a change to Notification Center's list, which is
    /// drawn over the rest every frame: nothing else needs drawing again.
    fn redraw_list(&mut self) {
        if let Some(sheet) = &mut self.sheet {
            sheet.repaint = true;
        }
        self.redraw(false);
    }

    /// Draws the sheet now, or once the frame in flight is shown.
    fn redraw(&mut self, repaint: bool) {
        let Some(sheet) = &mut self.sheet else { return };
        sheet.dirty = true;
        sheet.repaint |= repaint;
        if repaint {
            sheet.open.clear();
        }
        if !sheet.frame_pending {
            self.draw_sheet();
        }
    }

    fn draw_sheet(&mut self) {
        let Some(sheet) = &self.sheet else { return };
        if sheet.w == 0 {
            return;
        }
        let scale = self.scale;
        let (w, h) = self.sized(sheet.layer.wl_surface(), &sheet.hidpi, sheet.w, sheet.h);
        let (Some(sheet), Some(panel)) = (&mut self.sheet, &self.panel) else {
            return;
        };
        let edge = (panel.shown() * h as f32).ceil() as i32;
        let Some((buffer, pixels)) = buffer(&mut self.pool, &self.shm, w, h) else {
            return;
        };
        if sheet.open.len() != pixels.len() {
            sheet.open.resize(pixels.len(), 0);
            let mut scene = draw::Scene {
                font: &mut self.font,
                theme: &self.theme,
                system: &self.system,
                clock: &self.clock,
                icons: &self.icons,
            };
            if panel.page == Page::Notifications {
                let notes = draw::Notes {
                    center: &self.center,
                    date: &self.date,
                    now: Self::unix_now(),
                    icons: &self.note_icons,
                    serving: self.notify.is_some(),
                };
                draw::notifications(&mut sheet.open, w, h, scale, &mut scene, &notes);
            } else {
                draw::sheet(&mut sheet.open, w, h, scale, panel, &mut scene);
            }
        }
        // Past the bottom the edge has nowhere to go: the modules carry on.
        let shift = (panel.overshoot() * h as f32) as usize;
        let bar = (BAR_H * scale).ceil() as usize;
        draw::reveal(
            pixels,
            &sheet.open,
            w as usize * 4,
            edge as usize,
            bar,
            shift,
        );
        if panel.page == Page::Notifications {
            let mut scene = draw::Scene {
                font: &mut self.font,
                theme: &self.theme,
                system: &self.system,
                clock: &self.clock,
                icons: &self.icons,
            };
            let notes = draw::Notes {
                center: &self.center,
                date: &self.date,
                now: Self::unix_now(),
                icons: &self.note_icons,
                serving: self.notify.is_some(),
            };
            let (edge, shift) = (edge as f32 / scale, shift as f32 / scale);
            draw::notification_list(pixels, w, h, scale, &mut scene, &notes, edge, shift);
        }

        let surface = sheet.layer.wl_surface();
        if let Some(blur) = &sheet.blur {
            if let Ok(region) = Region::new(&self.compositor) {
                region.add(0, 0, sheet.w as i32, (edge as f32 / scale) as i32);
                blur.set_blur_region(Some(region.wl_region()));
            }
        }
        let bounce = panel.overshoot() > 0.0;
        sheet.repaint |= bounce || sheet.bounced;
        sheet.bounced = bounce;
        if edge >= h && !sheet.full {
            sheet.full = true;
            // The home pill blinks, as for an app that has arrived. Off this
            // thread: the reply takes a compositor frame.
            std::thread::spawn(|| ipc("pill"));
        } else if panel.shown() < 0.9 {
            sheet.full = false;
        }
        // Every buffer is drawn whole, but a moving edge only changes the
        // band it crossed, and the compositor uploads no more than that.
        if std::mem::take(&mut sheet.repaint) {
            surface.damage_buffer(0, 0, w, h);
        } else {
            let top = edge.min(sheet.drawn_edge) - 1;
            surface.damage_buffer(0, top, w, edge.max(sheet.drawn_edge) + 1 - top);
        }
        sheet.drawn_edge = edge;
        sheet.dirty = false;
        sheet.frame_pending = true;
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let _ = buffer.attach_to(surface);
        sheet.layer.commit();
    }

    fn set_scale(&mut self, scale: f32) {
        if scale != self.scale {
            self.scale = scale;
            self.draw_bar();
            self.redraw(true);
            self.redraw_lock(true);
        }
    }

    /// Asks the compositor for the lock and puts the lock screen on it.
    /// Asked again while locked (the screen went dark), back to the cover.
    fn lock(&mut self) {
        self.showing += 1;
        self.sensor_off();
        if let Some(locked) = &mut self.locked {
            locked.pad = lock::Lock::default();
            locked.leaving = false;
            locked.sensor = Sensor::Idle;
            return self.redraw_lock(true);
        }
        let Some(output) = self.output_state.outputs().next() else {
            return;
        };
        let session = match self.session_lock.lock(&self.qh) {
            Ok(session) => session,
            Err(e) => return eprintln!("sc-bar: the compositor has no session lock: {e}"),
        };
        self.close_sheet();
        self.center.dismiss_banner();
        self.drop_banner();
        self.banner_hide_at = None;
        let surface = self.compositor.create_surface(&self.qh);
        let hidpi = self.hidpi(&surface);
        let surface = session.create_lock_surface(surface, &output, &self.qh);
        self.locked = Some(Locked {
            hidpi,
            surface,
            session,
            w: 0,
            h: 0,
            frame_pending: false,
            dirty: true,
            pad: lock::Lock::default(),
            leaving: false,
            sensor: Sensor::Idle,
        });
        if let Some(marker) = lock_marker() {
            let _ = std::fs::write(marker, "");
        }
    }

    fn unlock(&mut self) {
        // Dropped before `locked`, the compositor would stay locked alone.
        if let Some(locked) = self.locked.take_if(|l| l.session.is_locked()) {
            locked.session.unlock();
            self.unlocked();
        } else if let Some(locked) = &mut self.locked {
            locked.leaving = true;
        }
    }

    fn sensor_off(&self) {
        if let Some(fprint) = &self.fprint {
            let _ = fprint.send(fprint::Cmd::Stop);
        }
    }

    /// Asks for fingers once per showing, from its first frame: none is shown
    /// on a dark panel, so the sensor sleeps with it.
    fn sensor_on(&mut self) {
        let Some(locked) = self.locked.as_mut().filter(|l| l.sensor == Sensor::Idle) else {
            return;
        };
        locked.sensor = Sensor::Asked;
        let (updates, wake) = (self.updates.clone(), self.wake.clone());
        let fprint = self.fprint.get_or_insert_with(|| {
            fprint::watch(move |event| {
                let _ = updates.send(Update::Fprint(event));
                let _ = (&*wake).write(&[0]);
            })
        });
        let _ = fprint.send(fprint::Cmd::Start(self.showing));
    }

    fn fingerprint(&mut self, event: fprint::Event) {
        use fprint::Event::*;
        let (Ready(showing) | Matched(showing) | Retry(showing) | Off(showing)) = event;
        let Some(locked) = self.locked.as_mut().filter(|_| showing == self.showing) else {
            return;
        };
        locked.sensor = match event {
            Matched(_) => return self.unlock(),
            Ready(_) => Sensor::Reading,
            Retry(_) => {
                locked.pad.refuse();
                locked.sensor = Sensor::Missed;
                return self.refused();
            }
            Off(_) => Sensor::Off,
        };
        self.redraw_lock(true);
    }

    /// A wrong PIN or finger: a buzz, and the shake starts from now.
    fn refused(&mut self) {
        if let Some(haptics) = &self.haptics {
            haptics.trigger("bell-terminal");
        }
        self.last_tick = self.now();
        self.redraw_lock(true);
    }

    /// The lock is gone, by the PIN or the compositor's word.
    fn unlocked(&mut self) {
        self.sensor_off();
        if let Some(marker) = lock_marker() {
            let _ = std::fs::remove_file(marker);
        }
        self.finger = None;
        self.center.rest();
    }

    /// PAM takes seconds over a wrong PIN: asked on a thread of its own.
    fn authenticate(&self, pin: String) {
        let (updates, wake, showing) = (self.updates.clone(), self.wake.clone(), self.showing);
        std::thread::spawn(move || {
            let _ = updates.send(Update::Auth(showing, pam::authenticate(&pin)));
            let _ = (&*wake).write(&[0]);
        });
    }

    /// Draws the lock screen once the frame in flight is shown, or `now`: a
    /// dark panel shows no frame, and must wake to the right one.
    fn redraw_lock(&mut self, now: bool) {
        let Some(locked) = &mut self.locked else {
            return;
        };
        locked.dirty = true;
        if now || !locked.frame_pending {
            self.draw_lock();
        }
    }

    // ponytail: the whole buffer for every frame the cover or a key moves;
    // cache the cover as the sheet does if a phone drops frames here.
    fn draw_lock(&mut self) {
        let Some(locked) = &self.locked else { return };
        if locked.w == 0 {
            return;
        }
        let scale = self.scale;
        let surface = locked.surface.wl_surface();
        let (w, h) = self.sized(surface, &locked.hidpi, locked.w, locked.h);
        let Some((buffer, pixels)) = buffer(&mut self.pool, &self.shm, w, h) else {
            return;
        };
        let mut scene = draw::Scene {
            font: &mut self.font,
            theme: &self.theme,
            system: &self.system,
            clock: &self.clock,
            icons: &self.icons,
        };
        let notes = draw::Notes {
            center: &self.center,
            date: &self.date,
            now: Self::unix_now(),
            icons: &self.note_icons,
            serving: self.notify.is_some(),
        };
        let locked = self.locked.as_mut().unwrap();
        let hint = match locked.sensor {
            Sensor::Reading => "Touch the sensor or swipe up",
            Sensor::Missed => "Not recognised, try again",
            _ => "Swipe up to unlock",
        };
        draw::lock(pixels, w, h, scale, &mut scene, &notes, &locked.pad, hint);
        let surface = locked.surface.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        locked.dirty = false;
        locked.frame_pending = true;
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        let _ = buffer.attach_to(surface);
        surface.commit();
    }

    fn lock_frame(&mut self) {
        self.sensor_on();
        let view = self.view();
        let now = self.now();
        let dt = (now - self.last_tick).min(0.05) as f32;
        self.last_tick = now;
        let Some(locked) = &mut self.locked else {
            return;
        };
        locked.frame_pending = false;
        locked.pad.step(dt);
        self.center.step(dt, Some(view));
        if locked.dirty || locked.pad.is_animating() || self.center.is_animating() {
            self.draw_lock();
        }
        self.flush_outcomes();
    }

    fn carry_out(&mut self, change: Change) {
        panel::apply(&mut self.system.controls, change);
        match change {
            Change::Launch(i) => {
                self.close_sheet();
                let Some(id) = self.system.shortcuts.get(i) else {
                    return;
                };
                if ipc(&format!("launch {id}")).is_none() {
                    eprintln!("sc-bar: springchick would not launch {id}");
                }
            }
            Change::Power(action) => {
                // Away first: nothing should wake up, or log back in, to it.
                self.close_sheet();
                match action {
                    panel::PowerAction::LogOut => {
                        ipc("quit");
                    }
                    _ => {
                        let _ = self.worker.send(Msg::Do(change));
                    }
                }
            }
            _ => {
                let _ = self.worker.send(Msg::Do(change));
            }
        }
    }

    fn view(&self) -> center::View {
        let (w, h) = self.screen();
        draw::list_view(w, h)
    }

    fn down(&mut self, surface: &wl_surface::WlSurface, id: i32, (x, y): (f64, f64)) {
        if self.finger.is_some() {
            return;
        }
        let at = (x as f32, y as f32);
        let target = if surface == self.bar.wl_surface() {
            Target::Bar
        } else if self
            .banner
            .as_ref()
            .is_some_and(|b| b.layer.wl_surface() == surface)
        {
            Target::Banner
        } else if self
            .locked
            .as_ref()
            .is_some_and(|l| l.surface.wl_surface() == surface)
        {
            Target::Lock
        } else {
            Target::Sheet
        };
        self.finger = Some(Finger {
            id,
            target,
            at: self.now(),
            x: at.0,
            y: at.1,
            vx: 0.0,
            vy: 0.0,
        });
        let (w, h) = self.screen();
        match target {
            // Notification Center comes down the left half, the controls the right.
            Target::Bar if self.sheet.is_none() => {
                let page = if at.0 < self.bar_w as f32 / 2.0 {
                    Page::Notifications
                } else {
                    Page::Main
                };
                self.open_sheet(at.1, page);
            }
            Target::Bar => {}
            Target::Banner => {
                if self.center.touch_banner(at, w) {
                    self.banner_hide_at = None;
                }
            }
            Target::Lock => {
                let Some(locked) = &mut self.locked else {
                    return;
                };
                // The bottom of the screen moves the cover, whatever is there.
                let list = locked.pad.covered() && at.1 < h * 0.88;
                if !(list && self.center.touch_list(at, draw::list_view(w, h))) {
                    locked.pad.touch_down(at, w, h);
                }
                self.redraw_lock(false);
            }
            Target::Sheet => {
                let Some(panel) = &mut self.panel else { return };
                // The bottom of the screen is the way out, whatever is there.
                let notes = panel.page == Page::Notifications;
                let list = notes && at.1 < h * 0.88;
                if !(list && self.center.touch_list(at, draw::list_view(w, h))) {
                    panel.touch_down(at, w, h, &self.system.controls);
                    self.hold = Some(self.now() + self.long_press_secs);
                }
                if notes {
                    self.redraw_list();
                } else {
                    self.redraw(true);
                }
            }
        }
    }

    fn motion(&mut self, id: i32, (x, y): (f64, f64)) {
        let now = self.now();
        let Some(finger) = self.finger.as_mut().filter(|f| f.id == id) else {
            return;
        };
        let at = (x as f32, y as f32);
        let dt = (now - finger.at).max(0.001) as f32;
        finger.vx = 0.5 * finger.vx + 0.5 * (at.0 - finger.x) / dt;
        finger.vy = 0.5 * finger.vy + 0.5 * (at.1 - finger.y) / dt;
        (finger.at, finger.x, finger.y) = (now, at.0, at.1);
        let target = finger.target;
        if self.center.is_touched() {
            self.center.touch_move(at, self.view());
            match target {
                Target::Banner => self.draw_banner(),
                Target::Lock => self.redraw_lock(false),
                _ => self.redraw_list(),
            }
            return;
        }
        let (w, h) = self.screen();
        if target == Target::Lock {
            if let Some(locked) = &mut self.locked {
                locked.pad.touch_move(at, w, h);
            }
            return self.redraw_lock(false);
        }
        let Some(panel) = &mut self.panel else { return };
        let change = panel.touch_move(at, w, h, &self.system.controls);
        if let Some(change) = change {
            self.carry_out(change);
        }
        self.redraw(change.is_some());
    }

    /// Runs the command of the toggle under a finger that stayed put.
    fn long_press(&mut self) {
        let Some(panel::Control::Toggle(toggle)) = self.panel.as_ref().and_then(Panel::held) else {
            return;
        };
        let Some((_, command)) = self.long_press.iter().find(|(t, _)| *t == toggle) else {
            return;
        };
        // Backgrounded, so the shell returns at once and nothing is left to reap.
        // ponytail: still in the bar's cgroup, so it goes when the bar's unit
        // does; hand it to the compositor's launcher if that bites.
        let run = std::process::Command::new("sh")
            .args(["-c", &format!("{command} &")])
            .status();
        if let Err(e) = run {
            eprintln!("sc-bar: {command}: {e}");
        }
        if let Some(panel) = &mut self.panel {
            panel.close();
            self.last_tick = self.now();
            self.redraw(true);
        }
    }

    fn up(&mut self, id: i32) {
        let now = self.now();
        let Some(finger) = self.finger.take_if(|f| f.id == id) else {
            return;
        };
        self.hold = None;
        // A finger that stopped before lifting is not a flick.
        let (vx, vy) = if now - finger.at > 0.1 {
            (0.0, 0.0)
        } else {
            (finger.vx, finger.vy)
        };
        (self.last_tick, self.banner_tick) = (now, now);
        let (_, h) = self.screen();
        if self.center.is_touched() {
            match self.center.touch_up((vx, vy), self.view()) {
                // Opening one takes the PIN first.
                Some(_) if finger.target == Target::Lock => {
                    if let Some(locked) = &mut self.locked {
                        locked.pad.show_pad(h);
                    }
                }
                Some((id, action)) => self.open_notification(id, action),
                // Put back, it stays its time again.
                None if finger.target == Target::Banner && !self.center.banner_animating() => {
                    self.banner_hide_at = Some(now + center::BANNER_SHOWN);
                }
                None => {}
            }
            self.flush_outcomes();
            self.draw_banner();
            self.redraw_list();
            self.redraw_lock(false);
            return;
        }
        if finger.target == Target::Lock {
            let pin = self.locked.as_mut().and_then(|l| l.pad.touch_up(vy, h));
            if let Some(pin) = pin {
                self.authenticate(pin);
            }
            return self.redraw_lock(false);
        }
        let Some(panel) = &mut self.panel else { return };
        let change = panel.touch_up(vy, h, &self.system.controls);
        if let Some(change) = change {
            self.carry_out(change);
        }
        self.redraw(true);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if self.fractional.is_none() {
            self.set_scale(factor as f32);
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self
            .banner
            .as_ref()
            .is_some_and(|b| b.layer.wl_surface() == surface)
        {
            return self.banner_frame();
        }
        if self
            .locked
            .as_ref()
            .is_some_and(|l| l.surface.wl_surface() == surface)
        {
            return self.lock_frame();
        }
        // A surface already put away can still have one in flight.
        if !self
            .sheet
            .as_ref()
            .is_some_and(|s| s.layer.wl_surface() == surface)
        {
            return;
        }
        let view = self.view();
        let now = self.now();
        let dt = (now - self.last_tick).min(0.05) as f32;
        self.last_tick = now;
        let (Some(sheet), Some(panel)) = (&mut self.sheet, &mut self.panel) else {
            return;
        };
        sheet.frame_pending = false;
        panel.step(dt);
        let mut list_moving = false;
        if panel.page == Page::Notifications {
            self.center.step(dt, Some(view));
            list_moving = self.center.is_animating();
        }
        if panel.is_dismissed() {
            self.close_sheet();
        } else if sheet.dirty || panel.is_animating() || list_moving {
            sheet.repaint |= list_moving;
            self.draw_sheet();
        }
        self.flush_outcomes();
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        if self.lock_screen && self.locked.is_none() && lock_marker().is_some_and(|m| m.exists()) {
            self.lock();
        }
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if *layer == self.bar {
            // Nothing to draw on: let the service manager decide.
            std::process::exit(0);
        }
        if self.banner.as_ref().is_some_and(|b| b.layer == *layer) {
            self.drop_banner();
            return;
        }
        self.close_sheet();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if *layer == self.bar {
            self.bar_w = configure.new_size.0;
            self.draw_bar();
        } else if let Some(banner) = self.banner.as_mut().filter(|b| b.layer == *layer) {
            banner.w = configure.new_size.0;
            banner.frame_pending = false;
            self.banner_tick = self.now();
            self.draw_banner();
        } else if let Some(sheet) = &mut self.sheet {
            (sheet.w, sheet.h) = configure.new_size;
            sheet.frame_pending = false;
            self.redraw(true);
        }
    }
}

impl SessionLockHandler for App {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        if self.locked.as_ref().is_some_and(|l| l.leaving) {
            self.unlock();
        }
    }

    /// Refused: another locker has the session.
    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        self.locked = None;
        self.unlocked();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        if let Some(locked) = &mut self.locked {
            (locked.w, locked.h) = configure.new_size;
            self.redraw_lock(true);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Touch => {
                let _ = self.seat_state.get_touch(qh, &seat);
            }
            Capability::Pointer => {
                let _ = self.seat_state.get_pointer(qh, &seat);
            }
            _ => {}
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl TouchHandler for App {
    fn down(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: u32,
        _: u32,
        surface: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        self.down(&surface, id, position);
    }
    fn up(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: u32,
        _: u32,
        id: i32,
    ) {
        self.up(id);
    }
    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: u32,
        id: i32,
        position: (f64, f64),
    ) {
        self.motion(id, position);
    }
    fn shape(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: i32,
        _: f64,
        _: f64,
    ) {
    }
    fn orientation(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        _: i32,
        _: f64,
    ) {
    }
    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch) {
        self.finger = None;
        self.center.cancel_touch();
        if let Some(locked) = &mut self.locked {
            locked.pad.cancel_touch();
            self.redraw_lock(false);
        }
        if let Some(panel) = &mut self.panel {
            panel.close();
            self.last_tick = self.now();
            self.redraw(true);
        }
    }
}

/// The mouse as one finger, for nested testing.
const POINTER_TOUCH: i32 = -1;

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            match e.kind {
                PointerEventKind::Press { .. } => self.down(&e.surface, POINTER_TOUCH, e.position),
                PointerEventKind::Release { .. } => self.up(POINTER_TOUCH),
                PointerEventKind::Motion { .. } => self.motion(POINTER_TOUCH, e.position),
                _ => {}
            }
        }
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

/// User data for ext-background-effect, which sctk doesn't wrap.
struct BlurData;

/// User data for fractional-scale and viewporter.
struct ScaleData;

impl Dispatch2<WpFractionalScaleV1, App> for ScaleData {
    fn event(
        &self,
        app: &mut App,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.set_scale(scale as f32 / 120.0);
        }
    }
}

macro_rules! no_events {
    ($($ty:ty: $data:ty),+ $(,)?) => {$(
        impl Dispatch2<$ty, App> for $data {
            fn event(
                &self,
                _: &mut App,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &Connection,
                _: &QueueHandle<App>,
            ) {
            }
        }
    )+};
}

no_events!(
    ExtBackgroundEffectManagerV1: BlurData,
    ExtBackgroundEffectSurfaceV1: BlurData,
    WpFractionalScaleManagerV1: ScaleData,
    WpViewporter: ScaleData,
    WpViewport: ScaleData,
);

delegate_registry!(App);
delegate_dispatch2!(App);
