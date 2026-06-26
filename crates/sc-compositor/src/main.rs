//! springchick compositor — M2: calloop-based Wayland compositor with home screen.
//!
//! Adopts Smithay's idiomatic calloop architecture. A single `State` holds Wayland
//! globals, running toplevels, the ShellModel, renderer + SkiaGl, and UI state.

mod backend;
mod launcher;
mod skia_gl;
pub mod ui_state;

use backend::{FP5_HEIGHT, FP5_WIDTH};
use launcher::spawn_app;
use skia_gl::SkiaGl;
use ui_state::{transition, ToplevelId, UiEvent, UiState};

use sc_config::{catalog, state as config_state, AppEntry};
use sc_icons::IconPixels;
use sc_layout::{self, Hit};
use sc_shell_model::ShellModel;

use smithay::backend::input::{InputEvent, KeyboardKeyEvent, KeyState};
use smithay::backend::input::Keycode;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::element::surface::{render_elements_from_surface_tree, WaylandSurfaceRenderElement};
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
use smithay::utils::{Rectangle, Serial, Transform};
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
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};

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

/// Tracks a horizontal drag for page swiping.
#[derive(Clone, Copy, Debug)]
struct DragState {
    start_x: f32,
    current_x: f32,
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

    // Rendering
    skia: SkiaGl,
    wayland_socket: String,

    // Input
    last_pointer_pos: Option<(f32, f32)>,
    /// Drag state for page swiping: (start_x, is_dragging)
    drag_state: Option<DragState>,

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
        output.change_current_state(Some(mode), None, Some(Scale::Integer(1)), None);
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
            skia: SkiaGl::new(),
            wayland_socket,
            last_pointer_pos: None,
            drag_state: None,
            start_time: std::time::Instant::now(),
            running: true,
        }
    }

    fn handle_tap(&mut self, x: f32, y: f32) {
        let page = match &self.ui {
            UiState::Home { page, .. } => *page,
            UiState::App { .. } => return,
        };

        let layout =
            sc_layout::compute(FP5_WIDTH as f32, FP5_HEIGHT as f32, page, &self.model);

        match sc_layout::hit_test(&layout, x, y) {
            Hit::GridIcon { app_id, .. } | Hit::DockIcon { app_id, .. } => {
                self.launch_or_raise(&app_id);
            }
            Hit::Bar | Hit::Miss => {}
        }
    }

    fn handle_return_home(&mut self) {
        transition(&mut self.ui, UiEvent::ReturnHome);
        if let UiState::Home { page_count, .. } = &mut self.ui {
            *page_count = self.model.pages.len().max(1);
        }
    }

    fn launch_or_raise(&mut self, app_id: &str) {
        // Check if already running — raise it.
        for (idx, slot) in self.toplevels.iter().enumerate() {
            if let Some(tl) = slot {
                if tl.app_id == app_id {
                    transition(
                        &mut self.ui,
                        UiEvent::AppMapped {
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

        transition(
            &mut self.ui,
            UiEvent::AppMapped {
                toplevel: id,
                app_id,
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
        surface.with_pending_state(|state| {
            state.size = Some((FP5_WIDTH, FP5_HEIGHT).into());
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

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_shm!(State);
delegate_data_device!(State);
delegate_output!(State);

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
    info!("springchick M2 — calloop compositor");
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

            // Esc (evdev key 1) intercept for return-home.
            // ESC in evdev = 1, XKB offset +8 = 9
            let esc_keycode: Keycode = 9u32.into();
            if key_code == esc_keycode && key_state == KeyState::Pressed {
                if matches!(state.ui, UiState::App { .. }) {
                    state.handle_return_home();
                    return;
                }
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
                    // Start tracking a potential drag.
                    state.drag_state = Some(DragState {
                        start_x: x,
                        current_x: x,
                    });
                } else {
                    // Released — determine if this was a tap or a swipe.
                    let was_swipe = if let Some(drag) = state.drag_state.take() {
                        let dx = drag.current_x - drag.start_x;
                        let threshold = FP5_WIDTH as f32 * 0.15;
                        if dx.abs() > threshold {
                            // Page swipe.
                            if let UiState::Home { page, page_count, .. } = &mut state.ui {
                                if dx < 0.0 && *page + 1 < *page_count {
                                    *page += 1;
                                } else if dx > 0.0 && *page > 0 {
                                    *page -= 1;
                                }
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !was_swipe {
                        // Treat as tap.
                        match &state.ui {
                            UiState::Home { .. } => {
                                state.handle_tap(x, y);
                            }
                            UiState::App { .. } => {
                                let layout = sc_layout::compute(
                                    FP5_WIDTH as f32,
                                    FP5_HEIGHT as f32,
                                    0,
                                    &state.model,
                                );
                                if layout.bar_rect.contains(x, y) {
                                    state.handle_return_home();
                                }
                            }
                        }
                    }
                }
            }
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let (x, y) = (
                event.x_transformed(FP5_WIDTH) as f32,
                event.y_transformed(FP5_HEIGHT) as f32,
            );
            state.last_pointer_pos = Some((x, y));
            // Update drag tracking.
            if let Some(ref mut drag) = state.drag_state {
                drag.current_x = x;
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

    // In App state, collect render elements from the focused toplevel.
    let app_surface: Option<WlSurface> = match &state.ui {
        UiState::App { toplevel, .. } => state
            .toplevels
            .get(*toplevel)
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone()),
        _ => None,
    };

    let (renderer, mut framebuffer) = backend.bind()?;

    // Collect render elements before starting frame (avoids double-borrow of renderer).
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = if let Some(ref wl_surface) = app_surface {
        render_elements_from_surface_tree(
            renderer,
            wl_surface,
            (0, 0),
            1.0,
            1.0,
            Kind::Unspecified,
        )
    } else {
        Vec::new()
    };

    {
        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Flipped180)
            .map_err(SwapBuffersError::from)?;
        frame
            .clear(CLEAR_COLOR, &[damage])
            .map_err(SwapBuffersError::from)?;

        // If in App state, composite the client's surface fullscreen.
        if !elements.is_empty() {
            if let Err(e) = draw_render_elements(&mut frame, 1.0, &elements, &[damage]) {
                warn!(?e, "failed to draw app elements");
            }
        }

        let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    }

    // Skia draws home screen or bar overlay on top.
    match &state.ui {
        UiState::Home { page, .. } => {
            let page = *page;
            state.skia.draw_home(
                size.w,
                size.h,
                page,
                &state.model,
                &state.icon_cache,
                &state.app_catalog,
            );
        }
        UiState::App { .. } => {
            // Bar overlay so return-home affordance is always visible.
            state.skia.draw_bar_overlay(size.w, size.h);
        }
    }

    // Send frame callbacks to the focused client so it keeps rendering.
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
