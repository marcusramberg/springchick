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
mod popups;
mod render;
pub mod scene;
mod skia_gl;
mod switcher;
mod touch;
mod touch_viz;
pub mod ui_state;

use app_history::AppHistory;
use launcher::spawn_app;
use scene::compute_scene;
use skia_gl::SkiaGl;
use ui_state::{transition, ToplevelId, UiEvent, UiState, ZoomOrigin};

use sc_catalog::AppEntry;
use sc_icons::IconPixels;
use sc_shell_model::{persist, ShellModel};

use smithay::backend::input::{Event, InputEvent, KeyboardKeyEvent};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram};
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
use smithay::delegate_input_method_manager;
use smithay::delegate_text_input_manager;
use smithay::wayland::input_method::{
    InputMethodHandler, InputMethodManagerState, PopupSurface as ImePopupSurface,
};
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::image_capture_source::{
    ImageCaptureSource, ImageCaptureSourceHandler, ImageCaptureSourceState,
    OutputCaptureSourceHandler, OutputCaptureSourceState,
};
use smithay::wayland::image_copy_capture::{
    BufferConstraints, Frame as CaptureFrame, ImageCopyCaptureHandler, ImageCopyCaptureState,
    Session as CaptureSession, SessionRef as CaptureSessionRef,
};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::drm::DrmNode;
use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::desktop::{PopupKind, PopupManager};
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
use smithay::wayland::dmabuf::{
    DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
};
use smithay::wayland::fractional_scale::{
    with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState,
};
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::output::{OutputHandler, OutputManagerState};
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

/// Persisted-state file path (`state.toml`; shared read-only with the search
/// app). Distinct from `config.toml`, which `sc-config` parses.
fn state_path() -> PathBuf {
    persist::state_path()
}

/// Current unix time in whole seconds (monotonic-enough for frecency).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A running app's toplevel state.
struct AppToplevel {
    surface: ToplevelSurface,
    app_id: String,
}

/// An app spawned from the launcher but not yet mapped to a toplevel. Its Home
/// icon pulses until the window opens, the process dies, or we time out waiting.
struct Launching {
    app_id: String,
    child: Child,
    started: std::time::Instant,
}

/// How long to keep pulsing a launching icon before giving up (a daemonizing or
/// hung launcher may never map a window; stop breathing forever).
const LAUNCH_PULSE_TIMEOUT: Duration = Duration::from_secs(10);

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
    pub app_popups: layer_shell::RenderList,
    pub layer_popups: layer_shell::RenderList,
    /// Touch indicator marks to overlay (empty unless `show_touches`).
    pub touch_marks: Vec<touch_viz::TouchMark>,
}

/// A popup and its clamped physical geometry: `(kind, origin, size)`. Chains are
/// ordered root→leaf.
type PopupRect = (PopupKind, (i32, i32), (i32, i32));

/// Hold duration (milliseconds) an icon press must survive before arrange
/// mode engages.
const HOLD_MS: u128 = 500;

/// xdg `app_id` the pull-down search app sets on its toplevel. The compositor
/// recognises it to give it a slide-up open animation and to keep it out of the
/// task switcher / MRU history.
const SEARCH_APP_ID: &str = "chick.springchick.Search";
/// Exec line for the search app, spawned on the pull-down gesture.
const SEARCH_APP_EXEC: &str = "sc-search";

/// A finger held on an icon on Home, waiting to see if it becomes a
/// long-press (arrange mode). Cancelled if the finger moves past the tap
/// slop (becomes a swipe) or releases before `HOLD_MS`.
struct IconPress {
    app_id: String,
    source: input_dispatch::IconSource,
    start: (f32, f32),
    at: std::time::Instant,
}

/// An icon currently being dragged in arrange mode.
struct DragItem {
    app_id: String,
    source: input_dispatch::IconSource,
    cur: (f32, f32),
    /// (page, index) hole the grid opens under the finger; None until first
    /// motion or when the finger is over the dock zone.
    hover: Option<(usize, usize)>,
    /// When the finger entered the current edge zone, for dwell-to-flip.
    /// None when not in an edge zone.
    edge_since: Option<(std::time::Instant, EdgeSide)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum EdgeSide {
    Left,
    Right,
}

/// Arrange-mode state: icons wiggle, badges/Done button are live, and an
/// icon may be mid-drag toward the dock (pin) or grid (unpin).
#[derive(Default)]
struct ArrangeState {
    drag: Option<DragItem>,
}


/// Main compositor state.
struct State {
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    /// Tracks xdg_popup trees (their lifecycle + geometry) so we can render,
    /// hit-test, and dismiss menus/dropdowns. Populated in `new_popup`.
    popups: PopupManager,
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
    /// Idle-blank countdown. Reset by input in the DRM loop; when it elapses the
    /// loop flips `blank`. Inert under winit (which never polls it).
    idle: blank::Idle,
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
    /// `zxdg_output_manager_v1`: output name + logical geometry for clients that
    /// ask (recorders). Held to keep the global advertised.
    #[allow(dead_code)]
    output_manager_state: OutputManagerState,
    /// `ext-image-capture-source-v1` + `ext-image-copy-capture-v1` (screencopy):
    /// let recorders capture the output. Held to keep the globals advertised.
    #[allow(dead_code)]
    image_capture_source: ImageCaptureSourceState,
    #[allow(dead_code)]
    output_capture_source: OutputCaptureSourceState,
    image_copy_capture: ImageCopyCaptureState,
    /// Capture buffer formats to advertise, set by the DRM backend once the
    /// renderer exists: `(render node, [(fourcc, modifiers)])`. `None` on the
    /// winit backend (no dmabuf capture there).
    capture_formats: Option<(DrmNode, Vec<(Fourcc, Vec<Modifier>)>)>,
    /// Capture frames awaiting a blit; drained by the DRM render loop each frame.
    pending_captures: Vec<CaptureFrame>,
    /// Tracked layer surfaces + reserved-area bookkeeping.
    layers: layer_shell::LayerShell,
    /// Seat touch handle, for forwarding taps to layer surfaces.
    touch: smithay::input::touch::TouchHandle<Self>,
    /// Per-slot touch routing. The phone panel delivers concurrent slots
    /// (fingers); each slot that lands on a client surface (OSK layer, app,
    /// popup) gets its own target here so one finger's up never clears another's
    /// grab, and a stray slot never leaks into the gesture funnel. Slots that
    /// start on empty space are absent (they drive the gesture funnel instead).
    /// Value is the slot's coord scale (`dpi`); presence marks it client-routed.
    touch_targets: std::collections::HashMap<smithay::backend::input::TouchSlot, f64>,
    /// The single slot currently driving the home-screen gesture funnel
    /// (`input_common`), which is inherently single-touch. Only this slot feeds
    /// press/motion/release; additional fingers on empty space are ignored until
    /// it lifts.
    gesture_slot: Option<smithay::backend::input::TouchSlot>,
    /// Whether the pointer press is currently held on a client surface.
    pointer_grab: bool,
    /// wl_surfaces of popups that issued an `xdg_popup.grab()`. Only these are
    /// modal — they capture touch and dismiss on an outside press. Non-grab
    /// popups (wvkbd's input-enabling hack popup, app tooltips/comboboxes) are
    /// tracked and rendered but must NOT steal or dismiss touch, else they break
    /// OSK and toplevel input. Cleared per-popup in `popup_destroyed`.
    popup_grabs: std::collections::HashSet<WlSurface>,
    /// Home-bar opacity, faded to 0 when a bottom exclusive-zone surface (the
    /// on-screen keyboard) covers it.
    bar_alpha: f32,
    /// Whether to draw the touch indicator overlay (`[main].show_touches`), for
    /// demo recordings. When true, input events feed `touch_viz`.
    show_touches: bool,
    /// Live touch-visualization state (contacts + fading release rings). Only
    /// populated while `show_touches` is set.
    touch_viz: touch_viz::TouchViz,

    // Shell state
    ui: UiState,
    model: ShellModel,
    app_catalog: HashMap<String, AppEntry>,
    icon_cache: HashMap<String, IconPixels>,
    toplevels: Vec<Option<AppToplevel>>,
    children: Vec<Child>,
    /// App spawned and awaiting its first toplevel — drives the pulsing launch
    /// icon. Cleared when the window maps, the process exits, or on timeout.
    launching: Option<Launching>,
    history: AppHistory,
    /// Last zoom origin (cached when launching).
    last_origin: ZoomOrigin,
    /// Actual output size in physical pixels (from the backend: DRM mode or
    /// winit window size). Set at construction; drives layout and app sizing.
    output_size: (i32, i32),
    /// The advertised output. Retained so surfaces can `enter` it (which is how
    /// clients learn the scale factor).
    output: Output,
    /// Output scale (`[main].dpi`), advertised via `wp_fractional_scale` so it
    /// may be fractional (e.g. 2.5). Client buffers are `logical * dpi`, so xdg
    /// configure sizes are physical/dpi.
    dpi: f64,
    /// Base card corner radius in logical px (`[main].card_radius`). Threaded
    /// into `compute_scene` so the switcher/drag card rounding is configurable.
    card_radius: f32,
    /// Prefer server-side (= no client) decorations for top-level app windows
    /// (`[main].prefer_no_csd`). Dialogs (child toplevels) always get CSD so
    /// their toolkit header bar — and its action buttons — stay present.
    prefer_no_csd: bool,
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
    /// Finger held on an icon, waiting to see if it becomes a long-press
    /// (arrange mode) or a tap/swipe. Cleared once arrange mode engages, the
    /// gesture becomes a swipe, or the finger releases.
    icon_press: Option<IconPress>,
    /// Arrange-mode state (icon reorder/pin/unpin/hide). `None` outside
    /// arrange mode.
    arrange: Option<ArrangeState>,
    /// Armed pull-down: `(start_x, start_y)` of an empty-space Home press that
    /// may become a downward drag launching the search app. Cleared once resolved.
    search_arm: Option<(f32, f32)>,
    /// Set when the search app was just spawned; the next toplevel to map is
    /// treated as the search app (slide-up open, no switcher/frecency). winit
    /// sets the xdg `app_id` after `new_toplevel` fires, so the client id is not
    /// yet readable at registration — the spawn intent is the reliable signal.
    expecting_search: bool,
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
    /// Per-app grid-reflow springs (x, y), keyed by app id. Drives icons
    /// sliding to their new slot when the grid order changes (launch reorder,
    /// arrange-mode edits). Seeded lazily on first `advance_frame` and kept in
    /// sync with `model.pages` by `reflow_grid`.
    grid_anim: std::collections::HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,
    /// Per-app dock-reflow springs (x, y), keyed by app id. Mirror of
    /// `grid_anim` for the dock row; seeded lazily and kept in sync by
    /// `reflow_dock`.
    dock_anim: std::collections::HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,

    // Timing
    start_time: std::time::Instant,

    // Perf instrumentation
    stats: frame_stats::FrameStats,
    perf_log: bool,
    last_perf_log: std::time::Instant,

    // Control
    running: bool,
}

/// Global-space target (x, y) for every app currently on the grid (dock and
/// hidden apps excluded — they aren't in `model.pages`), keyed by app id.
/// Pure so it's cheap to unit-test independent of `State`.
fn reflow_targets(model: &ShellModel, width: f32, height: f32) -> std::collections::HashMap<String, (f32, f32)> {
    reflow_targets_for(&model.pages, width, height)
}

/// Sentinel occupying the gap slot in the drag working order. Never a real app
/// id (NUL-prefixed), so it can't collide; it is laid out for spacing but never
/// drawn (it is not in `model.pages`, and `reflow_grid` drops it from targets).
pub(crate) const HOLE: &str = "\u{0}hole";

/// The drag "working order": the flattened grid with `dragged` removed and, when
/// `hover` is Some, a HOLE sentinel inserted at the hovered global index so the
/// real icons part to show the drop target. Re-chunked into pages.
fn working_order(pages: &[Vec<String>], dragged: &str, hover: Option<(usize, usize)>) -> Vec<Vec<String>> {
    let mut flat: Vec<String> = pages.iter().flatten().filter(|a| *a != dragged).cloned().collect();
    if let Some((page, index)) = hover {
        let gi = (page.saturating_mul(sc_shell_model::PAGE_CAP).saturating_add(index)).min(flat.len());
        flat.insert(gi, HOLE.to_string());
    }
    flat.chunks(sc_shell_model::PAGE_CAP).map(|c| c.to_vec()).collect()
}

/// Reflow targets over an explicit page list (used for the live drag "working
/// order": dragged app removed so remaining icons compact and open a gap).
fn reflow_targets_for(pages: &[Vec<String>], width: f32, height: f32)
    -> std::collections::HashMap<String, (f32, f32)>
{
    let mut out = std::collections::HashMap::new();
    for (page, apps) in pages.iter().enumerate() {
        for (index, app) in apps.iter().enumerate() {
            out.insert(app.clone(), sc_layout::global_slot_pos(page, index, width, height));
        }
    }
    out
}

impl State {
    /// Record one frame's duration and emit a perf summary at most once per
    /// second. Shared by the winit and DRM render loops.
    fn record_and_log_frame(&mut self, frame_start: std::time::Instant) {
        self.stats.record_frame(frame_start.elapsed());
        if self.perf_log && self.last_perf_log.elapsed() >= Duration::from_secs(1) {
            debug!(target: "springchick::perf", "{}", self.stats.format_line());
            self.last_perf_log = std::time::Instant::now();
        }
    }

    fn new(display: &Display<Self>, wayland_socket: String, output_size: (i32, i32)) -> Self {
        let dh = display.handle();
        let (out_w, out_h) = output_size;

        // v6 so clients like wvkbd that bind wl_compositor@6 can connect.
        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        // Advertise only Fullscreen as a WM capability. Maximize/Minimize/
        // WindowMenu are meaningless in a single-window phone shell, and hinting
        // them absent tells toolkits (GTK) to omit those title-bar buttons on the
        // few windows that do draw client-side decorations (dialogs).
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [xdg_toplevel::WmCapabilities::Fullscreen],
        );
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
        // Screencopy: the source-manager globals let a client name our output as a
        // capture source; the copy-capture global negotiates buffers + frames.
        let image_capture_source = ImageCaptureSourceState::new();
        let output_capture_source = OutputCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture = ImageCopyCaptureState::new::<Self>(&dh);
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
        let dpi = keybinds::load_dpi().max(1.0);
        output.change_current_state(
            Some(mode),
            None,
            Some(smithay::output::Scale::Fractional(dpi)),
            None,
        );
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);
        // xdg-output manager (`zxdg_output_manager_v1`): reports each output's
        // name + logical geometry. Recorders like wl-screenrec require it to
        // resolve which output to capture. Dispatched via `delegate_output!`.
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        // wlr-gamma-control: advertise the manager global. 256 is a mock LUT
        // size for the winit backend; the DRM backend overrides it with the
        // real CRTC gamma_length before clients connect.
        let gamma = gamma_control::GammaControl::new(&dh, 256);

        // Load shell model + app catalog.
        let model = persist::load(&state_path()).unwrap_or_default();
        let apps = sc_catalog::scan_apps();
        let app_catalog: HashMap<String, AppEntry> =
            apps.into_iter().map(|e| (e.id.clone(), e)).collect();

        // Seed new catalog apps, drop stats for uninstalled ones, derive order.
        let mut model = model;
        let now = unix_now();
        let mut catalog_ids: Vec<String> = app_catalog.keys().cloned().collect();
        catalog_ids.sort(); // deterministic seeding + first-run alpha order
        let first_run = model.frecency.apps.is_empty();
        model.reconcile(&catalog_ids, now, first_run);

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
            popups: PopupManager::default(),
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
            idle: blank::Idle::new(keybinds::load_idle_blank_secs(), std::time::Instant::now()),
            needs_render: false,
            last_present: None,
            osd: osd::Osd::new(),
            layer_shell_state,
            fractional_scale_manager_state,
            viewporter_state,
            output_manager_state,
            image_capture_source,
            output_capture_source,
            image_copy_capture,
            capture_formats: None,
            pending_captures: Vec::new(),
            layers: layer_shell::LayerShell::new(output.clone(), out_w as f32, out_h as f32),
            touch,
            touch_targets: std::collections::HashMap::new(),
            gesture_slot: None,
            pointer_grab: false,
            popup_grabs: std::collections::HashSet::new(),
            bar_alpha: 1.0,
            show_touches: keybinds::load_show_touches(),
            touch_viz: touch_viz::TouchViz::new(),
            ui,
            model,
            app_catalog,
            icon_cache,
            toplevels: Vec::new(),
            children: Vec::new(),
            launching: None,
            history: AppHistory::new(),
            last_origin: ZoomOrigin::icon((out_w as f32 / 2.0, out_h as f32 / 2.0)),
            output_size,
            output,
            dpi,
            card_radius: keybinds::load_card_radius(),
            prefer_no_csd: keybinds::load_prefer_no_csd(),
            gamma,
            skia: SkiaGl::new(),
            wayland_socket,
            last_pointer_pos: None,
            pointer_down: false,
            page_drag_start: None,
            bar_drag_start: None,
            pending_launch: None,
            icon_press: None,
            arrange: None,
            search_arm: None,
            expecting_search: false,
            switcher_drag: input_common::SwitcherDrag::None,
            switcher_cards: Vec::new(),
            active_gesture: None,
            active_key: None,
            active_touch: None,
            pending_settle: None,
            last_log_state: None,
            grid_anim: std::collections::HashMap::new(),
            dock_anim: std::collections::HashMap::new(),
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
    ///
    /// When `main_device` is known (the DRM backend), bind version 4 with default
    /// feedback: it advertises the render device + format tranches that zero-copy
    /// clients and, crucially, `wl-screenrec` need to allocate importable capture
    /// buffers. A version-3 global (no feedback) is rejected by wl-screenrec.
    fn init_dmabuf_global(
        &mut self,
        dh: &DisplayHandle,
        formats: impl IntoIterator<Item = DrmFormat>,
        main_device: Option<libc::dev_t>,
    ) {
        let formats: Vec<DrmFormat> = formats.into_iter().collect();
        let feedback = main_device
            .and_then(|dev| DmabufFeedbackBuilder::new(dev, formats.iter().copied()).build().ok());
        let global = match feedback {
            Some(feedback) => self
                .dmabuf_state
                .create_global_with_default_feedback::<Self>(dh, &feedback),
            None => self.dmabuf_state.create_global::<Self>(dh, formats),
        };
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

    /// Re-derive grid order from the current model state and persist it.
    /// Shared by launch (frecency-driven reorder) and arrange-mode edits
    /// (pin/unpin/hide).
    /// Persist + reflow after a manual grid/dock edit (pin/unpin/hide/reorder).
    /// No frecency recompute — grid order is now manual.
    fn after_arrange_edit(&mut self) {
        self.model.repack();
        if let Err(e) = persist::save(&self.model, &state_path()) {
            warn!(%e, "failed to save shell model after arrange edit");
        }
        self.reflow_grid();
        self.reflow_dock();
    }

    /// Re-target the grid-reflow springs to each app's current slot position,
    /// seeding new entries and dropping ones no longer on the grid (docked,
    /// hidden). Called after any change to `model.pages`.
    /// The current pages with `dragged` removed (its slot becomes a gap the
    /// remaining icons compact into). The dragged app renders as a ghost, so it
    /// is intentionally absent from the reflow targets. `hover` is accepted for
    /// future explicit-hole placement but the compacted layout already yields a
    /// gap at/after the removed slot.
    fn working_pages(&self, dragged: &str, hover: Option<(usize, usize)>) -> Vec<Vec<String>> {
        working_order(&self.model.pages, dragged, hover)
    }

    fn reflow_grid(&mut self) {
        let (w, h) = self.output_size_f();
        let drag_app = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .map(|d| (d.app_id.clone(), d.hover));
        let targets = match drag_app {
            Some((app_id, hover)) => {
                let working = self.working_pages(&app_id, hover);
                let mut t = reflow_targets_for(&working, w, h);
                t.remove(HOLE);
                t
            }
            None => reflow_targets(&self.model, w, h),
        };
        for (app, (tx, ty)) in &targets {
            match self.grid_anim.get_mut(app) {
                Some((sx, sy)) => {
                    sx.retarget(*tx);
                    sy.retarget(*ty);
                }
                None => {
                    self.grid_anim
                        .insert(app.clone(), (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty)));
                }
            }
        }
        self.grid_anim.retain(|app, _| targets.contains_key(app));
    }

    /// Retarget dock springs to the current dock layout, dropping a dock icon
    /// that is being dragged (it rides as the ghost). Mirror of `reflow_grid`.
    fn reflow_dock(&mut self) {
        let (w, h) = self.output_size_f();
        let dragged = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .filter(|d| d.source == input_dispatch::IconSource::Dock)
            .map(|d| d.app_id.clone());
        // Lay out with the dragged dock app removed so the surviving icons
        // re-center over the N-1 cells (the dock is anchored to fixed per-index
        // cells, so omitting the app from `targets` alone would not move them).
        let layout = if let Some(app) = &dragged {
            let mut m = self.model.clone();
            m.dock.retain(|a| a != app);
            sc_layout::compute(w, h, 0, &m)
        } else {
            sc_layout::compute(w, h, 0, &self.model)
        };
        let mut targets: std::collections::HashMap<String, (f32, f32)> = std::collections::HashMap::new();
        for slot in &layout.dock {
            targets.insert(slot.app_id.clone(), (slot.icon_rect.center_x(), slot.icon_rect.center_y()));
        }
        for (app, (tx, ty)) in &targets {
            match self.dock_anim.get_mut(app) {
                Some((sx, sy)) => {
                    sx.retarget(*tx);
                    sy.retarget(*ty);
                }
                None => {
                    self.dock_anim
                        .insert(app.clone(), (sc_anim::Spring::new(*tx), sc_anim::Spring::new(*ty)));
                }
            }
        }
        self.dock_anim.retain(|app, _| targets.contains_key(app));
    }

    /// Launch the pull-down search app. It is a normal Wayland client (a
    /// fullscreen xdg toplevel) that owns the search UI, keyboard, and app
    /// launching; the compositor only spawns it and gives it the usual app
    /// treatment (focus/touch/animation). Deduped: if it is already running,
    /// raise it instead of spawning a second copy.
    fn open_search(&mut self) {
        self.search_arm = None;
        self.page_drag_start = None;
        self.pending_launch = None;
        for (idx, slot) in self.toplevels.iter().enumerate() {
            if slot.as_ref().is_some_and(|tl| tl.app_id == SEARCH_APP_ID) {
                self.raise_toplevel_centered(idx, false);
                return;
            }
        }
        if let Some(child) = spawn_app(SEARCH_APP_EXEC, &self.wayland_socket) {
            self.children.push(child);
            self.expecting_search = true;
        }
        self.needs_render = true;
    }

    fn launch_or_raise(&mut self, app_id: &str, origin: ZoomOrigin) {
        self.last_origin = origin;

        // Frecency is recorded when a window actually maps (`register_toplevel`),
        // so launches from the search app — which exec apps directly rather than
        // through here — count the same as icon taps.

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

        // Launch new. Track it as `launching` so its icon pulses until the
        // window maps or the process dies. A prior in-flight launch is abandoned
        // (its child moved to the reap list) so only one icon pulses at a time.
        if let Some(entry) = self.app_catalog.get(app_id) {
            let exec = entry.exec.clone();
            if let Some(child) = spawn_app(&exec, &self.wayland_socket) {
                if let Some(prev) = self.launching.take() {
                    self.children.push(prev.child);
                }
                self.launching = Some(Launching {
                    app_id: app_id.to_string(),
                    child,
                    started: std::time::Instant::now(),
                });
            }
        }
    }

    /// Poll the pulsing launch: reap the child if it exited (launch failed) and
    /// give up after [`LAUNCH_PULSE_TIMEOUT`] (daemonized/hung launcher). Called
    /// from both backend loops; the mapped-window case is handled in
    /// `register_toplevel`.
    fn poll_launching(&mut self) {
        // Reap exited children so abandoned/finished launches don't zombie. The
        // `children` list is drained here (every loop tick) rather than on push,
        // since a launch's child may map, be abandoned, or time out — all funnel
        // through here.
        keybinds::reap(&mut self.children);

        let Some(l) = &mut self.launching else {
            return;
        };
        let exited = matches!(l.child.try_wait(), Ok(Some(_)) | Err(_));
        let timed_out = l.started.elapsed() >= LAUNCH_PULSE_TIMEOUT;
        if exited || timed_out {
            if let Some(prev) = self.launching.take() {
                self.children.push(prev.child);
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

        // The pull-down search app is a normal toplevel but gets special
        // treatment: a slide-up open, no frecency, and hidden from the switcher.
        // Detect it by the spawn intent (the app_id isn't set yet at this point)
        // or, as a fallback, its id. The flag is consumed by the first map.
        let is_search = self.expecting_search || wl_app_id == SEARCH_APP_ID;
        self.expecting_search = false;
        let app_id = if is_search {
            SEARCH_APP_ID.to_string()
        } else if !wl_app_id.is_empty() && self.app_catalog.contains_key(&wl_app_id) {
            wl_app_id
        } else {
            format!("unknown_{}", self.toplevels.len())
        };

        // Record frecency as a real app maps (covers both Home icon taps and
        // launches from the search app, which exec directly). Non-catalog ids
        // (the search app, unmatched clients) are skipped.
        if self.app_catalog.contains_key(&app_id) {
            self.model.frecency.record_launch(&app_id, unix_now());
            if let Err(e) = persist::save(&self.model, &state_path()) {
                warn!(%e, "failed to save shell model after launch");
            }
        }

        // Enter the output so the client receives its scale factor (`[main].dpi`)
        // and renders a HiDPI buffer instead of 1:1.
        self.output.enter(surface.wl_surface());

        // Window opened — stop the launch pulse. Keep the child handle for
        // reaping. The mapping client's `wl_app_id` is usually still empty at
        // first map (so `app_id` here is `unknown_N`), meaning an app_id match
        // is unreliable. Since only one launch pulses at a time and the search
        // app is handled separately, stop the pulse on the first non-search map.
        if !is_search && self.launching.is_some() {
            if let Some(prev) = self.launching.take() {
                self.children.push(prev.child);
            }
        }

        let id = self.toplevels.len();
        self.toplevels.push(Some(AppToplevel {
            surface,
            app_id: app_id.clone(),
        }));

        // Keep the search app out of the MRU / task switcher.
        if !is_search {
            self.history.push_foreground(id);
        }
        let open_mode = if is_search {
            ui_state::OpenMode::SlideUp
        } else {
            ui_state::OpenMode::Zoom
        };
        transition(
            &mut self.ui,
            UiEvent::AppMapped {
                toplevel: id,
                app_id,
                origin: self.last_origin,
                open_mode,
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
        if self.layers.usable_changed(self.dpi).is_some() {
            self.reconfigure_toplevels();
        }
    }

    /// Send every app toplevel a configure at the current usable size, so apps
    /// render within the area not covered by exclusive-zone layer surfaces.
    fn reconfigure_toplevels(&mut self) {
        // Logical size: client scales its buffer up by `dpi`.
        let usable = self.layers.usable(self.dpi);
        let size = (
            (usable.w as f64 / self.dpi).round() as i32,
            (usable.h as f64 / self.dpi).round() as i32,
        );
        for slot in self.toplevels.iter().flatten() {
            slot.surface.with_pending_state(|state| {
                state.size = Some(size.into());
            });
            slot.surface.send_configure();
        }
    }

    /// Bar fade target: 0 when a Top/Overlay layer surface covers the pill,
    /// else 1. The OSK is lifted above the pill's strip (see `shift_docked`),
    /// so it no longer fades the bar; only a surface actually over the pill
    /// (e.g. a fullscreen overlay) does.
    fn bar_alpha_target(&self) -> f32 {
        let (w, h) = self.output_size_f();
        let pill = sc_layout::pill_rect(w, h);
        if self.layers.top_overlaps(pill, self.dpi) {
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

    /// True while anything on screen is still changing, so the DRM loop should
    /// keep priming page-flips. False on a static screen (idle home, foreground
    /// app that isn't drawing) so the vblank render loop can stop and let the
    /// CPU/GPU idle. A fresh commit, input, or animation start re-arms rendering
    /// via `needs_render` and the animation springs below.
    fn is_animating(&self, now: std::time::Instant) -> bool {
        self.needs_render
            || self.ui.needs_animation()
            || self.launching.is_some()
            || self.osd.is_active(now)
            || self.bar_fading()
            || self
                .grid_anim
                .values()
                .any(|(sx, sy)| !sx.is_settled() || !sy.is_settled())
            || self
                .dock_anim
                .values()
                .any(|(sx, sy)| !sx.is_settled() || !sy.is_settled())
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
        let app = self.app_focus_surface();
        // Only a *grabbing* popup takes keyboard focus — a real menu that
        // requested the grab and drives keyboard nav from it. Redirecting focus
        // to a NON-grab popup makes clients (Firefox) read the toplevel's
        // wl_keyboard.leave as "app deactivated" and instantly roll the popup
        // back up. So focus the topmost grabbing popup in the app's chain if any,
        // else keep focus on the app itself. Both fall out of recomputing `want`
        // every frame, so focus restores to the app when the grab chain closes.
        let want = app
            .as_ref()
            .and_then(|s| {
                PopupManager::popups_for_surface(s)
                    .filter(|(kind, _)| self.popup_grabs.contains(kind.wl_surface()))
                    .last()
                    .map(|(kind, _)| kind.wl_surface().clone())
            })
            .or(app);
        if want == self.focused_surface {
            return;
        }
        self.focused_surface = want.clone();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, want, SERIAL_COUNTER.next_serial());
    }

    /// The wl_surface of the currently focused foreground app, if any.
    fn app_focus_surface(&self) -> Option<WlSurface> {
        ui_state::desired_focus(&self.ui)
            .and_then(|tid| self.toplevels.get(tid))
            .and_then(|slot| slot.as_ref())
            .map(|tl| tl.surface.wl_surface().clone())
    }

    /// Popups rooted at `root`, ordered root→leaf, as `(kind, phys_origin,
    /// phys_size)`. The origin is clamped so each popup stays fully on-screen.
    /// `root_origin` is where the root surface's `(0, 0)` is drawn, physical.
    fn popup_chain(
        &self,
        root: &WlSurface,
        root_origin: (i32, i32),
    ) -> Vec<PopupRect> {
        let dpi = self.dpi;
        PopupManager::popups_for_surface(root)
            .map(|(kind, loc)| {
                let geo = kind.geometry();
                let size = (
                    (geo.size.w as f64 * dpi).round() as i32,
                    (geo.size.h as f64 * dpi).round() as i32,
                );
                let origin = (
                    root_origin.0 + (loc.x as f64 * dpi).round() as i32,
                    root_origin.1 + (loc.y as f64 * dpi).round() as i32,
                );
                let clamped = popups::clamp_origin(origin, size, self.output_size);
                (kind, clamped, size)
            })
            .collect()
    }

    /// Popups parented to the fullscreen app (menus, dropdowns), root→leaf.
    fn app_popups(&self) -> Vec<PopupRect> {
        let usable = self.layers.usable(self.dpi);
        let origin = (usable.x.round() as i32, usable.y.round() as i32);
        self.app_focus_surface()
            .map(|s| self.popup_chain(&s, origin))
            .unwrap_or_default()
    }

    /// Popups parented to a top/overlay layer surface (e.g. an OSK menu).
    fn layer_popups(&self) -> Vec<PopupRect> {
        let mut out = Vec::new();
        let (below, above) = self.layers.render_lists(self.dpi);
        for (surface, origin) in below.iter().chain(above.iter()) {
            out.extend(self.popup_chain(surface, *origin));
        }
        out
    }

    /// Every open popup (app- and layer-rooted), root→leaf. Used by touch
    /// routing to hit-test and (for grabbing popups only) dismiss. A tap is
    /// routed into whichever popup it lands on regardless of grab; only a
    /// *grabbing* popup swallows an outside tap and dismisses — see
    /// `touch::popup_press`, which consults `popup_grabs` per popup.
    pub(crate) fn active_popups(&self) -> Vec<PopupRect> {
        let mut v = self.app_popups();
        v.extend(self.layer_popups());
        v
    }

    /// Whether `surface` is a popup that issued an `xdg_popup.grab()` (modal).
    pub(crate) fn popup_has_grab(&self, surface: &WlSurface) -> bool {
        self.popup_grabs.contains(surface)
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

    /// Current Home page, or `0` when not on the home screen (app/switcher/etc).
    fn current_home_page(&self) -> usize {
        if let UiState::Home { page, .. } = &self.ui {
            *page
        } else {
            0
        }
    }

    /// Raise `tid` to the foreground with a screen-centered zoom origin. Backs
    /// the bar swipe-up and the bar horizontal quick-switch, which differ only
    /// in which toplevel they pick. `reorder` records it as the most-recent app
    /// (swipe-up, a deliberate jump); quick-switch passes `false` so browsing
    /// left/right does not shuffle the MRU order.
    pub(crate) fn raise_toplevel_centered(&mut self, tid: ToplevelId, reorder: bool) {
        let Some(Some(tl)) = self.toplevels.get(tid) else {
            return;
        };
        let app_id = tl.app_id.clone();
        let (w, h) = self.output_size_f();
        self.last_origin = ZoomOrigin::icon((w / 2.0, h / 2.0));
        if reorder {
            self.history.push_foreground(tid);
        }
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
    /// Long-press hold: an icon held past HOLD_MS without moving into a swipe or
    /// launch engages arrange mode, picking up the same icon as the initial drag
    /// item so the finger doesn't need to move first.
    fn maybe_engage_arrange_hold(&mut self) {
        if self.arrange.is_some() || !self.pointer_down {
            return;
        }
        if let Some(p) = &self.icon_press {
            if p.at.elapsed().as_millis() >= HOLD_MS {
                let drag = DragItem {
                    app_id: p.app_id.clone(),
                    source: p.source,
                    cur: p.start,
                    hover: None,
                    edge_since: None,
                };
                self.arrange = Some(ArrangeState { drag: Some(drag) });
                self.pending_launch = None;
                self.page_drag_start = None;
                self.icon_press = None;
            }
        }
    }

    /// Edge-dwell page flip: while dragging a reorder icon, holding it against a
    /// screen edge past EDGE_DWELL_MS flips the home page (auto-repeating), adding
    /// a trailing page if needed.
    fn tick_edge_page_flip(&mut self) {
        const EDGE_FRAC: f32 = 0.06;
        const EDGE_DWELL_MS: u128 = 400;
        let Some((cur_x, mut es)) = self
            .arrange
            .as_ref()
            .and_then(|a| a.drag.as_ref())
            .map(|d| (d.cur.0, d.edge_since))
        else {
            return;
        };
        let (w, _h) = self.output_size_f();
        let side = if cur_x < w * EDGE_FRAC {
            Some(EdgeSide::Left)
        } else if cur_x > w * (1.0 - EDGE_FRAC) {
            Some(EdgeSide::Right)
        } else {
            None
        };
        let now = std::time::Instant::now();
        let mut flip: Option<i32> = None;
        match side {
            None => es = None,
            Some(s) => match es {
                Some((since, prev)) if prev == s => {
                    if now.duration_since(since).as_millis() >= EDGE_DWELL_MS {
                        flip = Some(if s == EdgeSide::Left { -1 } else { 1 });
                        es = Some((now, s)); // reset -> auto-repeat
                    }
                }
                _ => es = Some((now, s)),
            },
        }
        // Write edge_since back.
        if let Some(d) = self.arrange.as_mut().and_then(|a| a.drag.as_mut()) {
            d.edge_since = es;
        }
        // Apply a flip.
        if let Some(dir) = flip {
            let cur_page = self.current_home_page();
            let new_page = if dir < 0 {
                cur_page.saturating_sub(1)
            } else if cur_page + 1 < self.model.pages.len() {
                cur_page + 1
            } else if self.model.pages.last().is_some_and(|p| p.is_empty()) {
                // Already a trailing empty page — go to it, don't add more.
                self.model.pages.len() - 1
            } else {
                self.model.pages.push(Vec::new());
                self.model.pages.len() - 1
            };
            let page_count = self.model.pages.len().max(1);
            if let UiState::Home {
                page,
                page_spring,
                page_count: pc,
                ..
            } = &mut self.ui
            {
                *page = new_page;
                *pc = page_count;
                page_spring.retarget(new_page as f32);
            }
        }
    }

    fn advance_frame(&mut self, dt: f32) -> FramePrep {
        self.maybe_engage_arrange_hold();

        // Lazy-seed the grid-reflow springs on first use so they snap to the
        // current order instead of animating in from (0,0).
        if self.grid_anim.is_empty() {
            self.reflow_grid();
        }
        if self.dock_anim.is_empty() {
            self.reflow_dock();
        }
        // Live reorder: retarget springs to the working order each frame while
        // an icon is being dragged. Gated on the drag itself (not `hover`) so the
        // dragged app is dropped from `grid_anim` immediately on pickup and while
        // over the dock — otherwise it double-draws (in-slot + ghost).
        if self
            .arrange
            .as_ref()
            .is_some_and(|a| a.drag.is_some())
        {
            self.reflow_grid();
            self.reflow_dock();
        }

        self.tick_edge_page_flip();

        for (sx, sy) in self.grid_anim.values_mut() {
            sx.step(dt);
            sy.step(dt);
        }
        for (sx, sy) in self.dock_anim.values_mut() {
            sx.step(dt);
            sy.step(dt);
        }

        let effect = transition(&mut self.ui, UiEvent::Tick { dt });
        match effect {
            ui_state::Effect::CloseToplevel { toplevel } => {
                self.close_toplevel(toplevel);
            }
            ui_state::Effect::EnterSwitcher => {
                let cards = self.history.deck_order();
                info!(target: "springchick::debug", "Effect::EnterSwitcher deck={:?}", cards);
                transition(&mut self.ui, UiEvent::EnterSwitcher { cards });
            }
            _ => {}
        }

        // Animations that settle to home reset page_count to 1; restore from the model.
        if let UiState::Home { page_count, .. } = &mut self.ui {
            *page_count = self.model.pages.len().max(1);
        }

        let usable = self.layers.usable(self.dpi);
        let scene = compute_scene(
            &self.ui,
            self.output_size,
            (usable.x, usable.y),
            self.card_radius,
        );
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
        let (layers_below, layers_above) = self.layers.render_lists(self.dpi);
        // `origin` is the popup's on-screen geometry top-left (used for clamp and
        // hit-test). The buffer is drawn from its (0,0), which sits `geometry.loc`
        // above-left of the geometry rect (client-side shadow/margin), so shift
        // the render origin back by it — matching smithay's own popup placement.
        let dpi = self.dpi;
        let to_render_list = |chain: Vec<PopupRect>| {
            chain
                .into_iter()
                .map(|(kind, origin, _)| {
                    let gloc = kind.geometry().loc;
                    let render_origin = (
                        origin.0 - (gloc.x as f64 * dpi).round() as i32,
                        origin.1 - (gloc.y as f64 * dpi).round() as i32,
                    );
                    (kind.wl_surface().clone(), render_origin)
                })
                .collect::<layer_shell::RenderList>()
        };
        let app_popups = to_render_list(self.app_popups());
        let layer_popups = to_render_list(self.layer_popups());

        // Touch indicator overlay: prune expired rings, then snapshot the marks
        // for this frame. Empty (and cheap) unless `show_touches` is on.
        let touch_marks = if self.show_touches {
            self.touch_viz.prune(osd_now);
            // Keep the vblank-driven DRM loop awake while rings are still fading.
            if self.touch_viz.is_active(osd_now) {
                self.needs_render = true;
            }
            self.touch_viz.marks(osd_now)
        } else {
            Vec::new()
        };

        FramePrep {
            scene,
            app_surface,
            frame_time,
            osd_view,
            bar_alpha,
            layers_below,
            layers_above,
            app_popups,
            layer_popups,
            touch_marks,
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

        // Advance popup configure/geometry state (initial configure, acks,
        // reposition) for any tracked popup in this surface's tree.
        self.popups.commit(surface);

        // A layer surface committing may change its geometry or reserved area.
        // `handle_commit` arranges the map (map/unmap + configures); we then
        // resize apps if the usable area changed.
        if self.layers.handle_commit(surface) {
            self.recompute_layers();
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.configure_maximized(&surface);
        self.register_toplevel(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.unregister_toplevel(surface.wl_surface());
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        // A client (video player, game) asked to go truly immersive. Cover the
        // whole output — not just the usable area — force server-side (= no)
        // decorations, and set the Fullscreen state so the toolkit hides its own
        // chrome. This is the only path that hides the OSK's exclusive zone.
        let (ow, oh) = self.output_size;
        let w = (ow as f64 / self.dpi).round() as i32;
        let h = (oh as f64 / self.dpi).round() as i32;
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.decoration_mode = Some(DecorationMode::ServerSide);
            state.states.unset(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // Back to the normal maximized app state.
        self.configure_maximized(&surface);
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
        // Track the popup so it gets rendered, hit-tested, and dismissed. Its
        // configure/commit lifecycle and geometry are then advanced in `commit`.
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            warn!(?e, "failed to track popup");
        }
        self.needs_render = true;
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        self.popup_grabs.insert(surface.wl_surface().clone());
        // Marked modal above: only grabbing popups capture touch and dismiss on
        // an outside press (see `active_popups`). Non-grab popups (wvkbd's hack
        // popup, tooltips) never enter that set, so they can't swallow taps.
        //
        // A client opens a popup grab from within an in-flight press (menus —
        // e.g. Firefox — grab on the button's press/release that opened them).
        // Do NOT cancel or retarget that sequence: per wl_touch/xdg-shell grab
        // semantics the originating press belongs to the surface that received
        // its `down`, and its matching `up` must be delivered there as normal.
        // The client owns the grab and won't treat that release as an
        // outside-dismiss. Cancelling it (as we used to) made Firefox read the
        // gesture as aborted and flicker the menu closed the instant it opened.
        // We dismiss manually on the *next* outside press (see `popup_press`),
        // never off the opening sequence's release.
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
        });
        surface.send_repositioned(token);
        if let Err(e) = surface.send_configure() {
            warn!(?e, "failed to configure repositioned popup");
        }
        self.needs_render = true;
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        // smithay has already removed it from `known_popups`; drop our render of
        // it on the next frame and forget any grab it held.
        self.popup_grabs.remove(surface.wl_surface());
        self.needs_render = true;
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
        let scale = self.dpi;
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| {
                fractional.set_preferred_scale(scale);
            });
        });
    }
}

impl State {
    /// Decoration mode for a toplevel under the current policy.
    ///
    /// Dialogs (child toplevels) always keep client-side decorations: their
    /// toolkit draws the action buttons (a GTK file chooser's Open/Cancel) into
    /// its own header bar, which vanishes under server-side decorations. Top-level
    /// app windows follow `prefer_no_csd` — server-side (borderless) by default,
    /// otherwise honoring whatever the client asked for.
    fn decoration_for(
        &self,
        toplevel: &ToplevelSurface,
        requested: Option<DecorationMode>,
    ) -> DecorationMode {
        if toplevel.parent().is_some() {
            DecorationMode::ClientSide
        } else if self.prefer_no_csd {
            DecorationMode::ServerSide
        } else {
            requested.unwrap_or(DecorationMode::ClientSide)
        }
    }

    /// Resolve and send a toplevel's decoration mode. Called for every
    /// xdg-decoration request (create / set / unset).
    fn apply_decoration(&self, toplevel: &ToplevelSurface, requested: Option<DecorationMode>) {
        let mode = self.decoration_for(toplevel, requested);
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
    }

    /// Configure a toplevel as maximized to the usable area (output minus any
    /// exclusive-zone reservations, e.g. the OSK), so an app that opens while an
    /// OSK is up already fits above it. xdg sizes are logical; the client scales
    /// its buffer up by `dpi`.
    ///
    /// Maximized — not Fullscreen — is the normal app state: it fills the screen
    /// but leaves a client-side header bar visible (toolkits hide it in the
    /// Fullscreen state), which is what keeps a dialog's buttons on screen.
    fn configure_maximized(&self, surface: &ToplevelSurface) {
        let usable = self.layers.usable(self.dpi);
        let w = (usable.w as f64 / self.dpi).round() as i32;
        let h = (usable.h as f64 / self.dpi).round() as i32;
        let deco = self.decoration_for(surface, None);
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
            state.decoration_mode = Some(deco);
            state.states.unset(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Maximized);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.apply_decoration(&toplevel, None);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        self.apply_decoration(&toplevel, Some(mode));
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.apply_decoration(&toplevel, None);
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
delegate_text_input_manager!(State);
delegate_input_method_manager!(State);
smithay::delegate_image_capture_source!(State);
smithay::delegate_output_capture_source!(State);
smithay::delegate_image_copy_capture!(State);

impl ImageCaptureSourceHandler for State {}

impl OutputCaptureSourceHandler for State {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source
    }

    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        // Stash the output on the source so `capture_constraints` can recover it.
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for State {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        // Only our output is a valid source; recover it from the source's data.
        let output = source
            .user_data()
            .get::<smithay::output::WeakOutput>()?
            .upgrade()?;
        let mode = output.current_mode()?;
        let size = mode
            .size
            .to_logical(1)
            .to_buffer(1, smithay::utils::Transform::Normal);
        // Advertise dmabuf (zero-copy blit) when the DRM backend supplied formats,
        // plus shm as a universal fallback so a client can always allocate.
        let dma = self
            .capture_formats
            .as_ref()
            .map(|(node, formats)| smithay::wayland::image_copy_capture::DmabufConstraints {
                node: *node,
                formats: formats.clone(),
            });
        Some(BufferConstraints {
            size,
            shm: vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
            ],
            dma,
        })
    }

    fn new_session(&mut self, _session: CaptureSession) {
        // Sessions clean up on drop; we only act per-frame.
    }

    fn frame(&mut self, _session: &CaptureSessionRef, frame: CaptureFrame) {
        // Defer the actual capture to the render loop, which owns the renderer and
        // the just-composited scene. `success`/`fail` happens there.
        self.pending_captures.push(frame);
        self.needs_render = true;
    }
}

impl InputMethodHandler for State {
    fn new_popup(&mut self, surface: ImePopupSurface) {
        // Track the IME popup like an xdg popup so it renders + hit-tests. It is
        // parented to the focused app surface, so `app_popups()` picks it up
        // automatically (PopupManager::popups_for_surface yields all kinds).
        if let Err(e) = self.popups.track_popup(PopupKind::from(surface)) {
            warn!(?e, "failed to track input-method popup");
        }
        self.needs_render = true;
    }

    fn dismiss_popup(&mut self, surface: ImePopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = PopupManager::dismiss_popup(&parent, &PopupKind::from(surface));
        }
        self.needs_render = true;
    }

    fn popup_repositioned(&mut self, _surface: ImePopupSurface) {}

    fn parent_geometry(&self, _parent: &WlSurface) -> Rectangle<i32, smithay::utils::Logical> {
        // Apps are fullscreen within the usable area; report it in logical coords
        // (client space) so the IME positions its popup over the focused field.
        let u = self.layers.usable(self.dpi);
        Rectangle::from_size(
            (
                (u.w as f64 / self.dpi).round() as i32,
                (u.h as f64 / self.dpi).round() as i32,
            )
                .into(),
        )
    }
}

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
        // smithay's LayerMap tracks geometry + reservations and sends the
        // initial configure on the surface's first commit.
        self.layers.new_surface(surface, namespace);
    }

    fn layer_destroyed(&mut self, surface: smithay::wayland::shell::wlr_layer::LayerSurface) {
        if self.layers.destroyed(&surface) {
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
    let rounded_tex_shader =
        match render::compile_rounded_tex_shader(gfx_backend.renderer()) {
            Ok(prog) => prog,
            Err(err) => {
                error!(err = ?err, "failed to compile rounded-corner shader");
                return;
            }
        };

    let actual_size = gfx_backend.window_size();
    info!(w = actual_size.w, h = actual_size.h, "actual output size");
    let mut state = State::new(&display, socket_name.clone(), (actual_size.w, actual_size.h));
    // Winit has no DRM main device; a version-3 global is fine (no recording).
    state.init_dmabuf_global(&display.handle(), gfx_backend.renderer().dmabuf_formats(), None);

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
        if let Err(err) = render_frame(&mut gfx_backend, &mut state, &rounded_tex_shader) {
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
    if let Err(e) = persist::save(&state.model, &state_path()) {
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
    rounded_tex_shader: &GlesTexProgram,
) -> Result<(), SwapBuffersError> {
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);
    let frame_start = std::time::Instant::now();

    // Fixed 90 Hz step for the dev backend.
    let prep = state.advance_frame(1.0 / 90.0);

    keybinds::poll(state);
    state.poll_launching();
    state.sync_keyboard_focus();

    let (renderer, mut framebuffer) = backend.bind()?;
    // Screen-space animated grid centers: global spring position minus the
    // current page scroll, so the grid pass in draw_home can render sliding
    // icons without knowing about pages itself.
    let page_scroll = if let UiState::Home { page_spring, .. } = &state.ui {
        page_spring.value
    } else {
        0.0
    };
    let gp_w = state.output_size.0 as f32;
    let grid_positions: std::collections::HashMap<String, (f32, f32)> = state
        .grid_anim
        .iter()
        .map(|(app, (sx, sy))| (app.clone(), (sx.value - page_scroll * gp_w, sy.value)))
        .collect();
    let dock_positions: std::collections::HashMap<String, (f32, f32)> = state
        .dock_anim
        .iter()
        .map(|(a, (sx, sy))| (a.clone(), (sx.value, sy.value)))
        .collect();
    {
        // Hoisted so the `arrange` closure below captures a plain `usize`, not a
        // borrow of `state` (which would clash with `skia: &mut state.skia`).
        let cur_home_page = state.current_home_page();
        let mut ctx = render::DrawCtx {
            scene: &prep.scene,
            app_surface: prep.app_surface.as_ref(),
            skia: &mut state.skia,
            model: &state.model,
            icon_cache: &state.icon_cache,
            app_catalog: &state.app_catalog,
            toplevels: &state.toplevels,
            app_scale: state.dpi,
            app_origin: {
                let u = state.layers.usable(state.dpi);
                (u.x.round() as i32, u.y.round() as i32)
            },
            transform: Transform::Flipped180,
            skia_flip_y: false,
            frame_time: prep.frame_time,
            osd: prep.osd_view,
            touches: &prep.touch_marks,
            layers_below: &prep.layers_below,
            layers_above: &prep.layers_above,
            app_popups: &prep.app_popups,
            layer_popups: &prep.layer_popups,
            bar_alpha: prep.bar_alpha,
            pressed_app: state.pending_launch.as_ref().map(|p| p.app_id.as_str()),
            launching_app: state.launching.as_ref().map(|l| l.app_id.as_str()),
            launching_elapsed: state
                .launching
                .as_ref()
                .map_or(0.0, |l| l.started.elapsed().as_secs_f32()),
            arrange: state.arrange.as_ref().map(|a| {
                let drag_app = a.drag.as_ref().map(|d| d.app_id.as_str());
                let drag_pos = a.drag.as_ref().map(|d| d.cur);
                let over_dock = a
                    .drag
                    .as_ref()
                    .map(|d| {
                        // Only a grid-sourced drag can pin, so only highlight the
                        // dock drop target for those (a dock->dock drag is a no-op).
                        if d.source != input_dispatch::IconSource::Grid {
                            return false;
                        }
                        let (w, h) = (state.output_size.0 as f32, state.output_size.1 as f32);
                        let layout = sc_layout::compute(w, h, cur_home_page, &state.model);
                        layout.dock_zone.contains(d.cur.0, d.cur.1)
                    })
                    .unwrap_or(false);
                render::ArrangeView {
                    drag_app,
                    drag_pos,
                    over_dock,
                }
            }),
            // winit dev backend submits full damage; no partial hint.
            report_partial_damage: false,
            last_present: &mut state.last_present,
            grid_positions: &grid_positions,
            dock_positions: &dock_positions,
            rounded_tex_shader,
        };
        render::draw_scene(renderer, &mut framebuffer, size, &mut ctx)?;
    }

    drop(framebuffer);
    let result = backend.submit(Some(&[damage]));

    // Record + periodically log frame timing.
    state.record_and_log_frame(frame_start);

    // The winit backend has no dmabuf capture path, so fail any pending
    // screencopy frames rather than leave a recorder waiting forever. Real
    // capture is the DRM backend's job.
    for frame in std::mem::take(&mut state.pending_captures) {
        frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Unknown);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reflow_targets_maps_pages_and_excludes_dock() {
        let mut m = ShellModel::default();
        for i in 0..25 { m.place(format!("app{i:02}")); } // 24 on page 0, 1 on page 1
        let t = reflow_targets(&m, 1224.0, 2700.0);
        assert_eq!(t.len(), 25);
        let page1_app = &m.pages[1][0];
        assert!(t[page1_app].0 > 1224.0);
        let page0_app = &m.pages[0][0];
        assert!(t[page0_app].0 < 1224.0);
    }

    #[test]
    fn working_order_opens_hole_at_hover() {
        let pages = vec![vec!["a".to_string(), "b".into(), "c".into(), "d".into()]];
        // Drag "a", hover global index 2 -> order without "a" is [b,c,d];
        // hole at 2 -> [b, c, HOLE, d].
        let out = working_order(&pages, "a", Some((0, 2)));
        assert_eq!(out[0], vec!["b".to_string(), "c".into(), HOLE.to_string(), "d".into()]);
    }

    #[test]
    fn working_order_no_hole_when_hover_none() {
        let pages = vec![vec!["a".to_string(), "b".into(), "c".into()]];
        let out = working_order(&pages, "a", None);
        assert_eq!(out[0], vec!["b".to_string(), "c".into()]);
    }
}
