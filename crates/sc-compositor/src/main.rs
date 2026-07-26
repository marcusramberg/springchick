//! springchick compositor — M3: gestures + app transitions.

mod app_history;
mod backend;
mod blank;
mod debug_input;
mod drm_backend;
mod frame_stats;
mod gamma_control;
mod input_common;
mod input_dispatch;
mod keybinds;
mod launcher;
mod layer_shell;
mod osd;
mod render;
pub mod scene;
mod skia_gl;
mod switcher;
mod touch;
pub mod ui_state;

use app_history::AppHistory;
use launcher::spawn_app;
use scene::compute_scene;
use skia_gl::SkiaGl;
use ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};

use sc_config::{catalog, state as config_state, AppEntry};
use sc_icons::IconPixels;
use sc_shell_model::ShellModel;

use smithay::backend::input::{Event, InputEvent, KeyboardKeyEvent};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::backend::SwapBuffersError;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_shell;
use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, ListeningSocket};
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::reexports::winit::window::Window as WinitWindow;
use smithay::utils::{Rectangle, Serial, Transform, SERIAL_COUNTER};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::Format as DrmFormat;
use smithay::backend::renderer::ImportDma;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::{
    with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState,
};
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler,
};
use smithay::wayland::selection::ext_data_control::{
    DataControlHandler, DataControlState,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info, warn};

/// `$HOME`, or `/tmp` as a last resort so path construction never panics.
fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
fn config_home() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home_dir().join(".config"))
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
/// XDG base data directories, highest precedence first:
/// `$XDG_DATA_HOME` (default `~/.local/share`), then each `$XDG_DATA_DIRS`
/// entry (default `/usr/local/share:/usr/share`) left-to-right.
fn xdg_data_dirs() -> Vec<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local/share"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());

    let mut dirs = vec![data_home];
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs
}

/// Config file path.
fn config_path() -> PathBuf {
    config_home().join("springchick/state.toml")
}

/// Scan .desktop files from standard locations.
fn scan_apps() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Search `<datadir>/applications` for each XDG data dir. Dirs are ordered
    // highest precedence first, so the first .desktop seen for a given id wins.
    for dir in xdg_data_dirs() {
        let Ok(read) = std::fs::read_dir(dir.join("applications")) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Some(app) = catalog::parse_desktop(&path, &contents) {
                        if seen.insert(app.id.clone()) {
                            entries.push(app);
                        }
                    }
                }
            }
        }
    }
    entries
}

/// A running app's toplevel state.
struct AppToplevel {
    surface: ToplevelSurface,
    app_id: String,
}

/// Backend-agnostic render snapshot produced by [`State::advance_frame`]. Holds
/// everything both backends feed into [`render::DrawCtx`]; each backend adds
/// only its own output transform, Skia flip, and framebuffer binding.
pub(crate) struct FramePrep {
    pub scene: scene::Scene,
    pub app_surface: Option<WlSurface>,
    pub frame_time: u32,
    pub osd_view: Option<(f32, bool, f32)>,
    pub bar_alpha: f32,
    pub layers_below: layer_shell::RenderList,
    pub layers_above: layer_shell::RenderList,
}

/// Main compositor state.
struct State {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    #[allow(dead_code)] // Must stay alive to keep the global registered.
    xdg_decoration_state: XdgDecorationState,
    shm_state: ShmState,
    /// zwp_linux_dmabuf: lets GL clients (GTK4, etc.) share buffers zero-copy
    /// instead of falling back to slow shm software upload. The global is
    /// created by the backend once its renderer's importable formats are known.
    dmabuf_state: DmabufState,
    #[allow(dead_code)] // Must stay alive to keep the dmabuf global registered.
    dmabuf_global: Option<DmabufGlobal>,
    data_device_state: DataDeviceState,
    /// ext-data-control clipboard-manager protocol state.
    data_control_state: DataControlState,
    seat_state: SeatState<Self>,
    #[allow(dead_code)] // Must stay alive to keep the wl_seat global registered.
    seat: Seat<Self>,
    /// Seat keyboard. Owned by State (not the winit loop) so both backends
    /// share one key path.
    keyboard: smithay::input::keyboard::KeyboardHandle<Self>,
    /// Surface currently holding keyboard focus, to avoid re-sending it.
    focused_surface: Option<WlSurface>,
    /// Resolved keybindings + in-flight press state.
    keys: keybinds::Keys,
    /// Panel blanking (acted on by the DRM backend; inert under winit).
    blank: blank::Blank,
    /// Set when a client commit or input changed on-screen state, so the
    /// vblank-driven DRM loop re-primes a page-flip on the next wake. Inert
    /// under winit (which renders every loop iteration).
    needs_render: bool,
    /// Commit cursor for the partial page-flip damage hint: the fullscreen app
    /// surface and the `CommitCounter` last presented for it. See
    /// [`crate::render::DrawCtx::last_present`].
    last_present: Option<(WlSurface, smithay::backend::renderer::utils::CommitCounter)>,
    /// Volume on-screen display state.
    osd: osd::Osd,
    /// wlr-layer-shell protocol state.
    layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    /// `wp_fractional_scale` + `wp_viewporter`: how HiDPI-unaware clients (layer
    /// surfaces like wvkbd, and apps) learn to render at `dpi`. Held only to keep
    /// the globals advertised for the compositor's lifetime.
    #[allow(dead_code)]
    fractional_scale_manager_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    viewporter_state: ViewporterState,
    /// Tracked layer surfaces + reserved-area bookkeeping.
    layers: layer_shell::LayerShell,
    /// Seat touch handle, for forwarding taps to layer surfaces.
    touch: smithay::input::touch::TouchHandle<Self>,
    /// Which layer surface (if any) currently owns the touch sequence.
    touch_grab: Option<WlSurface>,
    /// Coordinate scale of the surface currently receiving forwarded input
    /// (`dpi` — OSK layer surfaces render at fractional scale `dpi`, apps at
    /// output scale `dpi`). Physical input coords are divided by this to reach
    /// the surface's logical space.
    input_scale: f64,
    /// Whether the pointer press is currently held on a client surface.
    pointer_grab: bool,
    /// Home-bar opacity, faded to 0 when a bottom exclusive-zone surface (the
    /// on-screen keyboard) covers it.
    bar_alpha: f32,

    // Shell state
    ui: UiState,
    model: ShellModel,
    app_catalog: HashMap<String, AppEntry>,
    icon_cache: HashMap<String, IconPixels>,
    toplevels: Vec<Option<AppToplevel>>,
    children: Vec<Child>,
    history: AppHistory,
    /// Last zoom origin (cached when launching).
    last_origin: ZoomOrigin,
    /// Actual output size in physical pixels (from the backend: DRM mode or
    /// winit window size). Set at construction; drives layout and app sizing.
    output_size: (i32, i32),
    /// The advertised output. Retained so surfaces can `enter` it (which is how
    /// clients learn the scale factor).
    output: Output,
    /// Output scale (`[main].dpi`). Client buffers are `logical * dpi`, so xdg
    /// configure sizes are physical/dpi.
    dpi: i32,
    /// wlr-gamma-control state (night-light / color-temperature clients).
    gamma: gamma_control::GammaControl,

    // Rendering
    skia: SkiaGl,
    wayland_socket: String,

    // Input
    last_pointer_pos: Option<(f32, f32)>,
    pointer_down: bool,
    /// Page drag tracking: start_x when dragging on home screen.
    page_drag_start: Option<f32>,
    /// Bar drag tracking from Home state: (start_x, start_y).
    bar_drag_start: Option<(f32, f32)>,
    /// App icon held on Home, pending tap-to-launch (also drives the press
    /// highlight). Cleared if the finger moves into a page swipe.
    pending_launch: Option<input_common::PendingLaunch>,
    /// Switcher deck drag state.
    switcher_drag: input_common::SwitcherDrag,
    /// Switcher card rects for hit-testing during drag.
    switcher_cards: Vec<switcher::CardRect>,
    /// In-flight synthetic swipe from the debug socket (dev harness).
    active_gesture: Option<debug_input::ActiveGesture>,
    /// In-flight synthetic key hold from the debug socket (dev harness).
    active_key: Option<debug_input::ActiveKey>,
    active_touch: Option<debug_input::ActiveTouch>,
    /// Pending debug `settle`: reply channel + deadline.
    pending_settle: Option<(std::sync::mpsc::SyncSender<String>, std::time::Instant)>,
    /// Last logged UI state discriminant (to avoid spam).
    last_log_state: Option<std::mem::Discriminant<UiState>>,

    // Timing
    start_time: std::time::Instant,

    // Perf instrumentation
    stats: frame_stats::FrameStats,
    perf_log: bool,
    last_perf_log: std::time::Instant,

    // Control
    running: bool,
}

impl State {
    fn new(display: &Display<Self>, wayland_socket: String, output_size: (i32, i32)) -> Self {
        let dh = display.handle();
        let (out_w, out_h) = output_size;

        // v6 so clients like wvkbd that bind wl_compositor@6 can connect.
        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        // Global created later by the backend via `init_dmabuf_global`, once the
        // renderer's importable formats are known.
        let dmabuf_state = DmabufState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // ext_data_control: clipboard managers (wl-clipboard, dms, ...). No
        // primary selection wired, so pass None.
        let data_control_state = DataControlState::new::<Self, _>(&dh, None, |_client| true);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "springchick");
        // 200ms delay / 25Hz repeat: xkb defaults, forwarded to clients.
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 200, 25)
            .expect("add keyboard");
        seat.add_pointer();
        let touch = seat.add_touch();

        let layer_shell_state =
            smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<Self>(&dh);
        // Fractional scale + viewporter: HiDPI-unaware clients (wvkbd and other
        // layer surfaces, and apps) render at `[main].dpi` by being told a
        // fractional scale rather than an integer output scale (which wvkbd
        // ignores). See `FractionalScaleHandler` below.
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        // Virtual keyboard (on-screen keyboards like wvkbd). smithay's built-in
        // handler works now that we're on smithay-git + xkbcommon 0.9, which
        // fixed the keymap-size off-by-one that used to truncate wvkbd's uploaded
        // keymap (xkbcommon 0.8 did `new_from_buffer(.., size - 1, ..)`).
        smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState::new::<Self, _>(
            &dh,
            |_client| true,
        );

        // Advertise an output so clients know the display geometry.
        let output = Output::new(
            "springchick-0".into(),
            PhysicalProperties {
                size: (70, 155).into(), // ~FP5 physical mm
                subpixel: Subpixel::Unknown,
                make: "springchick".into(),
                model: "dev".into(),
                serial_number: "0".into(),
            },
        );
        let mode = OutputMode {
            size: (out_w, out_h).into(),
            refresh: 90_000, // 90 Hz in mHz
        };
        let dpi = keybinds::load_dpi().max(1) as i32;
        output.change_current_state(
            Some(mode),
            None,
            Some(smithay::output::Scale::Integer(dpi)),
            None,
        );
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);

        // wlr-gamma-control: advertise the manager global. 256 is a mock LUT
        // size for the winit backend; the DRM backend overrides it with the
        // real CRTC gamma_length before clients connect.
        let gamma = gamma_control::GammaControl::new(&dh, 256);

        // Load shell model + app catalog.
        let model = config_state::load(&config_path()).unwrap_or_default();
        let apps = scan_apps();
        let app_catalog: HashMap<String, AppEntry> =
            apps.into_iter().map(|e| (e.id.clone(), e)).collect();

        // Place any apps from catalog that aren't already in the model.
        let mut model = model;
        let existing: std::collections::HashSet<String> = model
            .pages
            .iter()
            .flatten()
            .chain(model.dock.iter())
            .cloned()
            .collect();
        for id in app_catalog.keys() {
            if !existing.contains(id) {
                model.place(id.clone());
            }
        }

        // Pre-resolve icons.
        let mut icon_cache = HashMap::new();
        for (id, entry) in &app_catalog {
            icon_cache.insert(id.clone(), sc_icons::resolve(&entry.icon));
        }

        let page_count = model.pages.len().max(1);
        let ui = UiState::home(0, page_count);

        State {
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            shm_state,
            dmabuf_state,
            dmabuf_global: None,
            data_device_state,
            data_control_state,
            seat_state,
            seat,
            keyboard,
            focused_surface: None,
            keys: keybinds::Keys::load(),
            blank: blank::Blank::new(),
            needs_render: false,
            last_present: None,
            osd: osd::Osd::new(),
            layer_shell_state,
            fractional_scale_manager_state,
            viewporter_state,
            layers: layer_shell::LayerShell::new(out_w as f32, out_h as f32),
            touch,
            touch_grab: None,
            input_scale: 1.0,
            pointer_grab: false,
            bar_alpha: 1.0,
            ui,
            model,
            app_catalog,
            icon_cache,
            toplevels: Vec::new(),
            children: Vec::new(),
            history: AppHistory::new(),
            last_origin: ZoomOrigin::icon((out_w as f32 / 2.0, out_h as f32 / 2.0)),
            output_size,
            output,
            dpi,
            gamma,
            skia: SkiaGl::new(),
            wayland_socket,
            last_pointer_pos: None,
            pointer_down: false,
            page_drag_start: None,
            bar_drag_start: None,
            pending_launch: None,
            switcher_drag: input_common::SwitcherDrag::None,
            switcher_cards: Vec::new(),
            active_gesture: None,
            active_key: None,
            active_touch: None,
            pending_settle: None,
            last_log_state: None,
            start_time: std::time::Instant::now(),
            stats: frame_stats::FrameStats::new(std::time::Duration::from_micros(11_111)),
            perf_log: false, // disabled for debugging
            last_perf_log: std::time::Instant::now(),
            running: true,
        }
    }

    /// Advertise `zwp_linux_dmabuf` with the formats the backend's renderer can
    /// import. Called once per backend after the renderer exists, so GL clients
    /// negotiate zero-copy buffers instead of falling back to shm.
    fn init_dmabuf_global(
        &mut self,
        dh: &DisplayHandle,
        formats: impl IntoIterator<Item = DrmFormat>,
    ) {
        let global = self.dmabuf_state.create_global::<Self>(dh, formats);
        self.dmabuf_global = Some(global);
    }

    fn handle_return_home(&mut self) {
        transition(
            &mut self.ui,
            UiEvent::ReturnHome {
                origin: self.last_origin,
            },
        );
    }

    fn launch_or_raise(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;
        // Check if already running — raise it (no zoom, instant).
        for (idx, slot) in self.toplevels.iter().enumerate() {
            if let Some(tl) = slot {
                if tl.app_id == app_id {
                    self.history.push_foreground(idx);
                    transition(
                        &mut self.ui,
                        UiEvent::RaiseApp {
                            toplevel: idx,
                            app_id: app_id.to_string(),
                        },
                    );
                    return;
                }
            }
        }

        // Launch new.
        if let Some(entry) = self.app_catalog.get(app_id) {
            let exec = entry.exec.clone();
            if let Some(child) = spawn_app(&exec, &self.wayland_socket) {
                self.children.push(child);
            }
        }
    }

    fn register_toplevel(&mut self, surface: ToplevelSurface) -> ToplevelId {
        // Try to match by app_id from the surface.
        let wl_app_id = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.lock().ok().and_then(|d| d.app_id.clone()))
        })
        .unwrap_or_default();

        let app_id = if !wl_app_id.is_empty() && self.app_catalog.contains_key(&wl_app_id) {
            wl_app_id
        } else {
            format!("unknown_{}", self.toplevels.len())
        };

        // Enter the output so the client receives its scale factor (`[main].dpi`)
        // and renders a HiDPI buffer instead of 1:1.
        self.output.enter(surface.wl_surface());

        let id = self.toplevels.len();
        self.toplevels.push(Some(AppToplevel {
            surface,
            app_id: app_id.clone(),
        }));

        self.history.push_foreground(id);
        transition(
            &mut self.ui,
            UiEvent::AppMapped {
                toplevel: id,
                app_id,
                origin: self.last_origin,
            },
        );

        id
    }

    fn unregister_toplevel(&mut self, surface: &WlSurface) {
        let mut closed_id = None;
        for (idx, slot) in self.toplevels.iter_mut().enumerate() {
            if let Some(tl) = slot {
                if tl.surface.wl_surface() == surface {
                    closed_id = Some(idx);
                    *slot = None;
                    break;
                }
            }
        }
        if let Some(id) = closed_id {
            self.close_toplevel(id);
        }
    }

    /// Close a toplevel by id (remove from vec, notify UI state).
    /// Recompute layer-surface geometry + reserved area. If the area apps may
    /// use changed, resize the toplevels to fit around it (e.g. above an OSK).
    fn recompute_layers(&mut self) {
        let (ow, oh) = self.output_size_f();
        let before = self.layers.usable;
        let after = self.layers.recompute(ow, oh, self.dpi);
        if after != before {
            self.reconfigure_toplevels();
        }
    }

    /// Send every app toplevel a configure at the current usable size, so apps
    /// render within the area not covered by exclusive-zone layer surfaces.
    fn reconfigure_toplevels(&mut self) {
        // Logical size: client scales its buffer up by `dpi`.
        let size = (
            (self.layers.usable.w.round() as i32) / self.dpi,
            (self.layers.usable.h.round() as i32) / self.dpi,
        );
        for slot in self.toplevels.iter().flatten() {
            slot.surface.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            slot.surface.send_configure();
        }
    }

    /// Bar fade target: 0 when a Top/Overlay layer surface (the OSK) covers the
    /// bar, else 1.
    fn bar_alpha_target(&self) -> f32 {
        let (w, h) = self.output_size_f();
        let bar = sc_layout::bar_rect(w, h);
        if self.layers.top_overlaps(bar) {
            0.0
        } else {
            1.0
        }
    }

    /// Step the home-bar fade toward its target and return the current alpha.
    /// ~0.13s fade (0.15 per 90Hz frame).
    fn tick_bar_alpha(&mut self) -> f32 {
        let target = self.bar_alpha_target();
        let step = 0.15;
        if (self.bar_alpha - target).abs() <= step {
            self.bar_alpha = target;
        } else if self.bar_alpha < target {
            self.bar_alpha += step;
        } else {
            self.bar_alpha -= step;
        }
        self.bar_alpha
    }

    /// True while the bar fade is still animating (keeps the DRM loop rendering).
    fn bar_fading(&self) -> bool {
        (self.bar_alpha - self.bar_alpha_target()).abs() > f32::EPSILON
    }

    /// Close whatever app is in front, if any. Backs the `close-app` binding.
    fn close_front_app(&mut self) {
        let Some(id) = ui_state::desired_focus(&self.ui) else {
            return;
        };
        self.detach_toplevel(id);
        transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id });
    }

    /// Push `desired_focus` into the seat keyboard when it changed. Cheap enough
    /// to call every frame; the comparison keeps it from re-sending focus.
    fn sync_keyboard_focus(&mut self) {
        let want = ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone());
        if want == self.focused_surface {
            return;
        }
        self.focused_surface = want.clone();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
    }

    fn close_toplevel(&mut self, id: ToplevelId) {
        self.detach_toplevel(id);
        transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id });
    }

    /// Ask the client to close, without emitting a `ToplevelClosed` UI
    /// transition. Used when the UI has already removed the card (e.g. switcher
    /// swipe-to-close). The slot is kept until the client actually exits and
    /// `toplevel_destroyed` fires — dropping the `ToplevelSurface` here would
    /// destroy the xdg resource before the queued `close` event is flushed.
    pub(crate) fn detach_toplevel(&mut self, id: ToplevelId) {
        if let Some(Some(tl)) = self.toplevels.get(id) {
            tl.surface.send_close();
        }
        self.history.remove(id);
    }

    /// Output size as floats — shorthand for the `(w, h)` pair every geometry
    /// call needs.
    fn output_size_f(&self) -> (f32, f32) {
        (self.output_size.0 as f32, self.output_size.1 as f32)
    }

    /// Raise `tid` to the foreground with a screen-centered zoom origin,
    /// recording it as the most-recent app. Backs the bar swipe-up and the bar
    /// horizontal quick-switch, which differ only in which toplevel they pick.
    pub(crate) fn raise_toplevel_centered(&mut self, tid: ToplevelId) {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let app_id = tl.app_id.clone();
        let (w, h) = self.output_size_f();
        self.last_origin = ZoomOrigin::icon((w / 2.0, h / 2.0));
        self.history.push_foreground(tid);
        transition(
            &mut self.ui,
            UiEvent::RaiseApp {
                toplevel: tid,
                app_id,
            },
        );
    }

    /// Advance the shell by one frame and produce the render snapshot: tick the
    /// springs, apply any resulting effect, refresh `page_count`, compute the
    /// scene, and gather the app surface, OSD, bar fade, and layer lists. Shared
    /// by the winit and DRM backends, which differ only in how they present the
    /// resulting frame.
    fn advance_frame(&mut self, dt: f32) -> FramePrep {
        let effect = transition(&mut self.ui, UiEvent::Tick { dt });
        match effect {
            ui_state::Effect::CloseToplevel { toplevel } => {
                self.close_toplevel(toplevel);
            }
            ui_state::Effect::EnterSwitcher => {
                let cards = self.history.mru_list();
                info!(target: "springchick::debug", "Effect::EnterSwitcher mru_list={:?}", cards);
                transition(&mut self.ui, UiEvent::EnterSwitcher { cards });
            }
            _ => {}
        }

        // Animations that settle to home reset page_count to 1; restore from the model.
        if let UiState::Home { page_count, .. } = &mut self.ui {
            *page_count = self.model.pages.len().max(1);
        }

        let scene = compute_scene(&self.ui, self.output_size);
        self.switcher_cards = scene.cards.clone();
        let disc = std::mem::discriminant(&self.ui);
        if self.last_log_state != Some(disc) {
            self.last_log_state = Some(disc);
            info!(target: "springchick::debug", "state changed to {:?} cards={}", self.ui, scene.cards.len());
        }

        let app_surface = scene.window.as_ref().and_then(|(tid, _)| {
            self.toplevels
                .get(*tid)
                .and_then(|slot| slot.as_ref())
                .map(|tl| tl.surface.wl_surface().clone())
        });

        let frame_time = self.start_time.elapsed().as_millis() as u32;
        let osd_now = std::time::Instant::now();
        let osd_view = self
            .osd
            .is_active(osd_now)
            .then(|| (self.osd.level, self.osd.muted, self.osd.alpha(osd_now)));
        let bar_alpha = self.tick_bar_alpha();
        let (layers_below, layers_above) = self.layers.render_lists();

        FramePrep {
            scene,
            app_surface,
            frame_time,
            osd_view,
            bar_alpha,
            layers_below,
            layers_above,
        }
    }
}

// --- Smithay handler impls ---

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        // A client presented new content; ask the DRM loop to render. Without
        // this, an app committing while the screen is otherwise idle never
        // gets its frame callback (only sent during a render), so it stalls.
        self.needs_render = true;

        // A layer surface committing may change its geometry or reserved area.
        if self
            .layers
            .surfaces
            .iter()
            .any(|m| m.surface.wl_surface() == surface)
        {
            self.recompute_layers();
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Size to the usable area (output minus any exclusive-zone reservations),
        // so an app that opens while an OSK is up already fits above it.
        // xdg sizes are logical; the client scales its buffer up by `dpi`.
        let w = (self.layers.usable.w.round() as i32) / self.dpi;
        let h = (self.layers.usable.h.round() as i32) / self.dpi;
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.decoration_mode = Some(DecorationMode::ServerSide);
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        self.register_toplevel(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unregister_toplevel(surface.wl_surface());
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        // Send the initial configure. wvkbd (and other layer-shell OSKs) create
        // an xdg_popup child and ignore ALL input until that popup is
        // configured, so without this the on-screen keyboard never registers a
        // tap.
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
        });
        if let Err(e) = surface.send_configure() {
            warn!(?e, "failed to configure popup");
        }
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // The advertised formats come straight from the backend renderer's
        // importable set, so accept optimistically; the shared render path
        // imports the buffer lazily when it first composites the surface.
        let _ = notifier.successful::<State>();
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control_state
    }
}

// Phone shell: no server-initiated DnD. The default `dnd_requested` cancels the
// source, which is what we want.
impl WaylandDndGrabHandler for State {}

impl OutputHandler for State {}

impl FractionalScaleHandler for State {
    /// A client bound `wp_fractional_scale` for a surface: tell it to render at
    /// `dpi`. Constant here (single output), so one send at creation suffices.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.dpi as f64;
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

/// Force a toplevel to server-side decorations (= no CSD) and configure it.
/// Every xdg-decoration request resolves the same way, regardless of what the
/// client asked for.
fn force_server_side_decoration(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|state| {
        state.decoration_mode = Some(DecorationMode::ServerSide);
    });
    toplevel.send_configure();
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        force_server_side_decoration(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        force_server_side_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        force_server_side_decoration(&toplevel);
    }
}

delegate_compositor!(State);
smithay::delegate_dmabuf!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_shm!(State);
delegate_data_device!(State);
smithay::delegate_ext_data_control!(State);
delegate_output!(State);
delegate_xdg_decoration!(State);
smithay::delegate_layer_shell!(State);
smithay::delegate_virtual_keyboard_manager!(State);
smithay::delegate_fractional_scale!(State);
smithay::delegate_viewporter!(State);

impl smithay::wayland::shell::wlr_layer::WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut smithay::wayland::shell::wlr_layer::WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: smithay::wayland::shell::wlr_layer::LayerSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
        layer: smithay::wayland::shell::wlr_layer::Layer,
        namespace: String,
    ) {
        info!(%namespace, ?layer, "new layer surface");
        // Note: layer surfaces (OSK) deliberately do NOT enter the output, so
        // they stay at scale 1. Their geometry (layer_rect) is computed in
        // physical px; entering the output would make them render at 1/dpi.
        self.layers.add(surface, layer);
        // Geometry + the initial configure happen on the next commit, once the
        // client's anchor/size/exclusive-zone state has arrived.
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        if self.layers.remove(&surface) {
            self.recompute_layers();
        }
    }
}

/// Per-client state.
#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        match reason {
            DisconnectReason::ConnectionClosed => {
                debug!(?client_id, "client disconnected (connection closed)")
            }
            other => warn!(?client_id, ?other, "client disconnected"),
        }
    }
}

// --- Main entry ---

fn main() {
    init_tracing();
    match backend::BackendKind::from_env() {
        backend::BackendKind::Winit => {
            info!("springchick M4 — winit dev backend");
            run_winit();
        }
        backend::BackendKind::Drm => {
            info!("springchick M4 — DRM device backend");
            drm_backend::run_drm();
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sc_compositor=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}

/// Create the Wayland display + an auto-bound listening socket. Shared by the
/// winit and DRM backends.
fn create_display() -> Result<(Display<State>, ListeningSocket, String), Box<dyn std::error::Error>>
{
    let display: Display<State> = Display::new()?;
    let listener = ListeningSocket::bind_auto("springchick", 0..32)?;
    let socket_name = listener
        .socket_name()
        .ok_or("wayland socket has no name")?
        .to_string_lossy()
        .to_string();
    info!(%socket_name, "wayland socket listening");
    Ok((display, listener, socket_name))
}

/// Publish the compositor's Wayland socket so clients can find it.
///
/// `WAYLAND_DISPLAY` goes into our own environment so directly-spawned children
/// (launched apps, keybinding commands) inherit it. When running as a real
/// session (`import_to_systemd`), it is also pushed into the systemd/dbus user
/// activation environment so user services — e.g. the on-screen keyboard
/// `wvkbd-mobintl` — connect to us instead of failing with "Failed to create
/// display". The winit dev backend skips the systemd import so it does not
/// clobber the host session's value.
fn publish_wayland_display(socket_name: &str, import_to_systemd: bool) {
    std::env::set_var("WAYLAND_DISPLAY", socket_name);
    if !import_to_systemd {
        return;
    }
    for (program, args) in [
        (
            "systemctl",
            vec!["--user", "import-environment", "WAYLAND_DISPLAY"],
        ),
        (
            "dbus-update-activation-environment",
            vec!["--systemd", "WAYLAND_DISPLAY"],
        ),
    ] {
        match std::process::Command::new(program).args(&args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => warn!(program, code = ?s.code(), "activation-environment update failed"),
            Err(e) => warn!(%e, program, "could not run activation-environment update"),
        }
    }
}

/// Accept one pending client on the listener, if any.
fn accept_client(display: &Display<State>, listener: &ListeningSocket) {
    if let Some(stream) = listener.accept().ok().flatten() {
        debug!("new wayland client connected");
        let _ = display
            .handle()
            .insert_client(stream, Arc::new(ClientState::default()));
    }
}

fn run_winit() {
    let (win_w, win_h) = backend::dev_window_size();
    info!(width = win_w, height = win_h, "starting winit dev backend");

    let attributes = WinitWindow::default_attributes()
        .with_title("springchick")
        .with_inner_size(LogicalSize::new(f64::from(win_w), f64::from(win_h)))
        .with_visible(true);

    let (mut gfx_backend, mut winit_evt) =
        match winit::init_from_attributes::<GlesRenderer>(attributes) {
            Ok(ret) => ret,
            Err(err) => {
                error!(err = ?err, "failed to initialize winit backend");
                return;
            }
        };

    // Create Wayland display + listening socket.
    let (mut display, listener, socket_name) = create_display().expect("create wayland display");
    // Dev backend: own env only, don't disturb the host session's user services.
    publish_wayland_display(&socket_name, false);

    // Build State with the actual backend window size (the host compositor may
    // have clamped our requested dev-window size).
    let actual_size = gfx_backend.window_size();
    info!(w = actual_size.w, h = actual_size.h, "actual output size");
    let mut state = State::new(&display, socket_name.clone(), (actual_size.w, actual_size.h));
    state.init_dmabuf_global(&display.handle(), gfx_backend.renderer().dmabuf_formats());

    // Optional debug input socket (dev/test harness). Inert unless env is set.
    let debug_chan = match std::env::var("SPRINGCHICK_DEBUG_SOCK") {
        Ok(path) => {
            match debug_input::spawn(
                &path,
                state.output_size.0 as f32,
                state.output_size.1 as f32,
            ) {
                Ok(chan) => {
                    info!(path = %path, "debug input socket listening");
                    Some((path, chan))
                }
                Err(e) => {
                    error!(%e, "failed to bind debug input socket");
                    None
                }
            }
        }
        Err(_) => None,
    };

    info!("entering frame loop");

    while state.running {
        // Accept new clients.
        accept_client(&display, &listener);

        // Pump winit events.
        let status = winit_evt.dispatch_new_events(|event| match event {
            WinitEvent::CloseRequested => {
                info!("window close requested");
                state.running = false;
            }
            WinitEvent::Input(input_event) => {
                handle_winit_input(&mut state, input_event);
            }
            WinitEvent::Resized { .. } | WinitEvent::Focus(_) | WinitEvent::Redraw => {}
        });

        if let PumpStatus::Exit(_) = status {
            state.running = false;
        }

        if !state.running {
            break;
        }

        // Drain debug input (dev harness) before rendering this frame.
        if let Some((_, chan)) = &debug_chan {
            debug_input::drain(&mut state, chan);
        }

        // Dispatch Wayland clients.
        display.dispatch_clients(&mut state).ok();
        display.flush_clients().ok();

        // Render.
        if let Err(err) = render_frame(&mut gfx_backend, &mut state) {
            match err {
                SwapBuffersError::ContextLost(err) => {
                    error!(%err, "context lost, exiting");
                    break;
                }
                other => warn!(%other, "transient render error"),
            }
        }

        // Sleep remainder of frame budget.
        // TODO: switch to calloop timer in a future cleanup.
        std::thread::sleep(Duration::from_millis(1));
    }

    // Remove the debug socket file (best-effort).
    if let Some((path, _)) = &debug_chan {
        let _ = std::fs::remove_file(path);
    }

    // Save state.
    if let Err(e) = config_state::save(&state.model, &config_path()) {
        warn!(%e, "failed to save shell model");
    }

    info!("compositor shut down");
}

/// Handle input events from the winit backend.
fn handle_winit_input(
    state: &mut State,
    event: smithay::backend::input::InputEvent<smithay::backend::winit::WinitInput>,
) {
    use smithay::backend::input::{AbsolutePositionEvent, ButtonState, PointerButtonEvent};

    match event {
        InputEvent::Keyboard { event } => {
            keybinds::on_key_event(state, event.key_code(), event.state(), event.time_msec());
        }
        InputEvent::PointerButton { event } => {
            let pressed = event.state() == ButtonState::Pressed;
            touch::pointer_button(state, pressed, event.button_code(), event.time_msec());
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let x = event.x_transformed(state.output_size.0) as f32;
            let y = event.y_transformed(state.output_size.1) as f32;
            touch::pointer_motion(state, x, y, event.time_msec());
        }
        _ => {}
    }
}

/// Render one frame.
fn render_frame(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    state: &mut State,
) -> Result<(), SwapBuffersError> {
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);
    let frame_start = std::time::Instant::now();

    // Fixed 90 Hz step for the dev backend.
    let prep = state.advance_frame(1.0 / 90.0);

    keybinds::poll(state);
    state.sync_keyboard_focus();

    let (renderer, mut framebuffer) = backend.bind()?;
    {
        let mut ctx = render::DrawCtx {
            scene: &prep.scene,
            app_surface: prep.app_surface.as_ref(),
            skia: &mut state.skia,
            model: &state.model,
            icon_cache: &state.icon_cache,
            app_catalog: &state.app_catalog,
            toplevels: &state.toplevels,
            app_scale: state.dpi as f64,
            transform: Transform::Flipped180,
            skia_flip_y: false,
            frame_time: prep.frame_time,
            osd: prep.osd_view,
            layers_below: &prep.layers_below,
            layers_above: &prep.layers_above,
            bar_alpha: prep.bar_alpha,
            pressed_app: state.pending_launch.as_ref().map(|p| p.app_id.as_str()),
            // winit dev backend submits full damage; no partial hint.
            report_partial_damage: false,
            last_present: &mut state.last_present,
        };
        render::draw_scene(renderer, &mut framebuffer, size, &mut ctx)?;
    }

    drop(framebuffer);
    let result = backend.submit(Some(&[damage]));

    // Record + periodically log frame timing.
    state.stats.record_frame(frame_start.elapsed());
    if state.perf_log && state.last_perf_log.elapsed() >= Duration::from_secs(1) {
        info!(target: "springchick::perf", "{}", state.stats.format_line());
        state.last_perf_log = std::time::Instant::now();
    }

    result
}
