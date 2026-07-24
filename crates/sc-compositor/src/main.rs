//! springchick compositor — M3: gestures + app transitions.

mod app_history;
mod backend;
mod blank;
mod debug_input;
mod drm_backend;
mod frame_stats;
mod input_common;
mod input_dispatch;
mod keybinds;
mod launcher;
mod render;
pub mod scene;
mod skia_gl;
mod switcher;
pub mod ui_state;

use app_history::AppHistory;
use backend::{FP5_HEIGHT, FP5_WIDTH};
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
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
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
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info, warn};

/// Frame budget (~90 Hz).
#[allow(dead_code)]
const FRAME_BUDGET: Duration = Duration::from_micros(11_111);

/// Whether to emit the per-second perf log line. Set via `SPRINGCHICK_PERF`;
/// the DRM backend additionally forces it on at startup.
fn perf_enabled() -> bool {
    std::env::var("SPRINGCHICK_PERF").is_ok()
}

/// Config file path.
fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".config")
        });
    base.join("springchick/state.toml")
}

/// Scan .desktop files from standard locations.
fn scan_apps() -> Vec<AppEntry> {
    let data_home = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".local/share")
        });

    let dirs = [
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        // NixOS system-wide
        PathBuf::from("/run/current-system/sw/share/applications"),
        // NixOS per-user profile
        PathBuf::from("/etc/profiles/per-user")
            .join(std::env::var("USER").unwrap_or_default())
            .join("share/applications"),
        data_home.join("applications"),
    ];
    let mut entries = Vec::new();
    for dir in &dirs {
        let Ok(read) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Some(app) = catalog::parse_desktop(&path, &contents) {
                        entries.push(app);
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

/// Main compositor state.
struct State {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    #[allow(dead_code)] // Must stay alive to keep the global registered.
    xdg_decoration_state: XdgDecorationState,
    shm_state: ShmState,
    data_device_state: DataDeviceState,
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
    /// Actual output size (may differ from FP5 constants in nested dev mode).
    output_size: (i32, i32),

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
    /// Switcher deck drag state.
    switcher_drag: input_common::SwitcherDrag,
    /// Switcher card rects for hit-testing during drag.
    switcher_cards: Vec<switcher::CardRect>,
    /// In-flight synthetic swipe from the debug socket (dev harness).
    active_gesture: Option<debug_input::ActiveGesture>,
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
    fn new(display: &Display<Self>, wayland_socket: String) -> Self {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "springchick");
        // 200ms delay / 25Hz repeat: xkb defaults, forwarded to clients.
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 200, 25)
            .expect("add keyboard");
        seat.add_pointer();

        // Advertise an output so clients know the display geometry.
        let output = Output::new(
            "springchick-0".into(),
            PhysicalProperties {
                size: (70, 155).into(), // ~FP5 physical mm
                subpixel: Subpixel::Unknown,
                make: "springchick".into(),
                model: "dev".into(),
            },
        );
        let mode = OutputMode {
            size: (FP5_WIDTH, FP5_HEIGHT).into(),
            refresh: 90_000, // 90 Hz in mHz
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(smithay::output::Scale::Integer(1)),
            None,
        );
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);

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
            data_device_state,
            seat_state,
            seat,
            keyboard,
            focused_surface: None,
            keys: keybinds::Keys::load(),
            blank: blank::Blank::new(),
            ui,
            model,
            app_catalog,
            icon_cache,
            toplevels: Vec::new(),
            children: Vec::new(),
            history: AppHistory::new(),
            last_origin: ZoomOrigin::icon((FP5_WIDTH as f32 / 2.0, FP5_HEIGHT as f32 / 2.0)),
            output_size: (FP5_WIDTH, FP5_HEIGHT),
            skia: SkiaGl::new(),
            wayland_socket,
            last_pointer_pos: None,
            pointer_down: false,
            page_drag_start: None,
            bar_drag_start: None,
            switcher_drag: input_common::SwitcherDrag::None,
            switcher_cards: Vec::new(),
            active_gesture: None,
            pending_settle: None,
            last_log_state: None,
            start_time: std::time::Instant::now(),
            stats: frame_stats::FrameStats::new(std::time::Duration::from_micros(11_111)),
            perf_log: false, // disabled for debugging
            last_perf_log: std::time::Instant::now(),
            running: true,
        }
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
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let (w, h) = self.output_size;
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

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

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

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl OutputHandler for State {}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Always request server-side decorations (= no CSD).
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_shm!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_xdg_decoration!(State);

/// Per-client state.
#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
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
    info!(
        width = FP5_WIDTH,
        height = FP5_HEIGHT,
        "starting winit dev backend"
    );

    let attributes = WinitWindow::default_attributes()
        .with_title("springchick")
        .with_inner_size(LogicalSize::new(
            f64::from(FP5_WIDTH),
            f64::from(FP5_HEIGHT),
        ))
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

    let mut state = State::new(&display, socket_name.clone());

    // Update output size from actual backend window dimensions.
    let actual_size = gfx_backend.window_size();
    state.output_size = (actual_size.w, actual_size.h);
    info!(w = actual_size.w, h = actual_size.h, "actual output size");

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
            if event.state() == ButtonState::Pressed {
                input_common::on_press(state);
            } else {
                input_common::on_release(state);
            }
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let x = event.x_transformed(state.output_size.0) as f32;
            let y = event.y_transformed(state.output_size.1) as f32;
            input_common::on_motion(state, x, y);
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

    // Tick animations.
    let dt = 1.0 / 90.0;
    let effect = transition(&mut state.ui, UiEvent::Tick { dt });
    match effect {
        ui_state::Effect::CloseToplevel { toplevel } => {
            state.close_toplevel(toplevel);
        }
        ui_state::Effect::EnterSwitcher => {
            let cards = state.history.mru_list();
            info!(target: "springchick::debug", "Effect::EnterSwitcher mru_list={:?}", cards);
            transition(&mut state.ui, UiEvent::EnterSwitcher { cards });
        }
        _ => {}
    }

    keybinds::poll(state);
    state.sync_keyboard_focus();

    // Animations that settle to home reset page_count to 1; restore from the model.
    if let UiState::Home { page_count, .. } = &mut state.ui {
        *page_count = state.model.pages.len().max(1);
    }

    // Compute scene from current state.
    let scene = compute_scene(&state.ui, state.output_size);
    state.switcher_cards = scene.cards.clone();
    let disc = std::mem::discriminant(&state.ui);
    if state.last_log_state != Some(disc) {
        state.last_log_state = Some(disc);
        info!(target: "springchick::debug", "state changed to {:?} cards={}", state.ui, scene.cards.len());
    }

    // Resolve the app surface for compositing.
    let app_surface: Option<WlSurface> = scene.window.as_ref().and_then(|(tid, _)| {
        state
            .toplevels
            .get(*tid)
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone())
    });

    let frame_time = state.start_time.elapsed().as_millis() as u32;
    let (renderer, mut framebuffer) = backend.bind()?;
    {
        let mut ctx = render::DrawCtx {
            scene: &scene,
            app_surface: app_surface.as_ref(),
            skia: &mut state.skia,
            model: &state.model,
            icon_cache: &state.icon_cache,
            app_catalog: &state.app_catalog,
            toplevels: &state.toplevels,
            transform: Transform::Flipped180,
            skia_flip_y: false,
            frame_time,
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
