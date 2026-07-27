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
use smithay::utils::{DeviceFd, Size, Transform};

use tracing::{debug, error, info, warn};

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
    /// The CRTC driving the connector, for gamma-LUT programming.
    crtc: crtc::Handle,
    /// The CRTC gamma table captured at startup, restored when a gamma-control
    /// client releases the output. `None` if the CRTC has no gamma LUT.
    orig_gamma: Option<[Vec<u16>; 3]>,
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
    let mut state = State::new(&display, socket_name, (output_size.w, output_size.h));
    state.perf_log = true; // perf logging is the point of this backend
    // Advertise zwp_linux_dmabuf so GL clients (GTK4, etc.) share buffers
    // zero-copy instead of falling back to slow shm software upload.
    state.init_dmabuf_global(&display.handle(), renderer.dmabuf_formats());

    // Look up the connector's DPMS property so power-short can truly blank the
    // panel (disabling scanout) rather than freezing the last frame.
    let dpms_prop = find_dpms_prop(&device_fd, connector_handle);
    if dpms_prop.is_none() {
        warn!("connector exposes no DPMS property; blanking will freeze, not power off");
    }

    // wlr-gamma-control: advertise the real CRTC LUT size and snapshot the
    // current ramp so we can restore it when a client releases control.
    let gamma_size = device_fd
        .get_crtc(crtc_handle)
        .map(|info| info.gamma_length())
        .unwrap_or(0);
    let orig_gamma = capture_gamma(&device_fd, crtc_handle, gamma_size);
    if gamma_size > 0 {
        state.gamma.size = gamma_size;
    } else {
        warn!("CRTC exposes no gamma LUT; gamma-control uploads will be ignored");
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
        crtc: crtc_handle,
        orig_gamma,
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

    // Tell systemd we're up: DRM output live, first frame on screen, wayland
    // socket accepting. springchick.service is Type=notify and
    // BindsTo+Before graphical-session.target, so this READY is what pulls
    // that target active (it has RefuseManualStart=yes and cannot be started
    // by hand). Downstream user services that gate on an active graphical
    // session — xdg-desktop-portal-*, the OSK — only come up after this.
    // No-ops when NOTIFY_SOCKET is unset (bare VT launch), so it is harmless
    // outside the service.
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        warn!(%e, "sd_notify READY failed");
    }

    // The 2ms timeout wakes the loop to accept + dispatch wayland clients even
    // when no DRM/input event fires.
    event_loop.run(Some(Duration::from_millis(2)), &mut app, |app| {
        app.dispatch_wayland();
        // Long presses are polled here, not in `render`: page-flips stop when
        // nothing animates, so a frame-driven poll would never fire on an idle
        // screen.
        crate::keybinds::poll(&mut app.state);
        app.state.sync_keyboard_focus();
        // Idle-blank once the timeout elapses. Reuses the power-button DPMS-off
        // path; a power-button press wakes it and resets the countdown (input
        // routes through handle_input → idle.activity).
        if app.state.idle.should_blank(Instant::now()) && !app.state.blank.is_blanked() {
            app.state.blank.toggle();
        }
        app.apply_blanking();
        app.apply_gamma();
        // The OSD fades over time with no other event driving frames; keep
        // rendering while it is visible. `render` early-returns on pending_flip,
        // so this tracks vblank cadence rather than the 2ms wake.
        if app.state.needs_render
            || app.state.osd.is_active(Instant::now())
            || app.state.bar_fading()
        {
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
        // Any input may change on-screen state (or the app it's forwarded to
        // will commit in response). Prime a render for the next loop wake.
        self.state.needs_render = true;
        // Any input is activity: restart the idle-blank countdown.
        self.state.idle.activity(Instant::now());
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

    /// Program any pending gamma-control update into the CRTC LUT. A `Set`
    /// applies the client's ramps; a `Reset` (control released) restores the
    /// ramp captured at startup.
    fn apply_gamma(&mut self) {
        let Some(update) = self.state.gamma.take_pending() else {
            return;
        };
        let crtc = self.drm.crtc;
        let res = match &update {
            crate::gamma_control::GammaUpdate::Set([r, g, b]) => {
                self.drm.device_fd.set_gamma(crtc, r, g, b)
            }
            crate::gamma_control::GammaUpdate::Reset => match &self.drm.orig_gamma {
                Some([r, g, b]) => self.drm.device_fd.set_gamma(crtc, r, g, b),
                None => Ok(()),
            },
        };
        if let Err(e) = res {
            warn!("set_gamma failed: {e}");
        }
    }

    /// Render one frame to the scanout buffer and queue a page-flip.
    fn render(&mut self) {
        if !self.drm.active || self.drm.pending_flip || self.state.blank.is_blanked() {
            return;
        }
        // Consuming the request now: this frame reflects current state. A commit
        // arriving after this point re-sets the flag and gets its own render.
        self.state.needs_render = false;
        let frame_start = Instant::now();

        // Variable step, clamped so a long stall can't fling the springs.
        let dt = self.last_frame.elapsed().as_secs_f32().min(1.0 / 30.0);
        self.last_frame = Instant::now();
        let prep = self.state.advance_frame(dt);

        let size = self.drm.output_size;

        // Partial page-flip damage is only safe when nothing but the app surface
        // could have changed: fullscreen app, no switcher/home/OSD/OSK, and the
        // bar not mid-fade. Any of these repaint via Skia (untracked) and would
        // leave stale pixels if excluded from the damage hint.
        let report_partial = prep.scene.window_covers_screen()
            && prep.app_surface.is_some()
            && prep.scene.cards.is_empty()
            && !prep.scene.show_home
            && prep.osd_view.is_none()
            && !self.state.bar_fading()
            && prep.layers_below.is_empty()
            && prep.layers_above.is_empty();

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

        // Screen-space animated grid centers: global spring position minus the
        // current page scroll, so the grid pass in draw_home can render
        // sliding icons without knowing about pages itself.
        let page_scroll = if let crate::ui_state::UiState::Home { page_spring, .. } = &self.state.ui
        {
            page_spring.value
        } else {
            0.0
        };
        let gp_w = self.state.output_size.0 as f32;
        let grid_positions: std::collections::HashMap<String, (f32, f32)> = self
            .state
            .grid_anim
            .iter()
            .map(|(app, (sx, sy))| (app.clone(), (sx.value - page_scroll * gp_w, sy.value)))
            .collect();
        let dock_positions: std::collections::HashMap<String, (f32, f32)> = self
            .state
            .dock_anim
            .iter()
            .map(|(a, (sx, sy))| (a.clone(), (sx.value, sy.value)))
            .collect();

        let flip_damage = {
            let mut ctx = crate::render::DrawCtx {
                scene: &prep.scene,
                app_surface: prep.app_surface.as_ref(),
                skia: &mut self.state.skia,
                model: &self.state.model,
                icon_cache: &self.state.icon_cache,
                app_catalog: &self.state.app_catalog,
                toplevels: &self.state.toplevels,
                app_scale: self.state.dpi as f64,
                app_origin: {
                    let u = self.state.layers.usable(self.state.dpi);
                    (u.x.round() as i32, u.y.round() as i32)
                },
                transform: self.drm.transform,
                skia_flip_y: true,
                frame_time: prep.frame_time,
                osd: prep.osd_view,
                layers_below: &prep.layers_below,
                layers_above: &prep.layers_above,
                app_popups: &prep.app_popups,
                layer_popups: &prep.layer_popups,
                bar_alpha: prep.bar_alpha,
                pressed_app: self
                    .state
                    .pending_launch
                    .as_ref()
                    .map(|p| p.app_id.as_str()),
                arrange: self.state.arrange.as_ref().map(|a| {
                    let drag_app = a.drag.as_ref().map(|d| d.app_id.as_str());
                    let drag_pos = a.drag.as_ref().map(|d| d.cur);
                    let over_dock = a
                        .drag
                        .as_ref()
                        .map(|d| {
                            let (w, h) = (
                                self.state.output_size.0 as f32,
                                self.state.output_size.1 as f32,
                            );
                            let page = if let crate::ui_state::UiState::Home { page, .. } =
                                &self.state.ui
                            {
                                *page
                            } else {
                                0
                            };
                            let layout = sc_layout::compute(w, h, page, &self.state.model);
                            layout.dock_zone.contains(d.cur.0, d.cur.1)
                        })
                        .unwrap_or(false);
                    crate::render::ArrangeView {
                        drag_app,
                        drag_pos,
                        over_dock,
                    }
                }),
                report_partial_damage: report_partial,
                last_present: &mut self.state.last_present,
                grid_positions: &grid_positions,
                dock_positions: &dock_positions,
            };
            match crate::render::draw_scene(
                &mut self.drm.renderer,
                &mut framebuffer,
                size,
                &mut ctx,
            ) {
                Ok(damage) => damage,
                Err(e) => {
                    warn!("draw_scene failed: {e}");
                    return;
                }
            }
        };
        drop(framebuffer);

        // Fence the frame: block until smithay + Skia GL work has completed so
        // the page-flip never scans out a half-rendered buffer (tearing). We
        // have ~6ms of slack under the 11ms 90Hz budget, so the stall is free.
        self.state.skia.finish_gpu();

        // Queue the page-flip; vblank fires the next render.
        match self
            .drm
            .gbm_surface
            .queue_buffer(None, Some(flip_damage), ())
        {
            Ok(()) => self.drm.pending_flip = true,
            Err(e) => warn!("queue_buffer failed: {e}"),
        }

        self.state.stats.record_frame(frame_start.elapsed());
        if self.state.perf_log && self.state.last_perf_log.elapsed() >= Duration::from_secs(1) {
            debug!(target: "springchick::perf", "{}", self.state.stats.format_line());
            self.state.last_perf_log = Instant::now();
        }
    }
}

/// Snapshot the CRTC's current gamma ramp so it can be restored when a
/// gamma-control client releases the output. Returns `None` if the CRTC has no
/// LUT or the read fails.
fn capture_gamma(
    device: &DrmDeviceFd,
    crtc: crtc::Handle,
    size: u32,
) -> Option<[Vec<u16>; 3]> {
    if size == 0 {
        return None;
    }
    let n = size as usize;
    let (mut r, mut g, mut b) = (vec![0u16; n], vec![0u16; n], vec![0u16; n]);
    match device.get_gamma(crtc, &mut r, &mut g, &mut b) {
        Ok(()) => Some([r, g, b]),
        Err(e) => {
            warn!("read original gamma failed: {e}");
            None
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
