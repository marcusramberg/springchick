//! The central [`State`]. Behaviour lives in sibling modules that `impl State`.

use std::collections::{HashMap, HashSet};
use std::process::Child;
use std::time::Duration;

use smithay::backend::allocator::Format as DrmFormat;
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::drm::DrmNode;
use smithay::desktop::{PopupKind, PopupManager};
use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Clock, Monotonic};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::image_capture_source::{ImageCaptureSourceState, OutputCaptureSourceState};
use smithay::wayland::image_copy_capture::{
    Frame as CaptureFrame, ImageCopyCaptureState, Session as CaptureSession,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::ext_data_control::DataControlState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::wlr_data_control::DataControlState as WlrDataControlState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xdg_activation::XdgActivationState;

use sc_catalog::AppEntry;
use sc_icons::IconPixels;
use sc_shell_model::{persist, unix_now, ShellModel};

use tracing::{debug, trace, warn};

use crate::app_history::AppHistory;
use crate::arrange::ArrangeState;
use crate::ui_state::{ToplevelId, UiState, ZoomOrigin};
use crate::{
    background_effect, blank, content_type, debug_input, frame_stats, gamma_control, idle_inhibit,
    idle_notify, input_common, keybinds, layer_shell, osd, output_power, resources, rotation,
    scene, sensor, session_lock, skia_gl::SkiaGl, switcher, touch_viz,
};
use smithay::reexports::wayland_server::Resource;

pub(crate) struct AppToplevel {
    pub surface: ToplevelSurface,
    /// The launch it was attributed to ([`crate::provenance`]), else the
    /// client's id, else `unknown_N`. Drives icon, running dot and tap-to-raise.
    pub app_id: String,
    /// A launch-owned id is authoritative: `resolve_app_id` must not replace it
    /// with what the client announces (`foot` for `Terminal=true`).
    pub id_from_launch: bool,
    /// The client's own xdg `app_id`; icon fallback when no launch claimed it.
    pub wl_app_id: String,
    /// So the size log fires on change, not every commit.
    pub logged_size: Option<(i32, i32)>,
    /// The rotation this window was last configured at, i.e. how its buffer is
    /// oriented. Differs from the view once the app is backgrounded; a card
    /// drawn from the buffer is turned by the difference.
    pub rotation: crate::rotation::Rotation,
}

/// Spawned but not yet mapped; its icon pulses. Several can overlap ("new
/// window"), so attribution must pick the right one.
pub(crate) struct Launching {
    pub app_id: String,
    pub child: Child,
    /// Kept apart from `child` because reaping consumes the handle.
    pub pid: i32,
    /// Handed to the child's env; a client presenting it names its launch.
    pub token: String,
    pub started: std::time::Instant,
}

/// A daemonizing or hung launcher may never map a window.
pub(crate) const LAUNCH_PULSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-app icon overlays for the home pass. Refilled in place each frame:
/// rebuilding cost ~30 `String` allocations per frame at 90 Hz.
#[derive(Default)]
pub(crate) struct IconOverlays {
    /// Screen space: spring position minus the page scroll.
    pub grid_positions: HashMap<String, (f32, f32)>,
    /// Dock icons don't page, so these are the springs as-is.
    pub dock_positions: HashMap<String, (f32, f32)>,
    /// `(app_id, seconds since spawn)`.
    pub launch_pulses: Vec<(String, f32)>,
    pub running_apps: HashSet<String>,
}

/// Render snapshot from [`State::advance_frame`]. Backends add only their
/// output transform, Skia flip and framebuffer.
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
    pub touch_marks: Vec<touch_viz::TouchMark>,
    /// Physical px. `None` until a pointer moves, or after a finger takes over.
    pub cursor: Option<(f32, f32)>,
    /// Anything but `Unlocked` replaces the whole scene.
    pub lock_view: session_lock::LockView,
    pub lock_surface: Option<WlSurface>,
    pub icon_menu: Option<crate::render::MenuView>,
    pub library: Option<crate::render::LibraryView>,
    pub folder: Option<crate::render::FolderView>,
    /// The OSK sliding out: its held buffer and where it's drawn.
    pub closing: Option<(smithay::backend::renderer::utils::Buffer, sc_layout::Rect)>,
    pub card_chrome: crate::render::CardChromeView,
    /// 0..=1 black over everything: the rotation dip.
    pub dim: f32,
}

/// `(kind, origin, size)`, physical. Chains are root→leaf.
pub(crate) type PopupRect = (PopupKind, (i32, i32), (i32, i32));

/// `(render node, [(fourcc, modifiers)])`.
pub(crate) type CaptureFormats = (DrmNode, Vec<(Fourcc, Vec<Modifier>)>);

/// Gets a slide-up open and stays out of the switcher and MRU history.
pub(crate) const SEARCH_APP_ID: &str = "chick.springchick.Search";
pub(crate) const SEARCH_APP_EXEC: &str = "sc-search";

#[derive(Default)]
pub(crate) struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        match reason {
            DisconnectReason::ConnectionClosed => {
                debug!(?client_id, "client disconnected (connection closed)")
            }
            other => tracing::warn!(?client_id, ?other, "client disconnected"),
        }
    }
}

/// The icon-theme search path is built once, not per icon.
fn scan_catalog() -> (
    HashMap<String, AppEntry>,
    HashMap<String, IconPixels>,
    Vec<sc_catalog::Folder>,
) {
    let entries = sc_catalog::scan_apps();
    // From the ordered scan, not the map, so the library doesn't reshuffle.
    let folders = sc_catalog::folders(&entries);
    let app_catalog: HashMap<String, AppEntry> =
        entries.into_iter().map(|e| (e.id.clone(), e)).collect();
    let icon_dirs = sc_icons::theme_dirs(&sc_catalog::xdg_data_dirs());
    let icon_cache = app_catalog
        .iter()
        .map(|(id, entry)| {
            (
                id.clone(),
                sc_icons::resolve_with_dirs(&entry.icon, &icon_dirs),
            )
        })
        .collect();
    (app_catalog, icon_cache, folders)
}

fn reconcile_catalog(model: &mut ShellModel, catalog: &HashMap<String, AppEntry>, first_run: bool) {
    let mut catalog_ids: Vec<String> = catalog.keys().cloned().collect();
    catalog_ids.sort(); // deterministic seeding
    model.reconcile(&catalog_ids, unix_now(), first_run);
}

pub(crate) struct State {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Populated in `new_popup`.
    pub popups: PopupManager,
    #[allow(dead_code)] // Keeps the global registered.
    pub xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)] // Keeps the global registered.
    pub xdg_dialog_state: XdgDialogState,
    pub shm_state: ShmState,
    /// Global created by the backend once importable formats are known.
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)] // Keeps the global registered.
    pub dmabuf_global: Option<DmabufGlobal>,
    /// `focus_changed` doesn't get a `DisplayHandle`, but pointing the selection
    /// devices needs one.
    pub dh: DisplayHandle,
    pub data_device_state: DataDeviceState,
    /// Middle-click paste; terminals expect it.
    pub primary_selection_state: PrimarySelectionState,
    pub data_control_state: DataControlState,
    /// For wlr-era clients (wl-clipboard < 2.2, clipman, cliphist).
    #[allow(dead_code)] // Keeps the global registered.
    pub wlr_data_control_state: WlrDataControlState,
    pub seat_state: SeatState<Self>,
    #[allow(dead_code)] // Keeps the global registered.
    pub seat: Seat<Self>,
    pub keyboard: smithay::input::keyboard::KeyboardHandle<Self>,
    /// To avoid re-sending focus.
    pub focused_surface: Option<WlSurface>,
    pub keys: keybinds::Keys,
    /// Acted on by DRM; inert under winit.
    pub blank: blank::Blank,
    pub idle: blank::Idle,
    /// With a second screen lit, a blanked phone panel isn't "asleep", so typing
    /// must reach the app. Maintained by DRM on hotplug.
    pub external_display: bool,
    /// Makes the vblank-driven DRM loop render on the next wake. Inert under
    /// winit.
    pub needs_render: bool,
    /// Served by the backends, which own the renderer.
    pub screenshot_pending: bool,
    /// See [`crate::render::DrawCtx::last_present`].
    pub last_present: Option<(WlSurface, smithay::backend::renderer::utils::CommitCounter)>,
    pub osd: osd::Osd,
    pub layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    /// Held to keep the globals advertised.
    #[allow(dead_code)]
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    /// Held to keep the global advertised.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    /// Held to keep the globals advertised.
    #[allow(dead_code)]
    pub image_capture_source: ImageCaptureSourceState,
    #[allow(dead_code)]
    pub output_capture_source: OutputCaptureSourceState,
    pub image_copy_capture: ImageCopyCaptureState,
    /// Set by the DRM backend; `None` under winit.
    pub capture_formats: Option<CaptureFormats>,
    pub pending_captures: Vec<CaptureFrame>,
    /// Separate because the two protocols reply differently.
    pub wlr_captures: Vec<crate::wlr_screencopy::PendingCopy>,
    /// Dropping a `Session` sends `stopped` and fails the client's frames.
    pub capture_sessions: Vec<CaptureSession>,
    pub layers: layer_shell::LayerShell,
    pub touch: smithay::input::touch::TouchHandle<Self>,
    /// Client-routed touch slots and their coord scale (`dpi`). Per slot, so one
    /// finger's up never clears another's; slots on empty space are absent and
    /// drive the gesture funnel.
    pub touch_targets: HashMap<smithay::backend::input::TouchSlot, f64>,
    /// The one slot driving the single-touch gesture funnel.
    pub gesture_slot: Option<smithay::backend::input::TouchSlot>,
    /// Pointer press held on a client surface.
    pub pointer_grab: bool,
    /// False under winit, where the host draws the cursor.
    pub cursor_overlay: bool,
    /// Off until a pointer device moves; a touch-down hides it again.
    pub cursor_visible: bool,
    /// Popups that issued `xdg_popup.grab()`. Only these capture touch and
    /// dismiss on outside presses; non-grab popups (wvkbd's hack popup,
    /// tooltips) must not, or OSK and app input break.
    pub popup_grabs: std::collections::HashSet<WlSurface>,
    /// Occlusion fade: 0 when a Top/Overlay surface covers the pill.
    pub bar_alpha: f32,
    pub show_touches: bool,
    pub touch_viz: touch_viz::TouchViz,

    pub ui: UiState,
    pub model: ShellModel,
    pub app_catalog: HashMap<String, AppEntry>,
    pub icon_cache: HashMap<String, IconPixels>,
    /// Derived from the catalog, never persisted.
    pub folders: Vec<sc_catalog::Folder>,
    pub folder: Option<crate::library::OpenFolder>,
    /// `(index, start pos)`; opens on a release within the tap slop.
    pub pending_folder: Option<(usize, (f32, f32))>,
    /// Icon textures are keyed by app id alone; a new generation drops them.
    pub catalog_gen: u64,
    pub toplevels: Vec<Option<AppToplevel>>,
    /// Commits from anything else can't change the screen.
    pub drawn_toplevels: Vec<ToplevelId>,
    pub children: Vec<Child>,
    pub launching: Vec<Launching>,
    /// Catalog is rescanned when one exits.
    pub uninstalling: Vec<Child>,
    pub xdg_activation_state: XdgActivationState,
    /// Presented tokens by surface, matched against `launching` at register.
    /// The token, not the app id, so two launches of one app stay distinct.
    pub pending_activation: HashMap<WlSurface, String>,
    pub history: AppHistory,
    pub last_origin: ZoomOrigin,
    /// Physical, fixed at construction. The shell lays out in
    /// [`Self::output_size`], which is this turned by the view rotation.
    pub panel_size: (i32, i32),
    /// Surfaces `enter` it to learn the scale.
    pub output: Output,
    /// May be fractional. Client buffers are `logical * dpi`.
    pub dpi: f64,
    pub card_radius: f32,
    /// Dialogs always keep CSD so their action buttons survive.
    pub prefer_no_csd: bool,
    pub natural_scroll: bool,
    /// Startup only.
    pub uclamp_min: sc_config::UclampMin,
    /// DRM only, startup only.
    pub vrr: bool,
    pub resources: sc_config::Resources,
    /// `None` for no pinning.
    pub bg_allowed_cpus: Option<String>,
    /// The app in the foreground tier, for demotion on focus change.
    pub tiered: Option<resources::AppCgroup>,
    pub gamma: gamma_control::GammaControl,
    pub output_power: output_power::OutputPower,
    pub idle_notify: idle_notify::IdleNotify,
    pub idle_inhibit: idle_inhibit::IdleInhibit,
    #[allow(dead_code)]
    pub content_type: content_type::ContentType,
    #[allow(dead_code)]
    pub background_effect: background_effect::BackgroundEffect,
    pub session_lock: session_lock::SessionLock,
    /// Held to keep the globals alive; the work is in [`crate::pacing`].
    _fifo_manager: smithay::wayland::fifo::FifoManagerState,
    _commit_timing_manager: smithay::wayland::commit_timing::CommitTimingManagerState,
    /// Read through [`Self::view_rotation`].
    pub rotation: rotation::Rotation,
    /// From the accelerometer or the `orientation` ipc verb. `Normal` without a
    /// sensor.
    pub device_orientation: rotation::DeviceOrientation,
    pub orientation_settle: rotation::Settle,
    pub rotation_fade: rotation::Fade,
    /// Size configured by the turn being faded through; the first commit at it
    /// ends the dark stretch early.
    pub rotation_await_size: Option<(i32, i32)>,
    /// `None` without an accelerometer (dev box, VM).
    pub sensor: Option<sensor::Sensor>,
    /// Foreground app is fullscreen landscape content
    /// ([`content_type::wants_landscape`]).
    pub landscape_hint: bool,

    pub skia: SkiaGl,
    pub wayland_socket: String,

    pub last_pointer_pos: Option<(f32, f32)>,
    /// Physical, including touches routed to clients, so the compositor can
    /// take a sequence over ([`State::lift_from_search`]).
    pub last_touch_pos: Option<(f32, f32)>,
    pub pointer_down: bool,
    pub page_drag: Option<input_common::FingerDrag>,
    /// Feeds the tracker real elapsed time. Seeded on press.
    pub last_motion: Option<std::time::Instant>,
    pub bar_drag_start: Option<(f32, f32)>,
    /// Pending tap-to-launch; cleared if the finger starts a page swipe.
    pub pending_launch: Option<input_common::PendingLaunch>,
    pub icon_press: Option<crate::arrange::IconPress>,
    pub arrange: Option<ArrangeState>,
    pub icon_menu: Option<crate::icon_menu::IconMenu>,
    pub bg_press: Option<crate::arrange::BgPress>,
    /// Start of an empty-space Home press that may become the search pull-down.
    pub search_arm: Option<(f32, f32)>,
    /// The next toplevel to map is the search app. Its `app_id` isn't readable
    /// yet at `new_toplevel`, so the spawn intent is the signal.
    pub expecting_search: bool,
    pub switcher_drag: input_common::SwitcherDrag,
    /// For hit-testing during a drag.
    pub switcher_cards: Vec<switcher::CardRect>,
    pub card_chrome: switcher::CardChrome,
    pub kbd_switch: Option<crate::kbd_switch::KbdSwitch>,
    /// Multiplied into `bar_alpha`.
    pub bar_hint: crate::bar_hint::BarHint,
    pub active_gesture: Option<debug_input::ActiveGesture>,
    pub active_key: Option<debug_input::ActiveKey>,
    pub active_touch: Option<debug_input::ActiveTouch>,
    pub pending_settle: Option<(std::sync::mpsc::SyncSender<String>, std::time::Instant)>,
    /// Variant plus front toplevel: an app swap stays in `App` and would
    /// otherwise log nothing.
    pub last_log_state: Option<(std::mem::Discriminant<UiState>, Option<ToplevelId>)>,
    /// Seeded lazily, kept in sync with `model.pages` by `reflow_grid`.
    pub grid_anim: HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,
    pub dock_anim: HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,
    pub icon_overlays: IconOverlays,

    /// CLOCK_MONOTONIC for every client timestamp: frame callbacks, input times
    /// and presentation feedback. Clients mix them, so they must share a base.
    pub clock: Clock<Monotonic>,

    pub stats: frame_stats::FrameStats,
    pub perf_log: bool,
    pub last_perf_log: std::time::Instant,
    /// For the per-frame `gap_ms` trace.
    pub last_frame_end: Option<std::time::Instant>,

    pub running: bool,
}

impl State {
    pub(crate) fn new(
        display: &Display<Self>,
        wayland_socket: String,
        output_size: (i32, i32),
    ) -> Self {
        let dh = display.handle();
        let (out_w, out_h) = output_size;

        // Read once; `[main]` and `[keybinds]` must see the same file.
        let config = sc_config::load();
        let dpi = config.dpi.max(1.0);
        let idle_blank_secs = config.idle_blank_secs;
        let card_radius = config.card_radius;
        let show_touches = config.show_touches;
        let prefer_no_csd = config.prefer_no_csd;
        let natural_scroll = config.natural_scroll;
        let uclamp_min = config.uclamp_min;
        let vrr = config.vrr;
        let resources = config.resources.clone();
        let bg_allowed_cpus = resources::resolve_allowed_cpus(&resources.bg_allowed_cpus);
        let config_rotation_settle_ms = config.rotation_settle_ms;
        let config_rotation_fade_ms = config.rotation_fade_ms;

        // They inherit `WAYLAND_DISPLAY` from our env.
        let mut children = Vec::new();
        for command in &config.startup {
            crate::keybinds::spawn_command(command, &mut children);
        }

        // v6: wvkbd binds wl_compositor@6.
        let compositor_state = CompositorState::new_v6::<Self>(&dh);
        // Only Fullscreen: hinting the rest absent makes GTK drop those buttons from
        // CSD dialogs.
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &dh,
            [xdg_toplevel::WmCapabilities::Fullscreen],
        );
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        // Portal file choosers have no in-process parent; the dialog hint is the
        // only thing marking them for CSD.
        let xdg_dialog_state = XdgDialogState::new::<Self>(&dh);
        let xdg_activation_state = XdgActivationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let dmabuf_state = DmabufState::new();
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);
        // Both ext- and wlr- flavours; clients pick one.
        let data_control_state =
            DataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_client| true);
        let wlr_data_control_state =
            WlrDataControlState::new::<Self, _>(&dh, Some(&primary_selection_state), |_client| {
                true
            });
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "springchick");
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 200, 25)
            .expect("add keyboard");
        seat.add_pointer();
        let touch = seat.add_touch();

        let layer_shell_state =
            smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<Self>(&dh);
        // wvkbd ignores integer output scale; it needs fractional scale.
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let image_capture_source = ImageCaptureSourceState::new();
        let output_capture_source = OutputCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture = ImageCopyCaptureState::new::<Self>(&dh);
        smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState::new::<Self, _>(
            &dh,
            |_client| true,
        );
        let fifo_manager = smithay::wayland::fifo::FifoManagerState::new::<Self>(&dh);
        let commit_timing_manager =
            smithay::wayland::commit_timing::CommitTimingManagerState::new::<Self>(&dh);
        smithay::wayland::presentation::PresentationState::new::<Self>(
            &dh,
            libc::CLOCK_MONOTONIC as u32,
        );
        smithay::wayland::text_input::TextInputManagerState::new::<Self>(&dh);
        smithay::wayland::input_method::InputMethodManagerState::new::<Self, _>(&dh, |_client| {
            true
        });

        // 256 is a placeholder; DRM sets the real CRTC gamma_length.
        let gamma = gamma_control::GammaControl::new(&dh, 256);

        let output_power = output_power::OutputPower::new(&dh);

        // Gamma and output-power managers must come before wl_output: dms binds its
        // per-output controls from its wl_output callback and reports "no outputs"
        // otherwise.
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
            refresh: 90_000,
        };
        output.change_current_state(
            Some(mode),
            None,
            Some(smithay::output::Scale::Fractional(dpi)),
            None,
        );
        output.set_preferred(mode);
        output.create_global::<Self>(&dh);
        // wl-screenrec needs xdg-output to pick the output.
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        crate::wlr_screencopy::init(&dh);

        let idle_notify = idle_notify::IdleNotify::new(&dh, std::time::Instant::now());
        let idle_inhibit = idle_inhibit::IdleInhibit::new(&dh);
        let content_type = content_type::ContentType::new(&dh);
        let background_effect = background_effect::BackgroundEffect::new(&dh);
        let session_lock = session_lock::SessionLock::new(&dh);

        let model = persist::load(&persist::state_path()).unwrap_or_default();
        let (app_catalog, icon_cache, folders) = scan_catalog();

        let mut model = model;
        let first_run = model.frecency.apps.is_empty();
        reconcile_catalog(&mut model, &app_catalog, first_run);

        // +1 for the library page.
        let page_count = model.pages.len().max(1) + 1;
        let ui = UiState::home(0, page_count);

        State {
            compositor_state,
            xdg_shell_state,
            popups: PopupManager::default(),
            xdg_decoration_state,
            xdg_dialog_state,
            shm_state,
            dmabuf_state,
            dmabuf_global: None,
            dh: dh.clone(),
            data_device_state,
            primary_selection_state,
            data_control_state,
            wlr_data_control_state,
            seat_state,
            seat,
            keyboard,
            focused_surface: None,
            keys: keybinds::Keys::from_config(config),
            blank: blank::Blank::new(),
            idle: blank::Idle::new(idle_blank_secs, std::time::Instant::now()),
            external_display: false,
            needs_render: false,
            screenshot_pending: false,
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
            capture_sessions: Vec::new(),
            wlr_captures: Vec::new(),
            layers: layer_shell::LayerShell::new(output.clone(), out_w as f32, out_h as f32),
            touch,
            touch_targets: HashMap::new(),
            gesture_slot: None,
            pointer_grab: false,
            cursor_overlay: false,
            cursor_visible: false,
            popup_grabs: std::collections::HashSet::new(),
            bar_alpha: 1.0,
            show_touches,
            touch_viz: touch_viz::TouchViz::new(),
            ui,
            model,
            app_catalog,
            icon_cache,
            folders,
            folder: None,
            pending_folder: None,
            catalog_gen: 0,
            toplevels: Vec::new(),
            drawn_toplevels: Vec::new(),
            children,
            launching: Vec::new(),
            uninstalling: Vec::new(),
            xdg_activation_state,
            pending_activation: HashMap::new(),
            history: AppHistory::new(),
            last_origin: ZoomOrigin::icon((out_w as f32 / 2.0, out_h as f32 / 2.0)),
            panel_size: output_size,
            output,
            dpi,
            card_radius,
            prefer_no_csd,
            natural_scroll,
            uclamp_min,
            vrr,
            resources,
            bg_allowed_cpus,
            tiered: None,
            gamma,
            output_power,
            idle_notify,
            idle_inhibit,
            content_type,
            background_effect,
            session_lock,
            _fifo_manager: fifo_manager,
            _commit_timing_manager: commit_timing_manager,
            rotation: rotation::Rotation::None,
            device_orientation: rotation::DeviceOrientation::Normal,
            orientation_settle: rotation::Settle::new(
                config_rotation_settle_ms,
                rotation::DeviceOrientation::Normal,
            ),
            rotation_fade: rotation::Fade::new(config_rotation_fade_ms),
            rotation_await_size: None,
            sensor: sensor::spawn(),
            landscape_hint: false,
            skia: SkiaGl::new(),
            wayland_socket,
            last_pointer_pos: None,
            last_touch_pos: None,
            pointer_down: false,
            page_drag: None,
            last_motion: None,
            bar_drag_start: None,
            pending_launch: None,
            icon_press: None,
            arrange: None,
            icon_menu: None,
            bg_press: None,
            search_arm: None,
            expecting_search: false,
            switcher_drag: input_common::SwitcherDrag::None,
            switcher_cards: Vec::new(),
            card_chrome: switcher::CardChrome::new(),
            kbd_switch: None,
            bar_hint: crate::bar_hint::BarHint::new(),
            active_gesture: None,
            active_key: None,
            active_touch: None,
            pending_settle: None,
            last_log_state: None,
            grid_anim: HashMap::new(),
            dock_anim: HashMap::new(),
            icon_overlays: IconOverlays::default(),
            clock: Clock::new(),
            stats: frame_stats::FrameStats::new(Duration::from_micros(11_111)),
            perf_log: false,
            last_perf_log: std::time::Instant::now(),
            last_frame_end: None,
            running: true,
        }
    }

    /// Called once the renderer exists. With `main_device` (DRM) it binds v4
    /// with feedback; wl-screenrec rejects a v3 global.
    pub(crate) fn init_dmabuf_global(
        &mut self,
        dh: &DisplayHandle,
        formats: impl IntoIterator<Item = DrmFormat>,
        main_device: Option<libc::dev_t>,
    ) {
        let formats: Vec<DrmFormat> = formats.into_iter().collect();
        let feedback = main_device.and_then(|dev| {
            DmabufFeedbackBuilder::new(dev, formats.iter().copied())
                .build()
                .ok()
        });
        let global = match feedback {
            Some(feedback) => self
                .dmabuf_state
                .create_global_with_default_feedback::<Self>(dh, &feedback),
            None => self.dmabuf_state.create_global::<Self>(dh, formats),
        };
        self.dmabuf_global = Some(global);
    }

    /// Drop every in-flight gesture without acting on it (e.g. the session
    /// locked mid-gesture). Client slots are dropped too.
    pub(crate) fn cancel_gestures(&mut self) {
        self.pointer_down = false;
        self.pointer_grab = false;
        self.gesture_slot = None;
        self.touch_targets.clear();
        self.page_drag = None;
        self.last_motion = None;
        self.bar_drag_start = None;
        self.pending_launch = None;
        self.icon_press = None;
        self.bg_press = None;
        self.search_arm = None;
        self.switcher_drag = input_common::SwitcherDrag::None;
        if let Some(arrange) = self.arrange.as_mut() {
            arrange.drag = None;
        }
    }

    /// `dpi` isn't re-applied: it's baked into committed buffer sizes.
    /// `prefer_no_csd` applies to the next window that negotiates.
    pub(crate) fn reload_config(&mut self) {
        let config = sc_config::load();
        self.card_radius = config.card_radius;
        self.prefer_no_csd = config.prefer_no_csd;
        self.natural_scroll = config.natural_scroll;
        self.show_touches = config.show_touches;
        self.idle = blank::Idle::new(config.idle_blank_secs, std::time::Instant::now());
        self.orientation_settle.set_hold(config.rotation_settle_ms);
        // A fade in flight keeps its duration.
        self.rotation_fade.set_duration(config.rotation_fade_ms);
        self.resources = config.resources.clone();
        self.bg_allowed_cpus = resources::resolve_allowed_cpus(&self.resources.bg_allowed_cpus);
        // `children` stays on the existing `Keys`.
        self.keys.tracker = keybinds::Keys::from_config(config).tracker;
        // Re-apply to the focused app now.
        self.tiered = None;
        self.apply_resource_tiers();
    }

    /// Only resting states: `App` promotes, `Home` demotes. Transitions are left
    /// alone since each tier change forks `systemctl`. Not gated on
    /// `needs_animation`, or a stalled nested window would never tier.
    pub(crate) fn apply_resource_tiers(&mut self) {
        if !self.resources.enable {
            return;
        }
        let want = match &self.ui {
            UiState::App { toplevel, .. } => self.toplevel_unit(*toplevel),
            UiState::Home { .. } => None,
            _ => return,
        };
        if want == self.tiered {
            return;
        }
        let cpus = self.bg_allowed_cpus.clone();
        if let Some(old) = self.tiered.take() {
            let children = resources::apply(
                &old,
                resources::Tier::Background,
                &self.resources,
                cpus.as_deref(),
            );
            self.children.extend(children);
        }
        if let Some(new) = &want {
            let children = resources::apply(
                new,
                resources::Tier::Foreground,
                &self.resources,
                cpus.as_deref(),
            );
            self.children.extend(children);
        }
        self.tiered = want;
    }

    fn toplevel_unit(&self, tid: usize) -> Option<resources::AppCgroup> {
        let tl = self.toplevels.get(tid)?.as_ref()?;
        let client = tl.surface.wl_surface().client()?;
        let pid = client.get_credentials(&self.dh).ok()?.pid;
        resources::unit_of_pid(pid)
    }

    /// `first_run` seeding is never re-triggered; new apps seed cold.
    pub(crate) fn reload_catalog(&mut self) {
        let (app_catalog, icon_cache, folders) = scan_catalog();
        self.app_catalog = app_catalog;
        self.icon_cache = icon_cache;
        self.folders = folders;
        // Folder indices refer to a list that just changed.
        self.folder = None;
        self.catalog_gen = self.catalog_gen.wrapping_add(1);
        reconcile_catalog(&mut self.model, &self.app_catalog, false);
        if let Err(e) = persist::save(&self.model, &persist::state_path()) {
            warn!(?e, "failed to persist shell model after catalog reload");
        }
        // The grid draws from the springs, not the model.
        self.reflow_grid();
        self.reflow_dock();
        // A pruned page can leave the shell past the end.
        let page_count = self.home_page_count();
        if let UiState::Home {
            page,
            page_count: pc,
            ..
        } = &mut self.ui
        {
            *pc = page_count;
            *page = (*page).min(page_count - 1);
        }
        self.needs_render = true;
    }

    /// The panel, axis-swapped while turned.
    pub(crate) fn output_size(&self) -> (i32, i32) {
        self.view_rotation().app_size(self.panel_size)
    }

    pub(crate) fn output_size_f(&self) -> (f32, f32) {
        let (w, h) = self.output_size();
        (w as f32, h as f32)
    }

    /// Below/right of exclusive zones, or the view origin while turned.
    pub(crate) fn app_origin(&self) -> (f32, f32) {
        if self.view_rotation().swaps_axes() || self.foreground_is_fullscreen() {
            return (0.0, 0.0);
        }
        let u = self.layers.usable(self.dpi);
        (u.x, u.y)
    }

    /// The lock screen is always upright.
    pub(crate) fn view_rotation(&self) -> rotation::Rotation {
        if self.session_lock.is_locked() {
            rotation::Rotation::None
        } else {
            self.rotation
        }
    }

    /// For commit-timing targets. Falls back to 60Hz: a zero would release
    /// everything at once.
    pub(crate) fn output_refresh_interval(&self) -> Duration {
        const FALLBACK: Duration = Duration::from_nanos(16_666_666);
        let Some(mode) = self.output.current_mode() else {
            return FALLBACK;
        };
        if mode.refresh <= 0 {
            return FALLBACK;
        }
        Duration::from_nanos(1_000_000_000_000u64 / mode.refresh as u64)
    }

    pub(crate) fn current_home_page(&self) -> usize {
        if let UiState::Home { page, .. } = &self.ui {
            *page
        } else {
            0
        }
    }

    /// Logs a perf summary at most once a second.
    pub(crate) fn record_and_log_frame(&mut self, frame_start: std::time::Instant) {
        let dt = frame_start.elapsed();
        self.stats.record_frame(dt);
        // `springchick::perf=trace`. A large `gap_ms` marks the first frame after
        // idle, the one that pays schedutil's ramp; the aggregate can't show it.
        let now = std::time::Instant::now();
        trace!(
            target: "springchick::perf",
            "frame dt_ms={:.2} gap_ms={:.1}",
            dt.as_secs_f64() * 1000.0,
            self.last_frame_end
                .map(|t| (frame_start - t).as_secs_f64() * 1000.0)
                .unwrap_or(0.0),
        );
        self.last_frame_end = Some(now);
        if self.perf_log && self.last_perf_log.elapsed() >= Duration::from_secs(1) {
            debug!(target: "springchick::perf", "{}", self.stats.format_line());
            self.last_perf_log = std::time::Instant::now();
        }
    }
}
