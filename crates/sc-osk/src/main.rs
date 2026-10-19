//! springchick's on-screen keyboard. It types through uinput, so every client
//! sees a real keyboard; input-method-v2 says when to show and what the field
//! wants, and commits what the keymap can't type (emoji, accents, €), which
//! needs the client to speak text-input-v3. Keys are labelled from the seat keymap, and the bar above them
//! follows the app in front (wlr-foreign-toplevel). Standalone: any other
//! keyboard can replace it.

mod accents;
mod draw;
mod emoji;
mod grid;
mod labels;
mod osk;
mod uinput;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
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
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::sync::Arc;
use wayland_client::{
    backend::ObjectId,
    globals::{registry_queue_init, GlobalList},
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface, wl_touch},
    Connection, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_misc::zwp_input_method_v2::client::{
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{self, ZwpInputMethodV2},
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use grid::{Bar, Labels, Page, Placed};
use labels::Keymap;
use osk::{Osk, Out};

/// Apps that get Esc/Ctrl/Alt/Super and terminal paste (Ctrl+Shift+V).
const TERMINALS: &[&str] = &[
    "foot",
    "footclient",
    "org.codeberg.dnkl.foot",
    "kitty",
    "Alacritty",
    "org.wezfurlong.wezterm",
    "org.gnome.Console",
    "org.gnome.Ptyxis",
    "org.kde.konsole",
    "com.system76.CosmicTerm",
    "xterm",
];
/// text-input-v3 purposes that open on the numbers page: digits, number, phone.
const NUMERIC_PURPOSES: [u32; 3] = [2, 3, 4];

struct Toplevel {
    app_id: String,
    activated: bool,
}

struct App {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: Option<SlotPool>,
    layer: Option<LayerSurface>,
    /// Logical px; 0 until configured.
    width: u32,
    height: u32,
    scale: i32,
    osk: Osk,
    grid: Vec<Placed>,
    fonts: draw::Fonts,
    keymap: Option<Keymap>,
    layouts: Vec<String>,
    active_layout: u32,
    uinput: Option<uinput::Uinput>,
    im: Option<ZwpInputMethodV2>,
    pending_active: bool,
    pending_purpose: u32,
    /// `done` events so far; `commit` must echo it.
    im_serial: u32,
    toplevels: HashMap<ObjectId, Toplevel>,
    epoch: Instant,
    picker: emoji::Picker,
    recent_path: std::path::PathBuf,
    last_tick: f64,
}

fn main() {
    let conn = Connection::connect_to_env().expect("no wayland compositor");
    let (globals, mut queue) = registry_queue_init(&conn).expect("registry");
    let qh = queue.handle();

    let recent_path = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".local/state")))
        .unwrap_or_default()
        .join("springchick/osk-recent");
    let uinput = uinput::Uinput::new(sc_config::OSK_DEVICE_NAME)
        .map_err(|e| eprintln!("sc-osk: no /dev/uinput ({e}); keys won't type"))
        .ok();
    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor: CompositorState::bind(&globals, &qh).expect("wl_compositor"),
        layer_shell: LayerShell::bind(&globals, &qh).expect("layer shell"),
        shm: Shm::bind(&globals, &qh).expect("wl_shm"),
        pool: None,
        layer: None,
        width: 0,
        height: 0,
        scale: 1,
        osk: Osk::default(),
        grid: Vec::new(),
        fonts: draw::fonts(),
        keymap: Keymap::from_names("", ""),
        layouts: Vec::new(),
        active_layout: 0,
        uinput,
        im: None,
        pending_active: false,
        pending_purpose: 0,
        im_serial: 0,
        toplevels: HashMap::new(),
        epoch: Instant::now(),
        picker: emoji::Picker::new(emoji::load_recent(&recent_path)),
        recent_path,
        last_tick: 0.0,
    };
    let _ = globals.bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ToplevelsData);
    queue.roundtrip(&mut app).expect("roundtrip");
    bind_input_method(&globals, &qh, &mut app);
    app.refresh_layouts();
    if std::env::args().any(|a| a == "--show") {
        app.show(&qh);
    }

    // Not blocking_dispatch: a long press needs to wake without an event.
    loop {
        queue.dispatch_pending(&mut app).expect("wayland dispatch");
        queue.flush().expect("wayland flush");
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let frame = app.picker.animating().then(|| app.now() + 1.0 / 60.0);
        let timeout = app
            .osk
            .deadline()
            .into_iter()
            .chain(frame)
            .reduce(f64::min)
            .map(|d| {
                let s = (d - app.now()).max(0.0);
                rustix::event::Timespec {
                    tv_sec: s as i64,
                    tv_nsec: (s.fract() * 1e9) as i64,
                }
            });
        let ready = {
            let fd = guard.connection_fd();
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            rustix::event::poll(&mut fds, timeout.as_ref())
        };
        match ready {
            Ok(n) if n > 0 => {
                if let Err(e) = guard.read() {
                    if !matches!(e, wayland_client::backend::WaylandError::Io(ref io) if io.kind() == std::io::ErrorKind::WouldBlock)
                    {
                        panic!("wayland read: {e}");
                    }
                }
            }
            _ => drop(guard),
        }
        app.tick();
    }
}

fn bind_input_method(globals: &GlobalList, qh: &QueueHandle<App>, app: &mut App) {
    let Some(seat) = app.seat_state.seats().next() else {
        return;
    };
    match globals.bind::<ZwpInputMethodManagerV2, _, _>(qh, 1..=1, ImData) {
        Ok(manager) => app.im = Some(manager.get_input_method(&seat, qh, ImData)),
        Err(e) => eprintln!("sc-osk: no input-method-v2 ({e}); start with --show"),
    }
}

/// springchick's IPC. Elsewhere the keyboard keeps to layout 0.
fn ipc(line: &str) -> Option<String> {
    let path = std::env::var_os("SPRINGCHICK_IPC_SOCK")
        .map(Into::into)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(|d| std::path::PathBuf::from(d).join("springchick-ipc.sock"))
        })?;
    let mut stream = UnixStream::connect(path).ok()?;
    // The event loop waits on this: a wedged compositor must not hang the keys.
    let timeout = Some(std::time::Duration::from_millis(500));
    stream.set_read_timeout(timeout).ok()?;
    stream.set_write_timeout(timeout).ok()?;
    writeln!(stream, "{line}").ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    reply.strip_prefix("ok ").map(|r| r.trim_end().to_string())
}

impl App {
    fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    fn refresh_layouts(&mut self) {
        let names = self
            .keymap
            .as_ref()
            .map(Keymap::layout_names)
            .unwrap_or_default();
        self.active_layout = ipc("layout")
            .and_then(|r| r.split(' ').next()?.parse().ok())
            .filter(|&a: &u32| (a as usize) < names.len())
            .unwrap_or(0);
        self.layouts = names;
    }

    fn show(&mut self, qh: &QueueHandle<Self>) {
        if self.layer.is_some() {
            return;
        }
        let out_w = self
            .output_state
            .outputs()
            .next()
            .and_then(|o| self.output_state.info(&o)?.logical_size)
            .map_or(400, |(w, _)| w);
        let h = (out_w as f32 * 0.66).clamp(200.0, 320.0) as u32;
        let surface = self.compositor.create_surface(qh);
        let layer =
            self.layer_shell
                .create_layer_surface(qh, surface, Layer::Overlay, Some("osk"), None);
        layer.set_anchor(Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, h);
        layer.set_exclusive_zone(h as i32);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.layer = Some(layer);
        self.width = 0;
        self.height = h;
    }

    fn hide(&mut self) {
        let outs = self.osk.release_all();
        self.apply(&outs);
        self.layer = None;
        self.width = 0;
    }

    fn relayout(&mut self) {
        let Some(keymap) = &self.keymap else {
            return;
        };
        let layout = self.active_layout;
        let key = |code: u16, shift: bool| keymap.label(code, shift, layout);
        let reverse = keymap.reverse(layout);
        let find = |c: char| reverse.get(&c).copied();
        let name = self.layouts.get(layout as usize).map_or("", String::as_str);
        let labels = Labels {
            key: &key,
            find: &find,
            layout_name: name,
            layouts: self.layouts.len(),
        };
        self.picker
            .set_area(grid::emoji_area(self.width as f32, self.height as f32));
        self.grid = grid::layout(
            self.osk.page,
            self.osk.bar,
            self.osk.shift.on(),
            self.width as f32,
            self.height as f32,
            &labels,
        );
    }

    fn draw(&mut self) {
        let Some(layer) = &self.layer else { return };
        if self.width == 0 {
            return;
        }
        let (w, h) = (
            self.width as i32 * self.scale,
            self.height as i32 * self.scale,
        );
        let pool = self.pool.get_or_insert_with(|| {
            SlotPool::new((w * h * 4) as usize, &self.shm).expect("shm pool")
        });
        let Ok((buffer, canvas)) = pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888) else {
            return;
        };
        draw::draw(
            canvas,
            w,
            h,
            self.scale as f32,
            &self.grid,
            &self.osk,
            (self.osk.page == Page::Emoji).then_some(&self.picker),
            &mut self.fonts,
        );
        let surface = layer.wl_surface();
        surface.set_buffer_scale(self.scale);
        surface.damage_buffer(0, 0, w, h);
        let _ = buffer.attach_to(surface);
        layer.commit();
    }

    fn tick(&mut self) {
        let now = self.now();
        let dt = (now - self.last_tick).min(0.05) as f32;
        self.last_tick = now;
        if self.picker.animating() {
            self.picker.tick(dt);
            self.draw();
        }
        let Some(keymap) = &self.keymap else { return };
        let layout = self.active_layout;
        let variants = |key, shifted| match key {
            grid::Key::Char { code, .. } => keymap
                .label(code, shifted, layout)
                .map(|l| accents::variants(&l))
                .unwrap_or_default(),
            grid::Key::Text(c) => accents::variants(&c.to_string()),
            _ => Vec::new(),
        };
        if self.osk.tick(self.now(), self.width as f32, variants) {
            self.draw();
        }
    }

    fn apply(&mut self, outs: &[Out]) {
        for out in outs {
            match out {
                &Out::Press(code) | &Out::Release(code) => {
                    if let Some(u) = &mut self.uinput {
                        if let Err(e) = u.key(code, matches!(out, Out::Press(_))) {
                            eprintln!("sc-osk: uinput write: {e}");
                        }
                    }
                }
                Out::Text(text) => {
                    if let Some(im) = &self.im {
                        im.commit_string(text.clone());
                        im.commit(self.im_serial);
                    }
                }
                Out::NextLayout => {
                    let n = self.layouts.len() as u32;
                    if n > 1 {
                        let next = (self.active_layout + 1) % n;
                        if ipc(&format!("layout {next}")).is_some() {
                            self.active_layout = next;
                        }
                    }
                }
            }
        }
    }

    fn touch_down(&mut self, id: i32, (x, y): (f64, f64)) {
        let at = (x as f32, y as f32);
        if self.osk.page == Page::Emoji && self.picker.area().contains(at.0, at.1) {
            self.picker.down(id, at, self.now());
            return;
        }
        let Some(key) = grid::hit(&self.grid, at.0, at.1) else {
            return;
        };
        let outs = self.osk.down(id, key, at, self.now());
        self.apply(&outs);
        match key {
            grid::Key::Page(Page::Emoji) => self.picker.reset(),
            grid::Key::Category(i) => self.picker.jump(i as usize),
            _ => {}
        }
        self.relayout();
        self.draw();
    }

    fn touch_motion(&mut self, id: i32, (x, y): (f64, f64)) {
        if self.picker.owns(id) {
            self.picker.motion(id, (x as f32, y as f32), self.now());
            self.draw();
            return;
        }
        let selected = self.osk.popup.as_ref().map(|p| p.selected);
        let outs = self.osk.motion(id, (x as f32, y as f32));
        if !outs.is_empty() {
            self.apply(&outs);
            // A shift latch used up by the swipe changes the labels.
            self.relayout();
            self.draw();
        } else if self.osk.popup.as_ref().map(|p| p.selected) != selected {
            self.draw();
        }
    }

    fn touch_up(&mut self, id: i32) {
        if self.picker.owns(id) {
            if let Some(e) = self.picker.up(id, self.now()) {
                self.apply(&[Out::Text(e)]);
                emoji::save_recent(&self.recent_path, &self.picker.recent);
            }
            self.last_tick = self.now();
            self.draw();
            return;
        }
        let outs = self.osk.up(id);
        self.apply(&outs);
        self.relayout();
        self.draw();
    }

    fn update_bar(&mut self) {
        let bar = if self
            .toplevels
            .values()
            .any(|t| t.activated && TERMINALS.contains(&t.app_id.as_str()))
        {
            Bar::Terminal
        } else {
            Bar::Default
        };
        if bar != self.osk.bar {
            self.osk.bar = bar;
            self.relayout();
            self.draw();
        }
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
        if factor != self.scale {
            self.scale = factor;
            self.draw();
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.hide();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.width = configure.new_size.0;
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }
        self.relayout();
        self.draw();
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
            // For the keymap only: a layer without interactivity never gets focus.
            Capability::Keyboard => {
                seat.get_keyboard(qh, KeymapData);
            }
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
        _: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        self.touch_down(id, position);
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
        self.touch_up(id);
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
        self.touch_motion(id, position);
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
        let outs = self.osk.release_all();
        self.apply(&outs);
        self.draw();
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
                PointerEventKind::Press { .. } => self.touch_down(POINTER_TOUCH, e.position),
                PointerEventKind::Release { .. } => self.touch_up(POINTER_TOUCH),
                PointerEventKind::Motion { .. } => self.touch_motion(POINTER_TOUCH, e.position),
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

/// User data for the protocols sctk doesn't wrap.
struct KeymapData;
struct ImData;
struct ToplevelsData;

impl Dispatch2<wl_keyboard::WlKeyboard, App> for KeymapData {
    fn event(
        &self,
        app: &mut App,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        let wl_keyboard::Event::Keymap { fd, size, .. } = event else {
            return;
        };
        let mut text = String::with_capacity(size as usize);
        if std::fs::File::from(fd).read_to_string(&mut text).is_err() {
            return;
        }
        let text = text.trim_end_matches('\0').to_string();
        if let Some(k) = Keymap::from_string(text) {
            app.keymap = Some(k);
            app.refresh_layouts();
            app.relayout();
            app.draw();
        }
    }
}

impl Dispatch2<ZwpInputMethodManagerV2, App> for ImData {
    fn event(
        &self,
        _: &mut App,
        _: &ZwpInputMethodManagerV2,
        _: <ZwpInputMethodManagerV2 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<ZwpInputMethodV2, App> for ImData {
    fn event(
        &self,
        app: &mut App,
        _: &ZwpInputMethodV2,
        event: zwp_input_method_v2::Event,
        _: &Connection,
        qh: &QueueHandle<App>,
    ) {
        use zwp_input_method_v2::Event;
        match event {
            Event::Activate => {
                app.pending_active = true;
                app.pending_purpose = 0;
            }
            Event::Deactivate => app.pending_active = false,
            Event::ContentType { purpose, .. } => {
                app.pending_purpose = match purpose {
                    WEnum::Value(p) => p.into(),
                    WEnum::Unknown(p) => p,
                };
            }
            Event::Done => {
                app.im_serial += 1;
                let shown = app.layer.is_some();
                if app.pending_active && !shown {
                    app.osk.page = if NUMERIC_PURPOSES.contains(&app.pending_purpose) {
                        Page::Numbers
                    } else {
                        Page::Letters
                    };
                    app.show(qh);
                } else if !app.pending_active && shown {
                    app.hide();
                }
            }
            Event::Unavailable => {
                eprintln!("sc-osk: another input method holds the seat; start with --show");
                app.im = None;
            }
            _ => {}
        }
    }
}

impl Dispatch2<ZwlrForeignToplevelManagerV1, App> for ToplevelsData {
    fn event(
        &self,
        _: &mut App,
        _: &ZwlrForeignToplevelManagerV1,
        _: zwlr_foreign_toplevel_manager_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }

    fn event_created_child(
        opcode: u16,
        qh: &QueueHandle<App>,
    ) -> Arc<dyn wayland_client::backend::ObjectData> {
        assert_eq!(
            opcode,
            zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE
        );
        qh.make_data::<ZwlrForeignToplevelHandleV1, _>(ToplevelsData)
    }
}

impl Dispatch2<ZwlrForeignToplevelHandleV1, App> for ToplevelsData {
    fn event(
        &self,
        app: &mut App,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let entry = app.toplevels.entry(handle.id()).or_insert(Toplevel {
            app_id: String::new(),
            activated: false,
        });
        match event {
            Event::AppId { app_id } => entry.app_id = app_id,
            Event::State { state } => {
                entry.activated = state
                    .chunks_exact(4)
                    .any(|s| u32::from_ne_bytes([s[0], s[1], s[2], s[3]]) == 2);
            }
            Event::Done => app.update_bar(),
            Event::Closed => {
                app.toplevels.remove(&handle.id());
                handle.destroy();
                app.update_bar();
            }
            _ => {}
        }
    }
}

delegate_registry!(App);
delegate_dispatch2!(App);
