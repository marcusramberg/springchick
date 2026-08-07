//! The compositor's central [`State`]: every protocol state object, the shell
//! model, input bookkeeping, and the render snapshot both backends consume.
//!
//! Behaviour lives in sibling modules that `impl State`:
//! - [`crate::toplevel`] — app window lifecycle, focus, decoration, rotation.
//! - [`crate::arrange`] — home-grid reflow springs and arrange-mode drag.
//! - [`crate::frame`] — per-frame advance, popups, animation gating.
//! - [`crate::handlers`] — the smithay protocol handler impls.

use std::collections::HashMap;
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
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;
use smithay::wayland::viewporter::ViewporterState;

use sc_catalog::AppEntry;
use sc_icons::IconPixels;
use sc_shell_model::{persist, unix_now, ShellModel};

use tracing::debug;

use crate::app_history::AppHistory;
use crate::arrange::ArrangeState;
use crate::ui_state::{ToplevelId, UiState, ZoomOrigin};
use crate::{
    background_effect, blank, content_type, debug_input, frame_stats, gamma_control, idle_inhibit,
    idle_notify, input_common, keybinds, layer_shell, osd, rotation, scene, sensor, session_lock,
    skia_gl::SkiaGl, switcher, touch_viz,
};

/// A running app's toplevel state.
pub(crate) struct AppToplevel {
    pub surface: ToplevelSurface,
    pub app_id: String,
    /// Last client-set xdg window geometry logged for this toplevel, so the
    /// size log fires on change instead of on every commit.
    pub logged_size: Option<(i32, i32)>,
}

/// An app spawned from the launcher but not yet mapped to a toplevel. Its Home
/// icon pulses until the window opens, the process dies, or we time out waiting.
pub(crate) struct Launching {
    pub app_id: String,
    pub child: Child,
    pub started: std::time::Instant,
}

/// How long to keep pulsing a launching icon before giving up (a daemonizing or
/// hung launcher may never map a window; stop breathing forever).
pub(crate) const LAUNCH_PULSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Backend-agnostic render snapshot produced by [`State::advance_frame`]. Holds
/// everything both backends feed into [`crate::render::DrawCtx`]; each backend
/// adds only its own output transform, Skia flip, and framebuffer binding.
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
    /// What the session lock wants on screen. Anything but
    /// [`session_lock::LockView::Unlocked`] replaces the whole scene.
    pub lock_view: session_lock::LockView,
    /// The lock surface to draw when `lock_view` is `Surface`.
    pub lock_surface: Option<WlSurface>,
    /// Screen-space animated center `(x, y)` per grid app: the reflow spring
    /// position minus the current page scroll, so the grid pass can render
    /// sliding icons without knowing about pages.
    pub grid_positions: HashMap<String, (f32, f32)>,
    /// Screen-space animated center `(x, y)` per dock app. Dock icons don't
    /// scroll with pages, so these are the spring positions as-is.
    pub dock_positions: HashMap<String, (f32, f32)>,
}

/// A popup and its clamped physical geometry: `(kind, origin, size)`. Chains are
/// ordered root→leaf.
pub(crate) type PopupRect = (PopupKind, (i32, i32), (i32, i32));

/// Capture buffer formats to advertise: `(render node, [(fourcc, modifiers)])`.
pub(crate) type CaptureFormats = (DrmNode, Vec<(Fourcc, Vec<Modifier>)>);

/// xdg `app_id` the pull-down search app sets on its toplevel. The compositor
/// recognises it to give it a slide-up open animation and to keep it out of the
/// task switcher / MRU history.
pub(crate) const SEARCH_APP_ID: &str = "chick.springchick.Search";
/// Exec line for the search app, spawned on the pull-down gesture.
pub(crate) const SEARCH_APP_EXEC: &str = "sc-search";

/// Per-client state.
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

/// Main compositor state.
pub(crate) struct State {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    /// Tracks xdg_popup trees (their lifecycle + geometry) so we can render,
    /// hit-test, and dismiss menus/dropdowns. Populated in `new_popup`.
    pub popups: PopupManager,
    #[allow(dead_code)] // Must stay alive to keep the global registered.
    pub xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)] // Must stay alive to keep the xdg-wm-dialog global registered.
    pub xdg_dialog_state: XdgDialogState,
    pub shm_state: ShmState,
    /// zwp_linux_dmabuf: lets GL clients (GTK4, etc.) share buffers zero-copy
    /// instead of falling back to slow shm software upload. The global is
    /// created by the backend once its renderer's importable formats are known.
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)] // Must stay alive to keep the dmabuf global registered.
    pub dmabuf_global: Option<DmabufGlobal>,
    pub data_device_state: DataDeviceState,
    /// ext-data-control clipboard-manager protocol state.
    pub data_control_state: DataControlState,
    pub seat_state: SeatState<Self>,
    #[allow(dead_code)] // Must stay alive to keep the wl_seat global registered.
    pub seat: Seat<Self>,
    /// Seat keyboard. Owned by State (not the winit loop) so both backends
    /// share one key path.
    pub keyboard: smithay::input::keyboard::KeyboardHandle<Self>,
    /// Surface currently holding keyboard focus, to avoid re-sending it.
    pub focused_surface: Option<WlSurface>,
    /// Resolved keybindings + in-flight press state.
    pub keys: keybinds::Keys,
    /// Panel blanking (acted on by the DRM backend; inert under winit).
    pub blank: blank::Blank,
    /// Idle-blank countdown. Reset by input in the DRM loop; when it elapses the
    /// loop flips `blank`. Inert under winit (which never polls it).
    pub idle: blank::Idle,
    /// Set when a client commit or input changed on-screen state, so the
    /// vblank-driven DRM loop re-primes a page-flip on the next wake. Inert
    /// under winit (which renders every loop iteration).
    pub needs_render: bool,
    /// Commit cursor for the partial page-flip damage hint: the fullscreen app
    /// surface and the `CommitCounter` last presented for it. See
    /// [`crate::render::DrawCtx::last_present`].
    pub last_present: Option<(WlSurface, smithay::backend::renderer::utils::CommitCounter)>,
    /// Volume on-screen display state.
    pub osd: osd::Osd,
    /// wlr-layer-shell protocol state.
    pub layer_shell_state: smithay::wayland::shell::wlr_layer::WlrLayerShellState,
    /// `wp_fractional_scale` + `wp_viewporter`: how HiDPI-unaware clients (layer
    /// surfaces like wvkbd, and apps) learn to render at `dpi`. Held only to keep
    /// the globals advertised for the compositor's lifetime.
    #[allow(dead_code)]
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    #[allow(dead_code)]
    pub viewporter_state: ViewporterState,
    /// `zxdg_output_manager_v1`: output name + logical geometry for clients that
    /// ask (recorders). Held to keep the global advertised.
    #[allow(dead_code)]
    pub output_manager_state: OutputManagerState,
    /// `ext-image-capture-source-v1` + `ext-image-copy-capture-v1` (screencopy):
    /// let recorders capture the output. Held to keep the globals advertised.
    #[allow(dead_code)]
    pub image_capture_source: ImageCaptureSourceState,
    #[allow(dead_code)]
    pub output_capture_source: OutputCaptureSourceState,
    pub image_copy_capture: ImageCopyCaptureState,
    /// Capture buffer formats to advertise, set by the DRM backend once the
    /// renderer exists: `(render node, [(fourcc, modifiers)])`. `None` on the
    /// winit backend (no dmabuf capture there).
    pub capture_formats: Option<CaptureFormats>,
    /// Capture frames awaiting a blit; drained by the render loop each frame.
    pub pending_captures: Vec<CaptureFrame>,
    /// wlr-screencopy copy requests awaiting the render loop. Separate from
    /// `pending_captures` because the two protocols reply differently.
    pub wlr_captures: Vec<crate::wlr_screencopy::PendingCopy>,
    /// Live screencopy sessions. Held because dropping a `Session` sends
    /// `stopped` and fails the client's frames (that dropped-on-arrival session
    /// is what made `grim` print "failed to copy output").
    pub capture_sessions: Vec<CaptureSession>,
    /// Tracked layer surfaces + reserved-area bookkeeping.
    pub layers: layer_shell::LayerShell,
    /// Seat touch handle, for forwarding taps to layer surfaces.
    pub touch: smithay::input::touch::TouchHandle<Self>,
    /// Per-slot touch routing. The phone panel delivers concurrent slots
    /// (fingers); each slot that lands on a client surface (OSK layer, app,
    /// popup) gets its own target here so one finger's up never clears another's
    /// grab, and a stray slot never leaks into the gesture funnel. Slots that
    /// start on empty space are absent (they drive the gesture funnel instead).
    /// Value is `(coord scale, rotated)`: the slot's `dpi` scale and whether it
    /// routes to the rotated fullscreen app (whose coords need turning first).
    /// Presence marks the slot client-routed.
    pub touch_targets: HashMap<smithay::backend::input::TouchSlot, (f64, bool)>,
    /// The single slot currently driving the home-screen gesture funnel
    /// (`input_common`), which is inherently single-touch. Only this slot feeds
    /// press/motion/release; additional fingers on empty space are ignored until
    /// it lifts.
    pub gesture_slot: Option<smithay::backend::input::TouchSlot>,
    /// Whether the pointer press is currently held on a client surface.
    pub pointer_grab: bool,
    /// wl_surfaces of popups that issued an `xdg_popup.grab()`. Only these are
    /// modal — they capture touch and dismiss on an outside press. Non-grab
    /// popups (wvkbd's input-enabling hack popup, app tooltips/comboboxes) are
    /// tracked and rendered but must NOT steal or dismiss touch, else they break
    /// OSK and toplevel input. Cleared per-popup in `popup_destroyed`.
    pub popup_grabs: std::collections::HashSet<WlSurface>,
    /// Home-bar opacity, faded to 0 when a bottom exclusive-zone surface (the
    /// on-screen keyboard) covers it.
    pub bar_alpha: f32,
    /// Whether to draw the touch indicator overlay (`[main].show_touches`), for
    /// demo recordings. When true, input events feed `touch_viz`.
    pub show_touches: bool,
    /// Live touch-visualization state (contacts + fading release rings). Only
    /// populated while `show_touches` is set.
    pub touch_viz: touch_viz::TouchViz,

    // Shell state
    pub ui: UiState,
    pub model: ShellModel,
    pub app_catalog: HashMap<String, AppEntry>,
    pub icon_cache: HashMap<String, IconPixels>,
    pub toplevels: Vec<Option<AppToplevel>>,
    pub children: Vec<Child>,
    /// App spawned and awaiting its first toplevel — drives the pulsing launch
    /// icon. Cleared when the window maps, the process exits, or on timeout.
    pub launching: Option<Launching>,
    pub history: AppHistory,
    /// Last zoom origin (cached when launching).
    pub last_origin: ZoomOrigin,
    /// Actual output size in physical pixels (from the backend: DRM mode or
    /// winit window size). Set at construction; drives layout and app sizing.
    pub output_size: (i32, i32),
    /// The advertised output. Retained so surfaces can `enter` it (which is how
    /// clients learn the scale factor).
    pub output: Output,
    /// Output scale (`[main].dpi`), advertised via `wp_fractional_scale` so it
    /// may be fractional (e.g. 2.5). Client buffers are `logical * dpi`, so xdg
    /// configure sizes are physical/dpi.
    pub dpi: f64,
    /// Base card corner radius in logical px (`[main].card_radius`). Threaded
    /// into `compute_scene` so the switcher/drag card rounding is configurable.
    pub card_radius: f32,
    /// Prefer server-side (= no client) decorations for top-level app windows
    /// (`[main].prefer_no_csd`). Dialogs (child toplevels) always get CSD so
    /// their toolkit header bar — and its action buttons — stay present.
    pub prefer_no_csd: bool,
    /// wlr-gamma-control state (night-light / color-temperature clients).
    pub gamma: gamma_control::GammaControl,
    /// ext-idle-notify-v1 state: client idle timers, polled by both frame loops.
    pub idle_notify: idle_notify::IdleNotify,
    /// zwp_idle_inhibit_manager_v1 state: surfaces asking to hold off idle.
    pub idle_inhibit: idle_inhibit::IdleInhibit,
    /// wp_content_type_v1 state (holds the global; tags live per surface).
    #[allow(dead_code)]
    pub content_type: content_type::ContentType,
    /// ext-background-effect-v1 state (holds the global; blur regions live per
    /// surface).
    #[allow(dead_code)]
    pub background_effect: background_effect::BackgroundEffect,
    /// ext-session-lock-v1 state: whether the session is locked and the lock
    /// client's surface. See [`crate::session_lock`].
    pub session_lock: session_lock::SessionLock,
    /// Current app rotation, derived from [`Self::device_orientation`] and
    /// whether the foreground app is fullscreen. Only the app surface rotates —
    /// see [`crate::rotation`].
    pub rotation: rotation::Rotation,
    /// How the device is physically held. Fed by the accelerometer (and by the
    /// `orientation` control-socket verb, which is how the tests drive it);
    /// `Normal` until something says otherwise, so a device with no sensor
    /// behaves exactly as if it were held upright.
    pub device_orientation: rotation::DeviceOrientation,
    /// iio-sensor-proxy client. `None` on anything without an accelerometer (a
    /// dev box, the VM), where orientation only ever arrives over the control
    /// socket. See [`crate::sensor`].
    pub sensor: Option<sensor::Sensor>,
    /// Whether the foreground app is fullscreen content that wants a landscape
    /// display (see [`content_type::wants_landscape`]). Nothing rotates yet —
    /// this is the signal the rotation work will read.
    pub landscape_hint: bool,

    // Rendering
    pub skia: SkiaGl,
    pub wayland_socket: String,

    // Input
    pub last_pointer_pos: Option<(f32, f32)>,
    pub pointer_down: bool,
    /// Page drag tracking: start_x when dragging on home screen.
    pub page_drag_start: Option<f32>,
    /// Bar drag tracking from Home state: (start_x, start_y).
    pub bar_drag_start: Option<(f32, f32)>,
    /// App icon held on Home, pending tap-to-launch (also drives the press
    /// highlight). Cleared if the finger moves into a page swipe.
    pub pending_launch: Option<input_common::PendingLaunch>,
    /// Finger held on an icon, waiting to see if it becomes a long-press
    /// (arrange mode) or a tap/swipe. Cleared once arrange mode engages, the
    /// gesture becomes a swipe, or the finger releases.
    pub icon_press: Option<crate::arrange::IconPress>,
    /// Arrange-mode state (icon reorder/pin/unpin/hide). `None` outside
    /// arrange mode.
    pub arrange: Option<ArrangeState>,
    /// Armed pull-down: `(start_x, start_y)` of an empty-space Home press that
    /// may become a downward drag launching the search app. Cleared once resolved.
    pub search_arm: Option<(f32, f32)>,
    /// Set when the search app was just spawned; the next toplevel to map is
    /// treated as the search app (slide-up open, no switcher/frecency). winit
    /// sets the xdg `app_id` after `new_toplevel` fires, so the client id is not
    /// yet readable at registration — the spawn intent is the reliable signal.
    pub expecting_search: bool,
    /// Switcher deck drag state.
    pub switcher_drag: input_common::SwitcherDrag,
    /// Switcher card rects for hit-testing during drag.
    pub switcher_cards: Vec<switcher::CardRect>,
    /// In-flight synthetic swipe from the debug socket (dev harness).
    pub active_gesture: Option<debug_input::ActiveGesture>,
    /// In-flight synthetic key hold from the debug socket (dev harness).
    pub active_key: Option<debug_input::ActiveKey>,
    pub active_touch: Option<debug_input::ActiveTouch>,
    /// Pending debug `settle`: reply channel + deadline.
    pub pending_settle: Option<(std::sync::mpsc::SyncSender<String>, std::time::Instant)>,
    /// Last logged UI state: variant plus which toplevel is in front (to avoid
    /// spam without hiding real changes). The variant alone is not enough —
    /// swapping the foreground app leaves it in `App` either way, so a
    /// dismissed dialog handing the screen back, or a quick-switch between two
    /// apps, would log nothing at all.
    pub last_log_state: Option<(std::mem::Discriminant<UiState>, Option<ToplevelId>)>,
    /// Per-app grid-reflow springs (x, y), keyed by app id. Drives icons
    /// sliding to their new slot when the grid order changes (launch reorder,
    /// arrange-mode edits). Seeded lazily on first `advance_frame` and kept in
    /// sync with `model.pages` by `reflow_grid`.
    pub grid_anim: HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,
    /// Per-app dock-reflow springs (x, y), keyed by app id. Mirror of
    /// `grid_anim` for the dock row; seeded lazily and kept in sync by
    /// `reflow_dock`.
    pub dock_anim: HashMap<String, (sc_anim::Spring, sc_anim::Spring)>,

    // Timing
    pub start_time: std::time::Instant,

    // Perf instrumentation
    pub stats: frame_stats::FrameStats,
    pub perf_log: bool,
    pub last_perf_log: std::time::Instant,

    // Control
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

        // `config.toml`, read exactly once. Both the `[main]` settings below and
        // the `[keybinds]` table come from this same parse — reading it per
        // setting meant six filesystem reads and six TOML parses at startup, and
        // no guarantee they all saw the same file.
        let config = sc_config::load();
        let dpi = config.dpi.max(1.0);
        let idle_blank_secs = config.idle_blank_secs;
        let card_radius = config.card_radius;
        let show_touches = config.show_touches;
        let prefer_no_csd = config.prefer_no_csd;

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
        // xdg-dialog: lets toolkits flag a toplevel as a dialog/modal. We use the
        // hint (alongside set_parent) to keep client-side decorations — and thus
        // action buttons — on dialogs. Portal file choosers run in their own
        // process with no in-process parent, so the hint is the only signal that
        // identifies them as dialogs.
        let xdg_dialog_state = XdgDialogState::new::<Self>(&dh);
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
        // ignores). See `FractionalScaleHandler`.
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

        // wlr-screencopy: the older capture protocol, alongside the ext one.
        // wf-recorder / wlrobs / xdg-desktop-portal-wlr speak only this.
        crate::wlr_screencopy::init(&dh);

        // ext-idle-notify: idle daemons (swayidle et al) ask to be told when the
        // user has been inactive for N ms. Timeouts are polled per frame, not by
        // calloop timers — see `idle_notify`.
        let idle_notify = idle_notify::IdleNotify::new(&dh, std::time::Instant::now());
        // zwp_idle_inhibit: a visible surface can hold off idle entirely (video
        // playback keeping the screen on).
        let idle_inhibit = idle_inhibit::IdleInhibit::new(&dh);
        // wp_content_type: clients tag a surface photo/video/game. Used as the
        // auto-landscape hint.
        let content_type = content_type::ContentType::new(&dh);
        // ext-background-effect: panels/OSKs can ask for their backdrop to be
        // blurred. Advertised because `render` really blurs it.
        let background_effect = background_effect::BackgroundEffect::new(&dh);
        // ext-session-lock: an external lock client (dms) blanks the session and
        // draws its own lock screen over it. See `session_lock`.
        let session_lock = session_lock::SessionLock::new(&dh);

        // Load shell model + app catalog.
        let model = persist::load(&persist::state_path()).unwrap_or_default();
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

        // Pre-resolve icons. The search path is built once, not per icon.
        let icon_dirs = sc_icons::theme_dirs(&sc_catalog::xdg_data_dirs());
        let mut icon_cache = HashMap::new();
        for (id, entry) in &app_catalog {
            icon_cache.insert(
                id.clone(),
                sc_icons::resolve_with_dirs(&entry.icon, &icon_dirs),
            );
        }

        let page_count = model.pages.len().max(1);
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
            data_device_state,
            data_control_state,
            seat_state,
            seat,
            keyboard,
            focused_surface: None,
            keys: keybinds::Keys::from_config(config),
            blank: blank::Blank::new(),
            idle: blank::Idle::new(idle_blank_secs, std::time::Instant::now()),
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
            capture_sessions: Vec::new(),
            wlr_captures: Vec::new(),
            layers: layer_shell::LayerShell::new(output.clone(), out_w as f32, out_h as f32),
            touch,
            touch_targets: HashMap::new(),
            gesture_slot: None,
            pointer_grab: false,
            popup_grabs: std::collections::HashSet::new(),
            bar_alpha: 1.0,
            show_touches,
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
            card_radius,
            prefer_no_csd,
            gamma,
            idle_notify,
            idle_inhibit,
            content_type,
            background_effect,
            session_lock,
            rotation: rotation::Rotation::None,
            device_orientation: rotation::DeviceOrientation::Normal,
            sensor: sensor::spawn(),
            landscape_hint: false,
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
            grid_anim: HashMap::new(),
            dock_anim: HashMap::new(),
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

    /// Drop every in-flight touch/pointer gesture without acting on it.
    ///
    /// Used when something takes the screen away mid-gesture (the session lock).
    /// The finger that was down is gone as far as the shell is concerned: no
    /// launch fires on release, no page drag resumes, and the next press starts
    /// a fresh sequence. Client-routed slots are dropped too, so their `up`
    /// never re-enters the funnel.
    pub(crate) fn cancel_gestures(&mut self) {
        self.pointer_down = false;
        self.pointer_grab = false;
        self.gesture_slot = None;
        self.touch_targets.clear();
        self.page_drag_start = None;
        self.bar_drag_start = None;
        self.pending_launch = None;
        self.icon_press = None;
        self.search_arm = None;
        self.switcher_drag = input_common::SwitcherDrag::None;
        if let Some(arrange) = self.arrange.as_mut() {
            arrange.drag = None;
        }
    }

    /// Re-read `config.toml` and re-apply it, for `springchick ipc reload`.
    ///
    /// `dpi` is deliberately not re-applied: it is baked into the output's
    /// advertised fractional scale and into every buffer size clients have
    /// already committed against. `prefer_no_csd` takes effect on the next
    /// window that negotiates a decoration mode.
    pub(crate) fn reload_config(&mut self) {
        let config = sc_config::load();
        self.card_radius = config.card_radius;
        self.prefer_no_csd = config.prefer_no_csd;
        self.show_touches = config.show_touches;
        self.idle = blank::Idle::new(config.idle_blank_secs, std::time::Instant::now());
        // Same path as startup; `children` (spawned binding commands, still to
        // be reaped) stays on the existing `Keys`.
        self.keys.tracker = keybinds::Keys::from_config(config).tracker;
    }

    /// Output size as floats — shorthand for the `(w, h)` pair every geometry
    /// call needs.
    pub(crate) fn output_size_f(&self) -> (f32, f32) {
        (self.output_size.0 as f32, self.output_size.1 as f32)
    }

    /// Current Home page, or `0` when not on the home screen (app/switcher/etc).
    pub(crate) fn current_home_page(&self) -> usize {
        if let UiState::Home { page, .. } = &self.ui {
            *page
        } else {
            0
        }
    }

    /// Record one frame's duration and emit a perf summary at most once per
    /// second. Shared by the winit and DRM render loops.
    pub(crate) fn record_and_log_frame(&mut self, frame_start: std::time::Instant) {
        self.stats.record_frame(frame_start.elapsed());
        if self.perf_log && self.last_perf_log.elapsed() >= Duration::from_secs(1) {
            debug!(target: "springchick::perf", "{}", self.stats.format_line());
            self.last_perf_log = std::time::Instant::now();
        }
    }
}
