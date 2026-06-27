//! springchick compositor — M3: gestures + app transitions.

mod app_history;
mod backend;
mod input_dispatch;
mod launcher;
pub mod scene;
mod skia_gl;
pub mod ui_state;

use app_history::AppHistory;
use backend::{FP5_HEIGHT, FP5_WIDTH};
use input_dispatch::DownAction;
use launcher::spawn_app;
use scene::compute_scene;
use skia_gl::SkiaGl;
use ui_state::{transition, ToplevelId, UiEvent, UiState};

use sc_config::{catalog, state as config_state, AppEntry};
use sc_icons::IconPixels;
use sc_shell_model::ShellModel;

use smithay::backend::input::{InputEvent, KeyboardKeyEvent, KeyState};
use smithay::backend::input::Keycode;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
use smithay::backend::renderer::element::utils::{RescaleRenderElement, RelocateRenderElement, Relocate};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::utils::{draw_render_elements, on_commit_buffer_handler};
use smithay::backend::renderer::{Color32F, Frame, Renderer};
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::backend::SwapBuffersError;
use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_shell;
use smithay::input::keyboard::{FilterResult, XkbConfig};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, ListeningSocket};
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::reexports::winit::window::Window as WinitWindow;
use smithay::utils::{Point, Physical, Rectangle, Serial, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_surface_tree_downward, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes, TraversalAction,
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

/// Background clear color.
const CLEAR_COLOR: Color32F = Color32F::new(0.06, 0.10, 0.14, 1.0);

/// Frame budget (~90 Hz).
#[allow(dead_code)]
const FRAME_BUDGET: Duration = Duration::from_micros(11_111);

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
    seat: Seat<Self>,

    // Shell state
    ui: UiState,
    model: ShellModel,
    app_catalog: HashMap<String, AppEntry>,
    icon_cache: HashMap<String, IconPixels>,
    toplevels: Vec<Option<AppToplevel>>,
    children: Vec<Child>,
    history: AppHistory,
    /// Last icon center for zoom-back (cached when launching).
    last_icon_center: (f32, f32),
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

    // Timing
    start_time: std::time::Instant,

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
        let seat = seat_state.new_wl_seat(&dh, "springchick");

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
        output.change_current_state(Some(mode), None, Some(smithay::output::Scale::Integer(1)), None);
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
            ui,
            model,
            app_catalog,
            icon_cache,
            toplevels: Vec::new(),
            children: Vec::new(),
            history: AppHistory::new(),
            last_icon_center: (FP5_WIDTH as f32 / 2.0, FP5_HEIGHT as f32 / 2.0),
            output_size: (FP5_WIDTH, FP5_HEIGHT),
            skia: SkiaGl::new(),
            wayland_socket,
            last_pointer_pos: None,
            pointer_down: false,
            page_drag_start: None,
            bar_drag_start: None,
            start_time: std::time::Instant::now(),
            running: true,
        }
    }

    fn handle_return_home(&mut self) {
        transition(
            &mut self.ui,
            UiEvent::ReturnHome {
                icon_center: self.last_icon_center,
            },
        );
    }

    fn launch_or_raise(&mut self, app_id: &str, icon_center: (f32, f32)) {
        self.last_icon_center = icon_center;
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
                icon_center: self.last_icon_center,
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
            transition(&mut self.ui, UiEvent::ToplevelClosed { toplevel: id });
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
    info!("springchick M3 — gestures + transitions");
    run_winit();
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sc_compositor=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}

fn run_winit() {
    info!(
        width = FP5_WIDTH,
        height = FP5_HEIGHT,
        "starting winit dev backend"
    );

    let attributes = WinitWindow::default_attributes()
        .with_title("springchick")
        .with_inner_size(LogicalSize::new(f64::from(FP5_WIDTH), f64::from(FP5_HEIGHT)))
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
    let mut display: Display<State> = Display::new().expect("create display");
    let listener = ListeningSocket::bind_auto("springchick", 0..32)
        .expect("bind wayland socket");
    let socket_name = listener
        .socket_name()
        .expect("socket has name")
        .to_string_lossy()
        .to_string();
    info!(%socket_name, "wayland socket listening");

    let mut state = State::new(&display, socket_name.clone());

    // Update output size from actual backend window dimensions.
    let actual_size = gfx_backend.window_size();
    state.output_size = (actual_size.w, actual_size.h);
    info!(w = actual_size.w, h = actual_size.h, "actual output size");

    // Add keyboard to seat.
    let keyboard = state
        .seat
        .add_keyboard(XkbConfig::default(), 200, 25)
        .expect("add keyboard");
    state.seat.add_pointer();

    info!("entering frame loop");
    let mut clients: Vec<Client> = Vec::new();

    while state.running {
        // Accept new clients.
        if let Some(stream) = listener.accept().ok().flatten() {
            debug!("new wayland client connected");
            if let Ok(client) = display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                clients.push(client);
            }
        }

        // Pump winit events.
        let status = winit_evt.dispatch_new_events(|event| match event {
            WinitEvent::CloseRequested => {
                info!("window close requested");
                state.running = false;
            }
            WinitEvent::Input(input_event) => {
                handle_winit_input(&mut state, &keyboard, input_event);
            }
            WinitEvent::Resized { .. } | WinitEvent::Focus(_) | WinitEvent::Redraw => {}
        });

        if let PumpStatus::Exit(_) = status {
            state.running = false;
        }

        if !state.running {
            break;
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

    // Save state.
    if let Err(e) = config_state::save(&state.model, &config_path()) {
        warn!(%e, "failed to save shell model");
    }

    info!("compositor shut down");
}

/// Handle input events from the winit backend.
fn handle_winit_input(
    state: &mut State,
    keyboard: &smithay::input::keyboard::KeyboardHandle<State>,
    event: smithay::backend::input::InputEvent<smithay::backend::winit::WinitInput>,
) {
    use smithay::backend::input::{
        AbsolutePositionEvent, ButtonState, PointerButtonEvent,
    };

    match event {
        InputEvent::Keyboard { event } => {
            let key_code = event.key_code();
            let key_state = event.state();

            // ESC in evdev = 1, XKB offset +8 = 9
            let esc_keycode: Keycode = 9u32.into();
            if key_code == esc_keycode
                && key_state == KeyState::Pressed
                && matches!(
                    state.ui,
                    UiState::App { .. } | UiState::Grabbing { .. } | UiState::Settling { .. }
                )
            {
                state.handle_return_home();
                return;
            }

            // Forward to focused client.
            keyboard.input::<(), _>(
                state,
                key_code,
                key_state,
                0.into(),
                0,
                |_, _, _| FilterResult::Forward,
            );
        }
        InputEvent::PointerButton { event } => {
            if let Some((x, y)) = state.last_pointer_pos {
                if event.state() == ButtonState::Pressed {
                    state.pointer_down = true;
                    let action = input_dispatch::on_press(&state.ui, x, y, &state.model, state.output_size);
                    match action {
                        DownAction::Event(ev) => {
                            transition(&mut state.ui, ev);
                        }
                        DownAction::LaunchApp { app_id, icon_center } => {
                            state.launch_or_raise(&app_id, icon_center);
                        }
                        DownAction::StartPageDrag { start_x } => {
                            state.page_drag_start = Some(start_x);
                        }
                        DownAction::StartBarDrag { start_x, start_y } => {
                            state.bar_drag_start = Some((start_x, start_y));
                        }
                        DownAction::None => {}
                    }
                } else {
                    state.pointer_down = false;
                    // Bar drag from Home: classify swipe direction.
                    if let Some((start_x, start_y)) = state.bar_drag_start.take() {
                        let dx = x - start_x;
                        let dy = start_y - y; // positive = swiped up
                        let w = state.output_size.0 as f32;
                        let h = state.output_size.1 as f32;

                        if dy > h * 0.08 {
                            // Swiped up from bar → raise most recent app.
                            if let Some(tid) = state.history.previous() {
                                if let Some(Some(tl)) = state.toplevels.get(tid) {
                                    let app_id = tl.app_id.clone();
                                    state.last_icon_center = (w / 2.0, h / 2.0);
                                    state.history.push_foreground(tid);
                                    transition(
                                        &mut state.ui,
                                        UiEvent::RaiseApp {
                                            toplevel: tid,
                                            app_id,
                                        },
                                    );
                                }
                            }
                        } else if dx.abs() > w * 0.15 {
                            // Horizontal swipe on bar → quick-switch.
                            let dir = if dx < 0.0 { 1 } else { -1 };
                            if let Some(tid) = state.history.quick_switch(dir) {
                                if let Some(Some(tl)) = state.toplevels.get(tid) {
                                    let app_id = tl.app_id.clone();
                                    state.last_icon_center = (w / 2.0, h / 2.0);
                                    state.history.push_foreground(tid);
                                    transition(
                                        &mut state.ui,
                                        UiEvent::RaiseApp {
                                            toplevel: tid,
                                            app_id,
                                        },
                                    );
                                }
                            }
                        }
                    }
                    // Page swipe: snap based on 30% threshold.
                    if let Some(start_x) = state.page_drag_start.take() {
                        let dx = x - start_x;
                        let w = state.output_size.0 as f32;
                        let page_delta = -dx / w; // positive = swiping to next page
                        if let UiState::Home { page, page_spring, page_count, .. } = &mut state.ui {
                            let target_page = if page_delta > 0.3 && *page + 1 < *page_count {
                                *page + 1
                            } else if page_delta < -0.3 && *page > 0 {
                                *page - 1
                            } else {
                                *page
                            };
                            *page = target_page;
                            page_spring.retarget(target_page as f32);
                        }
                    }
                    // Release grab if active.
                    let release = if let UiState::Grabbing { tracker, toplevel, app_id } = &state.ui {
                        Some((sc_input::classify_release(tracker), *toplevel, app_id.clone()))
                    } else {
                        None
                    };
                    if let Some((target, cur_tid, cur_app)) = release {
                        match target {
                            sc_input::NavTarget::QuickSwitch(dir) => {
                                // Grab-based quick-switch: raise the adjacent app directly.
                                let adj = state
                                    .history
                                    .quick_switch(dir)
                                    .filter(|tid| matches!(state.toplevels.get(*tid), Some(Some(_))));
                                match adj {
                                    Some(tid) => {
                                        let app_id =
                                            state.toplevels[tid].as_ref().unwrap().app_id.clone();
                                        state.history.push_foreground(tid);
                                        transition(
                                            &mut state.ui,
                                            UiEvent::RaiseApp { toplevel: tid, app_id },
                                        );
                                    }
                                    // No adjacent app — snap back to the current one.
                                    None => {
                                        transition(
                                            &mut state.ui,
                                            UiEvent::RaiseApp {
                                                toplevel: cur_tid,
                                                app_id: cur_app,
                                            },
                                        );
                                    }
                                }
                            }
                            _ => {
                                transition(&mut state.ui, UiEvent::GrabRelease);
                                // Settling toward Home/Switcher needs the real icon origin.
                                if let UiState::Settling { icon_center, .. } = &mut state.ui {
                                    *icon_center = state.last_icon_center;
                                }
                            }
                        }
                    }
                    // Update page_count after returning home.
                    if let UiState::Home { page_count, .. } = &mut state.ui {
                        *page_count = state.model.pages.len().max(1);
                    }
                }
            }
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let (x, y) = (
                event.x_transformed(state.output_size.0) as f32,
                event.y_transformed(state.output_size.1) as f32,
            );
            state.last_pointer_pos = Some((x, y));

            if state.pointer_down {
                // Page drag: update spring value to follow finger.
                if let Some(start_x) = state.page_drag_start {
                    let dx = x - start_x;
                    let w = state.output_size.0 as f32;
                    if let UiState::Home { page, page_spring, page_count, .. } = &mut state.ui {
                        // Directly set spring value to track finger (no spring physics during drag).
                        let raw_target = *page as f32 - dx / w;
                        // Rubber-band past edges.
                        let max_page = (*page_count).saturating_sub(1) as f32;
                        page_spring.value = if raw_target < 0.0 {
                            raw_target * 0.3 // rubber-band left
                        } else if raw_target > max_page {
                            max_page + (raw_target - max_page) * 0.3 // rubber-band right
                        } else {
                            raw_target
                        };
                        page_spring.target = page_spring.value;
                        page_spring.velocity = 0.0;
                    }
                }

                // Feed movement to grab if active.
                let dt = 1.0 / 90.0;
                if let Some(ev) = input_dispatch::on_move(&state.ui, x, y, dt, state.output_size) {
                    transition(&mut state.ui, ev);
                }
            }
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

    // Tick animations.
    let dt = 1.0 / 90.0;
    transition(&mut state.ui, UiEvent::Tick { dt });

    // Animations that settle to home reset page_count to 1; restore from the model.
    if let UiState::Home { page_count, .. } = &mut state.ui {
        *page_count = state.model.pages.len().max(1);
    }

    // Compute scene from current state.
    let scene = compute_scene(&state.ui, state.output_size);

    // Resolve the app surface for compositing.
    let app_surface: Option<WlSurface> = scene.window.as_ref().and_then(|(tid, _)| {
        state
            .toplevels
            .get(*tid)
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone())
    });

    let transform = scene.window.as_ref().map(|(_, t)| *t);
    let is_fullscreen = transform.is_none_or(|t| t.scale >= 0.99);

    let (renderer, mut framebuffer) = backend.bind()?;

    // Collect render elements.
    let base_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        if let Some(ref wl_surface) = app_surface {
            render_elements_from_surface_tree(
                renderer, wl_surface, (0, 0), 1.0, 1.0, Kind::Unspecified,
            )
        } else {
            Vec::new()
        };

    // Pass 1: Clear background.
    {
        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Flipped180)
            .map_err(SwapBuffersError::from)?;
        frame.clear(CLEAR_COLOR, &[damage]).map_err(SwapBuffersError::from)?;

        // If fullscreen, draw app in this pass (no home behind).
        if is_fullscreen && !base_elements.is_empty() {
            if let Err(e) = draw_render_elements(&mut frame, 1.0, &base_elements, &[damage]) {
                warn!(?e, "failed to draw app elements");
            }
        }

        let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    }

    // Skia: draw home screen behind (if transitioning).
    if scene.show_home {
        state.skia.draw_home(
            size.w, size.h, scene.home_page, scene.page_offset,
            &state.model, &state.icon_cache, &state.app_catalog,
        );
    }

    // Pass 2: Draw scaled app ON TOP of home (no clear).
    if !is_fullscreen && !base_elements.is_empty() {
        if let Some(t) = transform {
            let scale_f = t.scale as f64;
            let card_w = size.w as f32 * t.scale;
            let card_h = size.h as f32 * t.scale;
            let card_x = (t.center_x - card_w / 2.0) as i32;
            let card_y = (t.center_y - card_h / 2.0) as i32;

            let scaled: Vec<RescaleRenderElement<RelocateRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>> =
                base_elements
                    .into_iter()
                    .map(|e| {
                        let relocated = RelocateRenderElement::from_element(
                            e,
                            Point::<i32, Physical>::from((card_x, card_y)),
                            Relocate::Relative,
                        );
                        RescaleRenderElement::from_element(
                            relocated,
                            Point::<i32, Physical>::from((card_x, card_y)),
                            smithay::utils::Scale::from(scale_f),
                        )
                    })
                    .collect();

            // Second render pass without clearing.
            let mut frame = renderer
                .render(&mut framebuffer, size, Transform::Flipped180)
                .map_err(SwapBuffersError::from)?;
            if let Err(e) = draw_render_elements(&mut frame, 1.0, &scaled, &[damage]) {
                warn!(?e, "failed to draw scaled app elements");
            }
            let _sync = frame.finish().map_err(SwapBuffersError::from)?;
        }
    }

    // Always draw the bar on top.
    state.skia.draw_bar_overlay(size.w, size.h);

    // Send frame callbacks.
    if let Some(ref wl_surface) = app_surface {
        send_frames_surface_tree(wl_surface, state.start_time.elapsed().as_millis() as u32);
    }

    drop(framebuffer);
    backend.submit(Some(&[damage]))
}

/// Send frame callbacks to all surfaces in the tree.
fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}
