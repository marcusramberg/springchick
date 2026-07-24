//! DRM/KMS device backend (M4).
//!
//! Runs the compositor on real hardware via libseat/logind + udev/DRM/GBM +
//! libinput, on its own calloop event loop, frame-paced by page-flip. The
//! render itself is the shared [`crate::render::draw_scene`] — this module only
//! provides the bind/submit primitives (Approach A). See
//! `docs/superpowers/specs/2026-06-27-springchick-m4-device-backend-perf.md`.

use std::time::{Duration, Instant};

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, GbmBufferedSurface};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::{
    AbsolutePositionEvent, Event as InputEventTrait, InputEvent, KeyboardKeyEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Bind, ImportDma};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev;
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::drm::control::{
    connector, crtc, property, Device as ControlDevice, ModeTypeFlags,
};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{DeviceFd, Rectangle, Size, Transform};

use tracing::{error, info, warn};

use crate::{accept_client, create_display, State};

/// Per-frame user data threaded through the GBM swapchain page-flip.
type FlipData = ();

struct Drm {
    _session: LibSeatSession,
    _device: DrmDevice,
    gbm_surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, FlipData>,
    renderer: GlesRenderer,
    output_size: Size<i32, smithay::utils::Physical>,
    transform: Transform,
    /// Set false while a VT-switch has us deactivated.
    active: bool,
    /// True while a page-flip is in flight (waiting on vblank).
    pending_flip: bool,
    /// DRM node fd, kept for DPMS toggling (blanking).
    device_fd: DrmDeviceFd,
    /// The connector we scan out to, and its `DPMS` property handle if the
    /// driver exposes one. `None` means blanking falls back to freezing.
    connector: connector::Handle,
    dpms_prop: Option<property::Handle>,
}

/// DPMS levels, per `drm_mode.h`. Off (3) disables the pipe and powers the
/// panel down; On (0) restores it.
const DPMS_ON: property::RawValue = 0;
const DPMS_OFF: property::RawValue = 3;

impl Drm {
    /// Drive the connector's DPMS property. No-op if the driver exposes none.
    fn set_dpms(&self, on: bool) {
        let Some(prop) = self.dpms_prop else {
            return;
        };
        let value = if on { DPMS_ON } else { DPMS_OFF };
        if let Err(e) = self.device_fd.set_property(self.connector, prop, value) {
            warn!("set DPMS {}: {e}", if on { "on" } else { "off" });
        }
    }
}

/// Find the `DPMS` property handle on a connector, if the driver exposes it.
fn find_dpms_prop(device: &DrmDeviceFd, connector: connector::Handle) -> Option<property::Handle> {
    let props = device.get_properties(connector).ok()?;
    let handles: Vec<property::Handle> = props.as_props_and_values().0.to_vec();
    handles.into_iter().find(|handle| {
        device
            .get_property(*handle)
            .is_ok_and(|info| info.name().to_str() == Ok("DPMS"))
    })
}

/// Entry point for the DRM backend. Selected by `SPRINGCHICK_BACKEND=drm`.
pub fn run_drm() {
    if let Err(e) = run() {
        error!("DRM backend error: {e}");
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;

    // --- Session (DRM master + input perms from logind on the active VT) ---
    let (mut session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    info!(seat = %seat_name, "libseat session acquired");

    // --- Pick the primary GPU ---
    let gpu_path = udev::primary_gpu(&seat_name)?.ok_or("no primary GPU found")?;
    info!(path = ?gpu_path, "primary GPU");

    // --- Open the DRM node through the session ---
    let fd = session.open(&gpu_path, OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // --- DRM device + GBM + EGL + GLES renderer ---
    let (mut drm_device, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
    let gbm = GbmDevice::new(device_fd.clone())?;
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let renderer = unsafe { GlesRenderer::new(egl_context)? };

    // --- Find a connected connector + crtc + preferred mode ---
    let (connector_handle, crtc_handle, mode) = find_output(&drm_device)?;
    let (mw, mh) = mode.size();
    let output_size: Size<i32, smithay::utils::Physical> = (mw as i32, mh as i32).into();
    info!(w = mw, h = mh, "selected mode");

    // --- Scanout surface (GBM double-buffered, page-flip on vblank) ---
    let drm_surface = drm_device.create_surface(crtc_handle, mode, &[connector_handle])?;
    let allocator = GbmAllocator::new(
        gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let render_formats = renderer.dmabuf_formats();
    let gbm_surface = GbmBufferedSurface::new(
        drm_surface,
        allocator,
        &[Fourcc::Argb8888, Fourcc::Xrgb8888],
        render_formats,
    )?;

    // --- Wayland display + shell State ---
    let (display, listener, socket_name) = create_display()?;
    // Session backend: publish to systemd/dbus so user services (e.g. wvkbd)
    // can reach our socket.
    crate::publish_wayland_display(&socket_name, true);
    let mut state = State::new(&display, socket_name);
    state.output_size = (output_size.w, output_size.h);
    state.perf_log = true; // perf logging is the point of this backend

    // Look up the connector's DPMS property so power-short can truly blank the
    // panel (disabling scanout) rather than freezing the last frame.
    let dpms_prop = find_dpms_prop(&device_fd, connector_handle);
    if dpms_prop.is_none() {
        warn!("connector exposes no DPMS property; blanking will freeze, not power off");
    }

    let drm = Drm {
        _session: session,
        _device: drm_device,
        gbm_surface,
        renderer,
        output_size,
        // App-window output transform. The DRM/GBM scanout buffer is itself
        // vertically flipped vs winit's framebuffer (hence Skia needs flip_y),
        // so the wayland surface composites correct with Normal here while the
        // Skia home/bar gets flip_y. Confirmed on-device 2026-06-27.
        transform: Transform::Normal,
        active: true,
        pending_flip: false,
        device_fd: device_fd.clone(),
        connector: connector_handle,
        dpms_prop,
    };

    let mut app = App {
        state,
        drm,
        display,
        listener,
        last_frame: Instant::now(),
    };

    // --- calloop sources ---

    // 1. DRM page-flip events.
    event_loop
        .handle()
        .insert_source(drm_notifier, |event, _meta, app| match event {
            DrmEvent::VBlank(_crtc) => {
                if let Err(e) = app.drm.gbm_surface.frame_submitted() {
                    warn!("frame_submitted error: {e}");
                }
                app.drm.pending_flip = false;
                app.render();
            }
            DrmEvent::Error(err) => warn!("DRM error: {err}"),
        })
        .map_err(|e| format!("insert drm source: {e}"))?;

    // 2. libinput touch + keyboard.
    let mut libinput =
        Libinput::new_with_udev(LibinputSessionInterface::from(app.drm._session.clone()));
    libinput
        .udev_assign_seat(&seat_name)
        .map_err(|_| "libinput assign seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput);
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, _, app| {
            app.handle_input(event);
        })
        .map_err(|e| format!("insert libinput source: {e}"))?;

    // 3. Session activate/deactivate (VT-switch).
    event_loop
        .handle()
        .insert_source(session_notifier, |event, _, app| match event {
            SessionEvent::PauseSession => {
                info!("session paused (VT switched away)");
                app.drm.active = false;
            }
            SessionEvent::ActivateSession => {
                info!("session activated (VT switched back)");
                app.drm.active = true;
                app.drm.gbm_surface.reset_buffers();
                app.drm.pending_flip = false;
                app.render();
            }
        })
        .map_err(|e| format!("insert session source: {e}"))?;

    info!("entering DRM frame loop");
    // Kick off the first frame.
    app.render();

    // The 2ms timeout wakes the loop to accept + dispatch wayland clients even
    // when no DRM/input event fires.
    event_loop.run(Some(Duration::from_millis(2)), &mut app, |app| {
        app.dispatch_wayland();
        // Long presses are polled here, not in `render`: page-flips stop when
        // nothing animates, so a frame-driven poll would never fire on an idle
        // screen.
        crate::keybinds::poll(&mut app.state);
        app.state.sync_keyboard_focus();
        app.apply_blanking();
        // The OSD fades over time with no other event driving frames; keep
        // rendering while it is visible. `render` early-returns on pending_flip,
        // so this tracks vblank cadence rather than the 2ms wake.
        if app.state.osd.is_active(Instant::now()) {
            app.render();
        }
    })?;

    Ok(())
}

/// Aggregate owned by the calloop loop.
struct App {
    state: State,
    drm: Drm,
    display: Display<State>,
    listener: ListeningSocket,
    last_frame: Instant,
}

impl App {
    fn dispatch_wayland(&mut self) {
        accept_client(&self.display, &self.listener);
        self.display.dispatch_clients(&mut self.state).ok();
        self.display.flush_clients().ok();
    }

    fn handle_input(&mut self, event: InputEvent<LibinputInputBackend>) {
        let (w, h) = (self.drm.output_size.w, self.drm.output_size.h);
        match event {
            InputEvent::TouchDown { event } => {
                use smithay::backend::input::{Event as _, TouchEvent as _};
                let x = event.x_transformed(w) as f32;
                let y = event.y_transformed(h) as f32;
                let slot = event.slot();
                crate::touch::down(&mut self.state, x, y, slot, event.time_msec());
            }
            InputEvent::TouchMotion { event } => {
                use smithay::backend::input::{Event as _, TouchEvent as _};
                let x = event.x_transformed(w) as f32;
                let y = event.y_transformed(h) as f32;
                let slot = event.slot();
                crate::touch::motion(&mut self.state, x, y, slot, event.time_msec());
            }
            InputEvent::TouchUp { event } => {
                use smithay::backend::input::{Event as _, TouchEvent as _};
                let slot = event.slot();
                crate::touch::up(&mut self.state, slot, event.time_msec());
            }
            InputEvent::Keyboard { event } => {
                crate::keybinds::on_key_event(
                    &mut self.state,
                    event.key_code(),
                    event.state(),
                    event.time_msec(),
                );
            }
            _ => {}
        }
    }

    /// Act on a blank/unblank request. Blanking drives the connector's DPMS
    /// property to Off, which disables scanout and powers the panel down;
    /// unblanking sets it back On and forces a redraw. If the driver has no DPMS
    /// property we can only stop flipping (the last frame freezes).
    fn apply_blanking(&mut self) {
        let Some(blanked) = self.state.blank.take_change() else {
            return;
        };
        if blanked {
            info!("blanking panel");
            self.drm.pending_flip = false;
            self.drm.set_dpms(false);
        } else {
            info!("unblanking panel");
            self.drm.set_dpms(true);
            self.drm.gbm_surface.reset_buffers();
            self.drm.pending_flip = false;
            self.render();
        }
    }

    /// Render one frame to the scanout buffer and queue a page-flip.
    fn render(&mut self) {
        if !self.drm.active || self.drm.pending_flip || self.state.blank.is_blanked() {
            return;
        }
        let frame_start = Instant::now();

        // Tick springs.
        let dt = self.last_frame.elapsed().as_secs_f32().min(1.0 / 30.0);
        self.last_frame = Instant::now();
        let effect =
            crate::ui_state::transition(&mut self.state.ui, crate::ui_state::UiEvent::Tick { dt });
        match effect {
            crate::ui_state::Effect::CloseToplevel { toplevel } => {
                self.state.close_toplevel(toplevel);
            }
            crate::ui_state::Effect::EnterSwitcher => {
                let cards = self.state.history.mru_list();
                crate::ui_state::transition(
                    &mut self.state.ui,
                    crate::ui_state::UiEvent::EnterSwitcher { cards },
                );
            }
            _ => {}
        }
        if let crate::ui_state::UiState::Home { page_count, .. } = &mut self.state.ui {
            *page_count = self.state.model.pages.len().max(1);
        }

        let scene = crate::scene::compute_scene(&self.state.ui, self.state.output_size);
        self.state.switcher_cards = scene.cards.clone();
        let app_surface = scene.window.as_ref().and_then(|(tid, _)| {
            self.state
                .toplevels
                .get(*tid)
                .and_then(|slot| slot.as_ref())
                .map(|tl| tl.surface.wl_surface().clone())
        });
        let frame_time = self.state.start_time.elapsed().as_millis() as u32;
        let osd_now = Instant::now();
        let osd_view = self.state.osd.is_active(osd_now).then(|| {
            (
                self.state.osd.level,
                self.state.osd.muted,
                self.state.osd.alpha(osd_now),
            )
        });
        let size = self.drm.output_size;
        let damage = Rectangle::from_size(size);

        // Acquire the next scanout buffer and bind it as the framebuffer.
        let (mut dmabuf, _age) = match self.drm.gbm_surface.next_buffer() {
            Ok(b) => b,
            Err(e) => {
                warn!("next_buffer failed: {e}");
                return;
            }
        };
        let mut framebuffer = match self.drm.renderer.bind(&mut dmabuf) {
            Ok(fb) => fb,
            Err(e) => {
                warn!("renderer.bind failed: {e}");
                return;
            }
        };

        let (layers_below, layers_above) = self.state.layers.render_lists();
        {
            let mut ctx = crate::render::DrawCtx {
                scene: &scene,
                app_surface: app_surface.as_ref(),
                skia: &mut self.state.skia,
                model: &self.state.model,
                icon_cache: &self.state.icon_cache,
                app_catalog: &self.state.app_catalog,
                toplevels: &self.state.toplevels,
                transform: self.drm.transform,
                skia_flip_y: true,
                frame_time,
                osd: osd_view,
                layers_below: &layers_below,
                layers_above: &layers_above,
            };
            if let Err(e) =
                crate::render::draw_scene(&mut self.drm.renderer, &mut framebuffer, size, &mut ctx)
            {
                warn!("draw_scene failed: {e}");
                return;
            }
        }
        drop(framebuffer);

        // Fence the frame: block until smithay + Skia GL work has completed so
        // the page-flip never scans out a half-rendered buffer (tearing). We
        // have ~6ms of slack under the 11ms 90Hz budget, so the stall is free.
        self.state.skia.finish_gpu();

        // Queue the page-flip; vblank fires the next render.
        match self
            .drm
            .gbm_surface
            .queue_buffer(None, Some(vec![damage]), ())
        {
            Ok(()) => self.drm.pending_flip = true,
            Err(e) => warn!("queue_buffer failed: {e}"),
        }

        self.state.stats.record_frame(frame_start.elapsed());
        if self.state.perf_log && self.state.last_perf_log.elapsed() >= Duration::from_secs(1) {
            info!(target: "springchick::perf", "{}", self.state.stats.format_line());
            self.state.last_perf_log = Instant::now();
        }
    }
}

/// Find the first connected connector, a usable crtc, and its preferred mode.
fn find_output(
    drm: &DrmDevice,
) -> Result<
    (
        connector::Handle,
        crtc::Handle,
        smithay::reexports::drm::control::Mode,
    ),
    Box<dyn std::error::Error>,
> {
    let res = drm.resource_handles()?;

    for &conn_handle in res.connectors() {
        let conn = drm.get_connector(conn_handle, false)?;
        if conn.state() != connector::State::Connected {
            continue;
        }
        if conn.modes().is_empty() {
            continue;
        }
        // Preferred mode, else the first.
        let mode = conn
            .modes()
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .copied()
            .unwrap_or_else(|| conn.modes()[0]);

        // Find a crtc reachable from one of the connector's encoders.
        for &enc_handle in conn.encoders() {
            let enc = drm.get_encoder(enc_handle)?;
            if let Some(crtc_handle) = res.filter_crtcs(enc.possible_crtcs()).into_iter().next() {
                return Ok((conn_handle, crtc_handle, mode));
            }
        }
    }
    Err("no connected connector with a usable crtc".into())
}
