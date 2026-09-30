//! DRM/KMS backend: libseat + udev/DRM/GBM + libinput on calloop, paced by
//! page-flip. Rendering is the shared [`crate::render::draw_scene`].

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, GbmBufferedSurface,
    NodeType, VrrSupport,
};
use smithay::backend::egl::fence::EGLFence;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::{
    AbsolutePositionEvent, ButtonState, Event as InputEventTrait, InputEvent, KeyboardKeyEvent,
    PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::{Capability, GlesRenderer, GlesTexProgram};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Bind, ImportDma, Renderer, RendererSuper};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::drm::control::{
    connector, crtc, property, Device as ControlDevice, ModeTypeFlags,
};
use smithay::reexports::input::{Device, DeviceCapability, Libinput, SendEventsMode};
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{Clock, DeviceFd, Monotonic, Physical, Rectangle, Size, Transform};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::image_copy_capture::CaptureFailureReason;
use smithay::wayland::presentation::PresentationFeedbackCallback;

use tracing::{debug, error, info, warn};

use crate::{accept_client, create_display, State};

type FlipData = ();

/// The KMS damage hint and the presentation feedback owed for the frame.
type DrawnFrame = (
    Vec<Rectangle<i32, Physical>>,
    Vec<PresentationFeedbackCallback>,
);

struct Drm {
    device: DrmDevice,
    gbm_surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, FlipData>,
    renderer: GlesRenderer,
    /// Kept for hotplugged displays' swapchains.
    gbm: GbmDevice<DrmDeviceFd>,
    /// See [`crate::mirror`]. Empty on a bare phone, which must stay free of
    /// extra work.
    mirrors: Vec<crate::mirror::MirrorOutput>,
    rounded_tex_shader: GlesTexProgram,
    output_size: Size<i32, Physical>,
    transform: Transform,
    /// False while VT-switched away.
    active: bool,
    pending_flip: bool,
    /// Answered from the vblank that scans it out. Empty when no flip is pending.
    pending_presentation: Vec<PresentationFeedbackCallback>,
    refresh_mhz: i32,
    device_fd: DrmDeviceFd,
    /// `None` means blanking falls back to freezing the last frame.
    connector: connector::Handle,
    dpms_prop: Option<property::Handle>,
    crtc: crtc::Handle,
    /// Restored when a gamma client releases. `None` if the CRTC has no LUT.
    orig_gamma: Option<[Vec<u16>; 3]>,
    /// Declared last so it drops after the DrmDevice releases master; closing
    /// the seat first can wedge the handoff.
    _session: LibSeatSession,
}

/// Per `drm_mode.h`.
const DPMS_ON: property::RawValue = 0;
const DPMS_OFF: property::RawValue = 3;

impl Drm {
    fn set_connector_dpms(
        &self,
        conn: connector::Handle,
        prop: Option<property::Handle>,
        on: bool,
    ) {
        let Some(prop) = prop else { return };
        let value = if on { DPMS_ON } else { DPMS_OFF };
        if let Err(e) = self.device_fd.set_property(conn, prop, value) {
            warn!("set DPMS {}: {e}", if on { "on" } else { "off" });
        }
    }

    fn set_primary_dpms(&self, on: bool) {
        self.set_connector_dpms(self.connector, self.dpms_prop, on);
    }

    fn set_mirror_dpms(&self, on: bool) {
        for m in &self.mirrors {
            self.set_connector_dpms(m.connector, m.dpms, on);
        }
    }

    /// Blanking the phone still leaves something needing frames.
    fn mirroring(&self) -> bool {
        !self.mirrors.is_empty()
    }
}

pub fn find_dpms_prop(
    device: &DrmDeviceFd,
    connector: connector::Handle,
) -> Option<property::Handle> {
    let props = device.get_properties(connector).ok()?;
    let handles: Vec<property::Handle> = props.as_props_and_values().0.to_vec();
    handles.into_iter().find(|handle| {
        device
            .get_property(*handle)
            .is_ok_and(|info| info.name().to_str() == Ok("DPMS"))
    })
}

pub fn run_drm() {
    if let Err(e) = run() {
        error!("DRM backend error: {e}");
    }
}

/// Render nodes need no DRM master, so they can be opened without libseat.
fn open_render_node(
    path: &std::path::Path,
) -> Result<std::os::fd::OwnedFd, Box<dyn std::error::Error>> {
    use smithay::reexports::rustix::fs::{open, Mode};
    let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK;
    Ok(open(path, flags, Mode::empty())?)
}

/// A GPU that isn't the scanout device (on the FP5 the DECON has no 3D
/// engine, so EGL there is llvmpipe). `None` when the scanout device is the
/// only GPU.
fn pick_render_node(seat: &str, scanout: &DrmDeviceFd) -> Option<std::path::PathBuf> {
    let scanout_node = DrmNode::from_file(scanout).ok()?;
    for card in udev::all_gpus(seat).ok()? {
        let node = match DrmNode::from_path(&card) {
            Ok(n) => n,
            Err(_) => continue,
        };
        if node.dev_id() == scanout_node.dev_id() {
            continue;
        }
        if let Some(render) = node.dev_path_with_type(NodeType::Render) {
            return Some(render);
        }
    }
    None
}

/// Retries: at login the greeter may still hold DRM master and logind
/// answers EPERM until the handover finishes. Aborting there drops the user
/// back to the greeter.
fn open_drm_node(
    session: &mut LibSeatSession,
    path: &std::path::Path,
) -> Result<std::os::fd::OwnedFd, Box<dyn std::error::Error>> {
    const MAX_ATTEMPTS: u32 = 20;
    const BACKOFF: Duration = Duration::from_millis(150);
    let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut attempt = 1;
    loop {
        match session.open(path, flags) {
            Ok(fd) => {
                if attempt > 1 {
                    info!(attempt, "DRM node opened after retrying session handover");
                }
                return Ok(fd);
            }
            Err(e) if attempt < MAX_ATTEMPTS => {
                warn!(attempt, error = %e, "DRM open failed; retrying (session handover in progress?)");
                std::thread::sleep(BACKOFF);
                attempt += 1;
            }
            Err(e) => return Err(format!("open DRM node after {attempt} attempts: {e}").into()),
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;

    let (mut session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    info!(seat = %seat_name, "libseat session acquired");

    let gpu_path = udev::primary_gpu(&seat_name)?.ok_or("no primary GPU found")?;
    info!(path = ?gpu_path, "scanout GPU");

    let fd = open_drm_node(&mut session, &gpu_path)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    let (mut drm_device, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
    let gbm = GbmDevice::new(device_fd.clone())?;

    let render_gbm = match pick_render_node(&seat_name, &device_fd) {
        Some(path) => {
            info!(?path, "render GPU");
            let rfd = open_render_node(&path)?;
            GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(rfd)))?
        }
        None => {
            info!("no separate render node; rendering on the scanout GPU");
            gbm.clone()
        }
    };
    let render_node = DrmNode::from_file(&render_gbm).ok();
    let egl_display = unsafe { EGLDisplay::new(render_gbm)? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
    let rounded_tex_shader = crate::render::compile_rounded_tex_shader(&mut renderer)?;

    let (connector_handle, crtc_handle, mode) = find_output(&drm_device)?;
    let (mw, mh) = mode.size();
    let output_size: Size<i32, Physical> = (mw as i32, mh as i32).into();
    info!(w = mw, h = mh, "selected mode");

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

    let (display, listener, socket_name) = create_display()?;
    // Publish to systemd/dbus so user services (wvkbd) find our socket.
    crate::publish_wayland_display(&socket_name, true);
    let mut state = State::new(&display, socket_name, (output_size.w, output_size.h));
    state.perf_log = true;
    // Nothing else draws a cursor on bare KMS.
    state.cursor_overlay = true;

    // With render-on-demand the panel would keep scanning out a frozen frame at
    // the mode's rate; VRR lets it stretch the vblank.
    match gbm_surface.vrr_supported(connector_handle) {
        Ok(VrrSupport::NotSupported) => info!("connector does not support VRR"),
        Ok(support) => {
            info!(?support, want = state.vrr, "connector supports VRR");
            if state.vrr {
                // `RequiresModeset` only stages it; the modeset rides the first
                // `queue_buffer`.
                match gbm_surface.use_vrr(true) {
                    Ok(()) => info!("VRR enabled"),
                    Err(e) => warn!("enabling VRR failed: {e}"),
                }
            }
        }
        Err(e) => warn!("VRR probe failed: {e}"),
    }
    // The render node (panthor) is the main device, so clients allocate where
    // we import zero-copy. v4 feedback is required by wl-screenrec.
    let main_device = render_node.map(|n| n.dev_id());
    state.init_dmabuf_global(&display.handle(), renderer.dmabuf_formats(), main_device);

    let debug_chan = crate::debug_input::spawn_listener(state.panel_size);
    let catalog_dirty = crate::catalog_watch::spawn();

    // Recorders allocate capture buffers against this; shm-only without a node.
    if let Some(node) = render_node {
        let cap_node = node
            .node_with_type(NodeType::Render)
            .and_then(Result::ok)
            .unwrap_or(node);
        state.capture_formats = Some((cap_node, group_formats(renderer.dmabuf_formats())));
    }

    // DPMS really blanks; without it we can only freeze the last frame.
    let dpms_prop = find_dpms_prop(&device_fd, connector_handle);
    if dpms_prop.is_none() {
        warn!("connector exposes no DPMS property; blanking will freeze, not power off");
    }

    // Snapshot the ramp to restore when a client releases control.
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

    // Later hotplug is handled by the udev source.
    let mut mirrors = Vec::new();
    crate::mirror::refresh(
        &mut mirrors,
        &mut drm_device,
        &gbm,
        &renderer,
        connector_handle,
        crtc_handle,
    );

    let drm = Drm {
        _session: session,
        device: drm_device,
        gbm_surface,
        gbm,
        mirrors,
        renderer,
        rounded_tex_shader,
        output_size,
        // The GBM scanout buffer is Y-flipped vs winit's framebuffer, so apps
        // composite with Normal while Skia gets flip_y.
        transform: Transform::Normal,
        active: true,
        pending_flip: false,
        pending_presentation: Vec::new(),
        refresh_mhz: mode_refresh_mhz(&mode),
        device_fd: device_fd.clone(),
        connector: connector_handle,
        dpms_prop,
        crtc: crtc_handle,
        orig_gamma,
    };
    // A blanked phone panel with a mirror isn't asleep; the key path needs this.
    state.external_display = drm.mirroring();

    // Dup before `display`/`listener` move into `app`, so client traffic can
    // wake the loop instead of being polled.
    let display_fd = display
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup wayland display fd: {e}"))?;
    let listener_fd = listener
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| format!("dup wayland listener fd: {e}"))?;

    let mut app = App {
        state,
        drm,
        display,
        listener,
        last_frame: Instant::now(),
        clock: Clock::new(),
        touch_devices: Vec::new(),
    };

    // calloop sources

    // 1. DRM page-flip events.
    event_loop
        .handle()
        .insert_source(drm_notifier, |event, meta, app| match event {
            DrmEvent::VBlank(crtc) => {
                // A mirror's vblank only releases its own buffer; mirrors follow the
                // primary.
                if crtc != app.drm.crtc {
                    if let Some(m) = app.drm.mirrors.iter_mut().find(|m| m.crtc == crtc) {
                        if let Err(e) = m.surface.frame_submitted() {
                            warn!("mirror frame_submitted error: {e}");
                        }
                        m.pending_flip = false;
                    }
                    // Except while the phone is blanked: its CRTC issues no vblank, so the
                    // mirror's is the only clock.
                    if app.state.blank.is_blanked() && app.state.is_animating(Instant::now()) {
                        app.render();
                    }
                    return;
                }
                if let Err(e) = app.drm.gbm_surface.frame_submitted() {
                    warn!("frame_submitted error: {e}");
                }
                app.drm.pending_flip = false;
                app.present_feedback(meta.take());
                // Only re-prime while something changes, so a static screen lets the loop
                // stop (idle cost was ~60% of a core). `needs_render` re-arms it.
                if app.state.is_animating(Instant::now()) {
                    app.render();
                }
            }
            DrmEvent::Error(err) => warn!("DRM error: {err}"),
        })
        .map_err(|e| format!("insert drm source: {e}"))?;

    // 2. libinput.
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

    // 3. udev display hotplug on our GPU. The rescan is idempotent.
    let gpu_dev_id = device_fd.dev_id().ok();
    let udev_backend = udev::UdevBackend::new(&seat_name).map_err(|e| format!("udev: {e}"))?;
    event_loop
        .handle()
        .insert_source(udev_backend, move |event, _, app| {
            if let udev::UdevEvent::Changed { device_id } = event {
                if gpu_dev_id.is_some_and(|id| id != device_id) {
                    return;
                }
                app.refresh_outputs();
            }
        })
        .map_err(|e| format!("insert udev source: {e}"))?;

    // 4. Session activate/deactivate (VT switch).
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
                for m in &mut app.drm.mirrors {
                    m.surface.reset_buffers();
                    m.pending_flip = false;
                }
                app.render();
            }
        })
        .map_err(|e| format!("insert session source: {e}"))?;

    // 5. Client traffic, so an idle compositor sleeps.
    event_loop
        .handle()
        .insert_source(
            Generic::new(display_fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                app.display.dispatch_clients(&mut app.state).ok();
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("insert wayland display source: {e}"))?;

    // 6. New client connections.
    event_loop
        .handle()
        .insert_source(
            Generic::new(listener_fd, Interest::READ, Mode::Level),
            |_, _, app: &mut App| {
                accept_client(&app.display, &app.listener);
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| format!("insert wayland listener source: {e}"))?;

    // 7. Pre-suspend blanking; see `sleep`. Absent without logind.
    if let Some(sleep) = crate::sleep::spawn() {
        let acks = sleep.acks;
        event_loop
            .handle()
            .insert_source(sleep.events, move |event, _, app| {
                if !matches!(
                    event,
                    calloop::channel::Event::Msg(crate::sleep::Event::AboutToSleep)
                ) {
                    return;
                }
                // Blank here: the ack releases the inhibitor, so the commit must be done.
                app.state.blank.set(true);
                app.apply_blanking();
                // Everything goes dark for suspend, external displays included.
                app.drm.set_mirror_dpms(false);
                for m in &mut app.drm.mirrors {
                    m.pending_flip = false;
                }
                let _ = acks.send(());
            })
            .map_err(|e| format!("insert sleep source: {e}"))?;
    }

    info!("entering DRM frame loop");
    app.render();

    // READY pulls graphical-session.target active (Type=notify, BindsTo); the
    // portals and OSK wait on it. No-op without NOTIFY_SOCKET.
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        warn!(%e, "sd_notify READY failed");
    }

    // Tear down cleanly on SIGTERM/SIGINT. A SIGKILL mid-modeset can wedge the
    // msm DPU / Adreno GMU so the next compositor hard-resets the device.
    let signals =
        Signals::new(&[Signal::SIGTERM, Signal::SIGINT]).map_err(|e| format!("signals: {e}"))?;
    // Dispatched manually, so the stop needs its own flag.
    let running = Arc::new(AtomicBool::new(true));
    let stop = running.clone();
    event_loop
        .handle()
        .insert_source(signals, move |event, _, _app| {
            info!(signal = ?event.signal(), "termination signal; shutting down");
            stop.store(false, Ordering::Relaxed);
        })
        .map_err(|e| format!("insert signals source: {e}"))?;

    // Everything is an event source; the timeout only paces housekeeping. Tight
    // while animating (the render at the bottom primes frames when no flip is
    // in flight), 50ms idle to save battery. See `active_tick`.
    const ACTIVE_TIMEOUT: Duration = Duration::from_millis(2);
    const IDLE_TIMEOUT: Duration = Duration::from_millis(50);

    // This is the render thread.
    let mut uclamp = crate::uclamp::Uclamp::new(app.state.uclamp_min);

    while running.load(Ordering::Relaxed) && app.state.running {
        let timeout = if active_tick(
            app.state.blank.is_blanked(),
            app.drm.mirroring(),
            app.state.is_animating(Instant::now()),
        ) {
            ACTIVE_TIMEOUT
        } else {
            IDLE_TIMEOUT
        };
        if let Err(e) = event_loop.dispatch(Some(timeout), &mut app) {
            error!("event loop dispatch error: {e}");
            break;
        }

        // Before housekeeping so a synthetic gesture lands this tick.
        if let Some(chan) = &debug_chan {
            crate::debug_input::drain(&mut app.state, chan);
        }
        // Long presses are polled here: page-flips stop on an idle screen.
        if crate::catalog_watch::take(&catalog_dirty) {
            app.state.reload_catalog();
        }
        crate::keybinds::poll(&mut app.state);
        app.state.poll_launching();
        app.state.drain_sensor();
        app.state.sync_keyboard_focus();
        // An idle inhibitor (video) keeps the screen on and holds off client idle.
        let inhibited = app.state.is_idle_inhibited();
        if inhibited {
            // Keep the countdown fresh so it doesn't blank the moment the inhibitor goes.
            app.state.idle.activity(Instant::now());
        } else if app.state.idle.should_blank(Instant::now()) && !app.state.blank.is_blanked() {
            app.state.blank.toggle();
        }
        app.state.idle_notify.refresh(Instant::now(), inhibited);
        // Tell a wlr-output-power client about blanks it didn't cause.
        let blanked = app.state.blank.is_blanked();
        app.state.output_power.sync(blanked);
        app.apply_blanking();
        // A lock engaged while dark gets no frame to confirm from.
        if blanked && !app.drm.mirroring() {
            app.state.session_lock.confirm_dark();
        }
        app.apply_gamma();
        // Frames no vblank is priming. Raise the uclamp floor before rendering so
        // the first frame of a touch benefits.
        let now = Instant::now();
        let drawing = app.state.is_animating(now);
        uclamp.update(drawing, now);
        if drawing {
            app.render();
        }

        // Flush last: the render emits frame callbacks and housekeeping emits
        // configures; flushing earlier stalls clients until the next wake.
        app.display.flush_clients().ok();
    }

    // Drop order: renderer, surface, DrmDevice (master), then the seat.
    app.shutdown();

    Ok(())
}

/// Dark with no mirror: nothing can be shown, and `render` returns before
/// `advance_frame`, so unsettled animations would pin the tight timeout.
fn active_tick(blanked: bool, mirroring: bool, animating: bool) -> bool {
    animating && (!blanked || mirroring)
}

struct App {
    state: State,
    drm: Drm,
    display: Display<State>,
    listener: ListeningSocket,
    last_frame: Instant,
    clock: Clock<Monotonic>,
    touch_devices: Vec<Device>,
}

impl App {
    /// Hand over a sane device: panel on, original gamma. Master and seat are
    /// released as `App` drops.
    fn shutdown(&mut self) {
        info!("restoring DRM state for clean handoff");
        self.drm.set_primary_dpms(true);
        self.drm.set_mirror_dpms(true);
        if let Some([r, g, b]) = &self.drm.orig_gamma {
            if let Err(e) = self.drm.device_fd.set_gamma(self.drm.crtc, r, g, b) {
                warn!("restore gamma on shutdown: {e}");
            }
        }
    }

    fn handle_input(&mut self, event: InputEvent<LibinputInputBackend>) {
        // Prime a render for the next wake.
        self.state.needs_render = true;
        let now = Instant::now();
        self.state.idle.activity(now);
        self.state.idle_notify.activity(now);
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
            // The client's `wl_touch.frame` follows libinput's frame event.
            InputEvent::TouchFrame { .. } => {
                crate::touch::frame(&mut self.state);
            }
            // Palm rejection can drop a sequence with no `up`; see `touch::cancel`.
            InputEvent::TouchCancel { .. } => {
                crate::touch::cancel(&mut self.state);
            }
            InputEvent::Keyboard { event } => {
                crate::keybinds::on_key_event(
                    &mut self.state,
                    event.key_code(),
                    event.state(),
                    event.time_msec(),
                );
            }
            InputEvent::PointerButton { event } => {
                let pressed = event.state() == ButtonState::Pressed;
                debug!(
                    target: "springchick::debug",
                    "pointer button: code={} pressed={}",
                    event.button_code(),
                    pressed
                );
                crate::touch::pointer_button(
                    &mut self.state,
                    pressed,
                    event.button_code(),
                    event.time_msec(),
                );
            }
            InputEvent::PointerMotionAbsolute { event } => {
                let x = event.x_transformed(w) as f32;
                let y = event.y_transformed(h) as f32;
                debug!(target: "springchick::debug", "pointer motion: abs x={x} y={y}");
                crate::touch::pointer_motion(&mut self.state, x, y, event.time_msec());
            }
            InputEvent::PointerMotion { event } => {
                let d = event.delta();
                debug!(target: "springchick::debug", "pointer motion: dx={} dy={}", d.x, d.y);
                crate::touch::pointer_motion_relative(&mut self.state, d.x, d.y, event.time_msec());
            }
            InputEvent::PointerAxis { event } => {
                crate::touch::pointer_axis_event::<LibinputInputBackend, _>(
                    &mut self.state,
                    &event,
                    event.time_msec(),
                );
            }
            InputEvent::DeviceAdded { device }
                if device.has_capability(DeviceCapability::Touch) =>
            {
                self.touch_devices.push(device);
            }
            InputEvent::DeviceRemoved { device } => self.touch_devices.retain(|d| *d != device),
            // Only touchpads report tap fingers; mouse wheels keep their direction.
            InputEvent::DeviceAdded { mut device } if device.config_tap_finger_count() > 0 => {
                let natural = self.state.natural_scroll;
                if let Err(e) = device.config_scroll_set_natural_scroll_enabled(natural) {
                    warn!("natural scroll on {}: {e:?}", device.name());
                }
            }
            _ => {}
        }
    }

    /// DPMS off/on; without DPMS we can only stop flipping.
    fn apply_blanking(&mut self) {
        let Some(blanked) = self.state.blank.take_change() else {
            return;
        };
        // A dark panel can't act on orientation; drop the claim.
        self.state.sync_sensor_claim();
        // Disabling closes the evdev fd; libinput cancels any touch in flight.
        let mode = if blanked {
            SendEventsMode::DISABLED
        } else {
            SendEventsMode::ENABLED
        };
        for d in &mut self.touch_devices {
            if let Err(e) = d.config_send_events_set_mode(mode) {
                warn!("send-events on {}: {e:?}", d.name());
            }
        }
        if blanked {
            // External displays keep power and frames while the phone panel sleeps.
            let mirroring = self.drm.mirroring();
            info!(mirroring, "blanking panel");
            self.drm.pending_flip = false;
            self.drm.set_primary_dpms(false);
            if mirroring {
                // Mirrors' vblanks become the clock; prime the first.
                self.render();
            } else {
                for m in &mut self.drm.mirrors {
                    m.pending_flip = false;
                }
                // No frames while dark, so a slide-out would hold the client's buffer.
                self.state.layers.end_slides();
                self.drm.set_mirror_dpms(false);
                // Drop GPU texture and dmabuf import caches while dark; they're rebuilt on
                // the first frame after unblanking.
                if let Err(err) = self.drm.renderer.invalidate_caches() {
                    warn!("invalidate_caches on blank failed: {err}");
                }
            }
        } else {
            info!("unblanking panel");
            self.drm.set_primary_dpms(true);
            self.drm.set_mirror_dpms(true);
            self.drm.gbm_surface.reset_buffers();
            self.drm.pending_flip = false;
            for m in &mut self.drm.mirrors {
                m.surface.reset_buffers();
                m.pending_flip = false;
            }
            self.render();
        }
    }

    /// `Reset` restores the startup ramp.
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

    /// A fence the KMS commit can wait on instead of the CPU. `None` falls back
    /// to glFinish.
    fn frame_fence(&mut self) -> Option<SyncPoint> {
        if !self
            .drm
            .renderer
            .capabilities()
            .contains(&Capability::ExportFence)
        {
            return None;
        }
        let fence = EGLFence::create(self.drm.renderer.egl_context().display()).ok()?;
        // The fence only signals once the commands ahead of it reach the hardware.
        self.state.skia.flush_gpu();
        Some(SyncPoint::from(fence))
    }

    fn render(&mut self) {
        if !self.drm.active || self.drm.pending_flip {
            return;
        }
        // Blanked with no mirror: no target, no frame.
        if self.state.blank.is_blanked() {
            // No primary flip to rate-limit while dark; if every mirror is still
            // waiting, the frame would be dropped anyway.
            if !self.drm.mirroring() || self.drm.mirrors.iter().all(|m| m.pending_flip) {
                // Clear the request, or `is_animating` pins the loop at 2ms while dark.
                self.state.needs_render = false;
                return;
            }
        }
        // A commit after this point re-sets the flag and gets its own render.
        self.state.needs_render = false;
        let frame_start = Instant::now();

        // Clamped so a stall can't fling the springs.
        let dt = self.last_frame.elapsed().as_secs_f32().min(1.0 / 30.0);
        self.last_frame = Instant::now();
        let prep = self.state.advance_frame(dt);

        // Partial damage is only safe when nothing but the app surface can change.
        // Every Skia overlay is untracked and would never reach scanout. A locked
        // session draws its own full frame.
        let report_partial = prep.lock_view == crate::session_lock::LockView::Unlocked
            && prep.scene.window_covers_screen()
            && prep.app_surface.is_some()
            && prep.scene.cards.is_empty()
            && !prep.scene.show_home
            && prep.osd_view.is_none()
            && !self.state.bar_fading()
            && prep.dim <= 0.0
            && prep.touch_marks.is_empty()
            && prep.cursor.is_none()
            && prep.layers_below.is_empty()
            && prep.layers_above.is_empty()
            // The OSK slide-out texture isn't in the app damage either.
            && prep.closing.is_none()
            // Popups draw in their own pass, outside the app damage.
            && prep.app_popups.is_empty()
            && prep.layer_popups.is_empty();

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

        let (flip_damage, presented) =
            match self.draw_scene_into(&mut framebuffer, &prep, report_partial) {
                Some(drawn) => drawn,
                None => return,
            };
        drop(framebuffer);

        // An exportable fence goes to the atomic commit as IN_FENCE_FD so the
        // display controller waits; glFinish is the fallback and stalls the CPU.
        let sync = self.frame_fence();
        if sync.is_none() {
            self.state.skia.finish_gpu();
        }

        // Skipped while blanked with a mirror: the primary CRTC is off, a flip
        // would error. `next_buffer` keeps returning this un-queued slot, which the
        // mirrors read.
        if self.state.blank.is_blanked() {
            debug!(target: "springchick::debug", "mirror-only frame (panel blanked)");
            // Panel off: no vblank to time this frame.
            crate::presentation::discard(presented);
        } else {
            match self
                .drm
                .gbm_surface
                .queue_buffer(sync, Some(flip_damage), ())
            {
                Ok(()) => {
                    self.drm.pending_flip = true;
                    // Anything still held from an earlier frame never got a vblank (refused
                    // flip, VT switch); discard it so clients stop waiting.
                    let stale = std::mem::replace(&mut self.drm.pending_presentation, presented);
                    crate::presentation::discard(stale);
                }
                Err(e) => {
                    warn!("queue_buffer failed: {e}");
                    crate::presentation::discard(presented);
                }
            }
        }

        // After the primary flip is queued, so a mirror can't delay the phone.
        self.present_mirrors(&dmabuf);

        self.state.record_and_log_frame(frame_start);

        self.capture_pending_frames(&prep);
        self.wlr_capture_pending_frames(&prep);
        self.take_screenshot(&prep);
    }

    fn take_screenshot(&mut self, prep: &crate::FramePrep) {
        if !std::mem::take(&mut self.state.screenshot_pending) {
            return;
        }
        let size: smithay::utils::Size<i32, smithay::utils::Buffer> =
            (self.drm.output_size.w, self.drm.output_size.h).into();
        let Some(mut tex) = crate::capture::offscreen(
            &mut self.drm.renderer,
            smithay::backend::allocator::Fourcc::Xrgb8888,
            size,
        ) else {
            return;
        };
        let pixels = {
            let mut fb = match self.drm.renderer.bind(&mut tex) {
                Ok(fb) => fb,
                Err(e) => {
                    warn!("screenshot: offscreen bind failed: {e}");
                    return;
                }
            };
            if self.draw_scene_into(&mut fb, prep, false).is_none() {
                return;
            }
            crate::capture::readback_rgba(&mut self.drm.renderer, &fb, size)
        };
        if let Some(pixels) = pixels {
            crate::screenshot::to_clipboard(&mut self.state, &pixels, size);
        }
    }

    /// All mirrors draw, fence once, then flip; one still waiting skips the
    /// frame. The app rotation is undone for the external panel.
    fn present_mirrors(&mut self, src: &smithay::backend::allocator::dmabuf::Dmabuf) {
        if self.drm.mirrors.is_empty() {
            return;
        }
        let size = self.drm.output_size;
        let rotation = self.state.view_rotation();
        let mut drawn = false;
        for i in 0..self.drm.mirrors.len() {
            if self.drm.mirrors[i].pending_flip {
                continue;
            }
            match self.drm.mirrors[i].render_into(&mut self.drm.renderer, src, size, rotation) {
                Ok(()) => drawn = true,
                Err(e) => warn!("mirror render failed: {e}"),
            }
        }
        if !drawn {
            return;
        }
        self.state.skia.finish_gpu();
        for m in &mut self.drm.mirrors {
            if m.pending_flip {
                continue;
            }
            if let Err(e) = m.queue() {
                warn!("mirror queue_buffer failed: {e}");
            }
        }
    }

    /// Uses the kernel's timestamp and sequence when they're on a monotonic
    /// clock (flagged `HW_CLOCK`/`HW_COMPLETION`); otherwise reads our clock and
    /// drops the hardware flags.
    fn present_feedback(&mut self, meta: Option<DrmEventMetadata>) {
        let callbacks = std::mem::take(&mut self.drm.pending_presentation);
        if callbacks.is_empty() {
            return;
        }
        let hw_time = meta.and_then(|m| match m.time {
            DrmEventTime::Monotonic(time) => Some((time, m.sequence)),
            DrmEventTime::Realtime(_) => None,
        });
        let (time, seq, flags) = match hw_time {
            Some((time, sequence)) => (
                time,
                u64::from(sequence),
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            ),
            None => (
                self.clock.now().into(),
                0,
                wp_presentation_feedback::Kind::Vsync,
            ),
        };
        crate::presentation::present(
            callbacks,
            &self.state.output,
            time,
            crate::presentation::refresh_from_mhz(self.drm.refresh_mhz),
            seq,
            flags,
        );
    }

    /// Re-derive the mirrors after hotplug and repaint, so a new panel doesn't
    /// stay black until something animates.
    fn refresh_outputs(&mut self) {
        let before = self.drm.mirrors.len();
        crate::mirror::refresh(
            &mut self.drm.mirrors,
            &mut self.drm.device,
            &self.drm.gbm,
            &self.drm.renderer,
            self.drm.connector,
            self.drm.crtc,
        );
        self.state.external_display = self.drm.mirroring();
        if self.drm.mirrors.len() != before {
            self.state.needs_render = true;
            self.render();
        }
    }

    /// Shared by scanout and screencopy so captures are pixel-identical.
    /// `None` if the draw failed.
    fn draw_scene_into(
        &mut self,
        framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
        prep: &crate::FramePrep,
        report_partial: bool,
    ) -> Option<DrawnFrame> {
        let size = self.drm.output_size;

        // Scanout is Y-flipped vs Skia's BottomLeft surface.
        let mut sinks = crate::render::FrameSinks::default();
        let mut ctx = self.state.draw_ctx(
            prep,
            self.drm.transform,
            true,
            report_partial,
            &self.drm.rounded_tex_shader,
            &mut sinks,
        );
        let drawn =
            match crate::render::draw_scene(&mut self.drm.renderer, framebuffer, size, &mut ctx) {
                Ok(damage) => Some((damage, std::mem::take(&mut ctx.sinks.presented))),
                Err(e) => {
                    warn!("draw_scene failed: {e}");
                    crate::presentation::discard(std::mem::take(&mut ctx.sinks.presented));
                    None
                }
            };
        crate::pacing::clear_blockers(&mut self.state, sinks.unblocked);
        drawn
    }

    /// dmabuf capture keeps the GPU→CPU download in the recorder process.
    fn capture_pending_frames(&mut self, prep: &crate::FramePrep) {
        if self.state.pending_captures.is_empty() {
            return;
        }
        let present = self.clock.now();
        let transform = self.drm.transform;
        for frame in std::mem::take(&mut self.state.pending_captures) {
            let buffer = frame.buffer();
            let mut dmabuf = match get_dmabuf(&buffer) {
                Ok(d) => d.clone(),
                Err(_) => {
                    // grim and friends allocate shm.
                    match self.capture_frame_shm(&buffer, prep) {
                        Some(true) => frame.success(transform, None, present),
                        Some(false) => frame.fail(CaptureFailureReason::Unknown),
                        None => frame.fail(CaptureFailureReason::BufferConstraints),
                    }
                    continue;
                }
            };
            let mut fb = match self.drm.renderer.bind(&mut dmabuf) {
                Ok(fb) => fb,
                Err(e) => {
                    warn!("capture bind failed: {e}");
                    frame.fail(CaptureFailureReason::Unknown);
                    continue;
                }
            };
            let drawn = self.draw_scene_into(&mut fb, prep, false).is_some();
            drop(fb);
            if drawn {
                self.state.skia.finish_gpu();
                frame.success(transform, None, present);
            } else {
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    /// `None`: not usable shm (constraints failure). `Some(false)`: real failure.
    fn capture_frame_shm(
        &mut self,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        prep: &crate::FramePrep,
    ) -> Option<bool> {
        let target = crate::capture::shm_target(buffer)?;
        let src = Rectangle::from_size(target.size);
        self.capture_region_shm(buffer, prep, &target, src)
    }

    fn capture_region_shm(
        &mut self,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        prep: &crate::FramePrep,
        target: &crate::capture::ShmTarget,
        src: Rectangle<i32, smithay::utils::Buffer>,
    ) -> Option<bool> {
        let scene_size = (self.drm.output_size.w, self.drm.output_size.h).into();
        let mut tex = crate::capture::offscreen(&mut self.drm.renderer, target.fourcc, scene_size)?;
        let mut fb = match self.drm.renderer.bind(&mut tex) {
            Ok(fb) => fb,
            Err(e) => {
                warn!("screencopy: offscreen bind failed: {e}");
                return Some(false);
            }
        };
        if self.draw_scene_into(&mut fb, prep, false).is_none() {
            return Some(false);
        }
        let ok =
            crate::capture::readback_into_shm(&mut self.drm.renderer, &fb, buffer, target, src);
        Some(ok)
    }

    fn wlr_capture_pending_frames(&mut self, prep: &crate::FramePrep) {
        if self.state.wlr_captures.is_empty() {
            return;
        }
        let present = self.clock.now();
        for frame in std::mem::take(&mut self.state.wlr_captures) {
            let target = frame.target();
            let ok = self
                .capture_region_shm(&frame.buffer, prep, &target, frame.region)
                .unwrap_or(false);
            if ok {
                self.state.skia.finish_gpu();
                frame.success(present);
            } else {
                frame.failed();
            }
        }
    }
}

fn group_formats(
    formats: smithay::backend::allocator::format::FormatSet,
) -> Vec<(Fourcc, Vec<Modifier>)> {
    let mut by_code: std::collections::HashMap<Fourcc, Vec<Modifier>> =
        std::collections::HashMap::new();
    for f in formats.iter() {
        by_code.entry(f.code).or_default().push(f.modifier);
    }
    by_code.into_iter().collect()
}

fn capture_gamma(device: &DrmDeviceFd, crtc: crtc::Handle, size: u32) -> Option<[Vec<u16>; 3]> {
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

/// Computed from the mode timings: `vrefresh()` rounds 89.6Hz to 90. 0 for
/// a degenerate mode.
fn mode_refresh_mhz(mode: &smithay::reexports::drm::control::Mode) -> i32 {
    let clock = u64::from(mode.clock());
    let htotal = u64::from(mode.hsync().2);
    let vtotal = u64::from(mode.vsync().2);
    if htotal == 0 || vtotal == 0 {
        return 0;
    }
    // kHz clock; the extra 1_000_000 converts Hz to mHz.
    ((clock * 1_000_000_000) / (htotal * vtotal)) as i32
}

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
    let first = crate::mirror::scan(drm, &[], &[])?
        .into_iter()
        .next()
        .ok_or("no connected connector with a usable crtc")?;
    // Log all modes: same-resolution modes at other rates are the fallback for
    // adaptive cadence on a panel without VRR.
    if let Ok(conn) = drm.get_connector(first.connector, false) {
        for m in conn.modes() {
            let (w, h) = m.size();
            info!(
                target: "springchick::debug",
                "connector mode {}x{}@{:.3} preferred={}",
                w,
                h,
                mode_refresh_mhz(m) as f64 / 1000.0,
                m.mode_type().contains(ModeTypeFlags::PREFERRED),
            );
        }
    }
    Ok((first.connector, first.crtc, first.mode))
}

#[cfg(test)]
mod tick_tests {
    use super::active_tick;

    #[test]
    fn dark_panel_never_takes_the_tight_timeout() {
        assert!(!active_tick(true, false, true));
        assert!(!active_tick(true, false, false));
    }

    #[test]
    fn a_mirror_is_still_a_target_while_dark() {
        assert!(active_tick(true, true, true));
        assert!(!active_tick(true, true, false));
    }

    #[test]
    fn lit_panel_follows_the_animation() {
        assert!(active_tick(false, false, true));
        assert!(!active_tick(false, false, false));
    }
}
