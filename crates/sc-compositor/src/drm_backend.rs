//! DRM/KMS device backend (M4).
//!
//! Runs the compositor on real hardware via libseat/logind + udev/DRM/GBM +
//! libinput, on its own calloop event loop, frame-paced by page-flip. The
//! render itself is the shared [`crate::render::draw_scene`] — this module only
//! provides the bind/submit primitives (Approach A). See
//! `docs/superpowers/specs/2026-06-27-springchick-m4-device-backend-perf.md`.

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, GbmBufferedSurface, NodeType,
};
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::input::{
    AbsolutePositionEvent, Event as InputEventTrait, InputEvent, KeyboardKeyEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram};
use smithay::backend::renderer::{Bind, ImportDma, RendererSuper};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::drm::control::{connector, crtc, property, Device as ControlDevice};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use smithay::utils::{Clock, DeviceFd, Monotonic, Physical, Rectangle, Size, Transform};
use smithay::wayland::dmabuf::get_dmabuf;
use smithay::wayland::image_copy_capture::CaptureFailureReason;

use tracing::{debug, error, info, warn};

use crate::{accept_client, create_display, State};

/// Per-frame user data threaded through the GBM swapchain page-flip.
type FlipData = ();

struct Drm {
    device: DrmDevice,
    gbm_surface: GbmBufferedSurface<GbmAllocator<DrmDeviceFd>, FlipData>,
    renderer: GlesRenderer,
    /// GBM device, kept so a hotplugged external display can get its own
    /// swapchain without re-opening the node.
    gbm: GbmDevice<DrmDeviceFd>,
    /// Every *other* connected connector, mirroring the primary. See
    /// [`crate::mirror`]. Empty on a phone with nothing plugged in, which is
    /// the path that must stay free of extra work.
    mirrors: Vec<crate::mirror::MirrorOutput>,
    /// Rounded-corner texture program, compiled once, passed to `draw_scene`.
    rounded_tex_shader: GlesTexProgram,
    output_size: Size<i32, Physical>,
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
    /// libseat session. Declared last so it drops *after* the DrmDevice releases
    /// the master and the renderer/surface tear down — closing the seat before
    /// the device is released can leave the handoff in a bad state.
    _session: LibSeatSession,
}

/// DPMS levels, per `drm_mode.h`. Off (3) disables the pipe and powers the
/// panel down; On (0) restores it.
const DPMS_ON: property::RawValue = 0;
const DPMS_OFF: property::RawValue = 3;

impl Drm {
    /// Drive one connector's DPMS property. No-op if the driver exposes none.
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

    /// Power the phone panel on or off.
    fn set_primary_dpms(&self, on: bool) {
        self.set_connector_dpms(self.connector, self.dpms_prop, on);
    }

    /// Power every external panel on or off.
    fn set_mirror_dpms(&self, on: bool) {
        for m in &self.mirrors {
            self.set_connector_dpms(m.connector, m.dpms, on);
        }
    }

    /// Whether an external display is attached, i.e. whether blanking the phone
    /// leaves something on screen that still needs frames.
    fn mirroring(&self) -> bool {
        !self.mirrors.is_empty()
    }
}

/// Find the `DPMS` property handle on a connector, if the driver exposes it.
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

/// Entry point for the DRM backend. Selected by `SPRINGCHICK_BACKEND=drm`.
pub fn run_drm() {
    if let Err(e) = run() {
        error!("DRM backend error: {e}");
    }
}

/// Open the DRM node through the session, retrying on transient failure.
///
/// At login the previous session's compositor (the greeter) may still hold the
/// DRM master when we start. logind then refuses to hand us the device fd —
/// `session.open` fails with EPERM (`Operation not permitted`) — until it
/// finishes deactivating that session and activating ours on the seat. Without a
/// retry this aborts the compositor at login and drops the user back to the
/// greeter, which on the phone reads as a reboot. Retry a bounded number of
/// times with a short backoff so the handover can complete; after that, give up
/// and surface the last error.
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

    // --- Session (DRM master + input perms from logind on the active VT) ---
    let (mut session, session_notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    info!(seat = %seat_name, "libseat session acquired");

    // --- Pick the primary GPU ---
    let gpu_path = udev::primary_gpu(&seat_name)?.ok_or("no primary GPU found")?;
    info!(path = ?gpu_path, "primary GPU");

    // --- Open the DRM node through the session ---
    let fd = open_drm_node(&mut session, &gpu_path)?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // --- DRM device + GBM + EGL + GLES renderer ---
    let (mut drm_device, drm_notifier) = DrmDevice::new(device_fd.clone(), true)?;
    let gbm = GbmDevice::new(device_fd.clone())?;
    let egl_display = unsafe { EGLDisplay::new(gbm.clone())? };
    let egl_context = EGLContext::new(&egl_display)?;
    let mut renderer = unsafe { GlesRenderer::new(egl_context)? };
    let rounded_tex_shader = crate::render::compile_rounded_tex_shader(&mut renderer)?;

    // --- Find a connected connector + crtc + preferred mode ---
    let (connector_handle, crtc_handle, mode) = find_output(&drm_device)?;
    let (mw, mh) = mode.size();
    let output_size: Size<i32, Physical> = (mw as i32, mh as i32).into();
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
                           // zero-copy instead of falling back to slow shm software upload. Passing the
                           // main device binds version 4 with default feedback, which wl-screenrec
                           // requires to allocate capture buffers.
    let main_device = device_fd.dev_id().ok();
    state.init_dmabuf_global(&display.handle(), renderer.dmabuf_formats(), main_device);

    // Control/IPC socket (`springchick ipc …`), same as the winit backend.
    // Always listening; carries the debug-input gestures the VM tests drive too.
    let debug_chan = crate::debug_input::spawn_listener(state.output_size);

    // Screencopy dmabuf constraints: the render node + format/modifier set a
    // recorder must allocate its capture buffers with, so we can blit into them
    // zero-copy. Falls back to shm-only if the node can't be resolved.
    if let Ok(node) = DrmNode::from_file(&device_fd) {
        let render_node = node
            .node_with_type(NodeType::Render)
            .and_then(Result::ok)
            .unwrap_or(node);
        state.capture_formats = Some((render_node, group_formats(renderer.dmabuf_formats())));
    }

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

    // Any additional connected connector mirrors the panel from the first
    // frame; hotplug after this is handled by the udev source below.
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
    // The key path reads this to decide whether a blanked phone panel means the
    // session is asleep or merely that the user is watching the other screen.
    state.external_display = drm.mirroring();

    // Duplicate the wayland fds before `display`/`listener` move into `app`, so
    // both can be registered as calloop sources below. Without these the loop
    // has no way to learn about client traffic and has to poll for it.
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
    };

    // --- calloop sources ---

    // 1. DRM page-flip events.
    event_loop
        .handle()
        .insert_source(drm_notifier, |event, _meta, app| match event {
            DrmEvent::VBlank(crtc) => {
                // A mirror's vblank normally only releases its own buffer —
                // mirrors are driven by the primary's render, never the other
                // way round.
                if crtc != app.drm.crtc {
                    if let Some(m) = app.drm.mirrors.iter_mut().find(|m| m.crtc == crtc) {
                        if let Err(e) = m.surface.frame_submitted() {
                            warn!("mirror frame_submitted error: {e}");
                        }
                        m.pending_flip = false;
                    }
                    // ...except while the phone panel is blanked. Then the
                    // primary CRTC is powered down and issues no vblank at all,
                    // so the external display's own vblank is the only clock
                    // left to carry the next frame.
                    if app.state.blank.is_blanked() && app.state.is_animating(Instant::now()) {
                        app.render();
                    }
                    return;
                }
                if let Err(e) = app.drm.gbm_surface.frame_submitted() {
                    warn!("frame_submitted error: {e}");
                }
                app.drm.pending_flip = false;
                // Only re-prime a flip if something is still changing. On a
                // static screen (idle home, quiescent app) this lets the vblank
                // loop stop instead of rendering every frame forever — the idle
                // CPU cost was ~60% of one core on-device. A commit/input/
                // animation start re-arms via `needs_render` in the 2ms timeout.
                if app.state.is_animating(Instant::now()) {
                    app.render();
                }
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

    // 3. udev: display hotplug. A connector coming or going on our GPU raises a
    // `Changed` event for the node; anything else on the seat is ignored. The
    // rescan is cheap and idempotent, so a spurious event costs nothing.
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

    // 4. Session activate/deactivate (VT-switch).
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

    // 5. Wayland client traffic. Registering these means an idle compositor
    //    sleeps until a client actually says something, instead of waking to ask.
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

    // 6. New client connections on the listening socket.
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

    // 7. Pre-suspend blanking. logind holds the suspend off until we ack, so the
    //    DPMS-off lands before the machine goes down and `Blank` still describes
    //    the panel on the other side — which is what makes the press that wakes
    //    the machine read as a wake instead of firing `toggle-display` and
    //    blanking the screen the user just woke. Absent (no bus, no logind) is a
    //    normal state: the panel simply does not blank before sleep.
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
                // Blank here rather than leaving it to the loop body below: the
                // ack releases the inhibitor, so the commit has to have happened
                // by the time we send it.
                app.state.blank.set(true);
                app.apply_blanking();
                // Everything goes dark for a suspend, external displays
                // included: an attached panel keeps its frames while the *phone*
                // blanks, but the whole machine is about to stop rendering.
                app.drm.set_mirror_dpms(false);
                for m in &mut app.drm.mirrors {
                    m.pending_flip = false;
                }
                let _ = acks.send(());
            })
            .map_err(|e| format!("insert sleep source: {e}"))?;
    }

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

    // Graceful shutdown: catch SIGTERM (systemd stop, e.g. nixos-rebuild) and
    // SIGINT (Ctrl-C on a bare VT launch), stop the loop, and tear down the DRM
    // output cleanly below. Without this the process is SIGKILL'd mid-modeset,
    // which can leave the msm DPU / Adreno GMU wedged so the *next* compositor's
    // DrmDevice::new hard-resets the device at login.
    let signals =
        Signals::new(&[Signal::SIGTERM, Signal::SIGINT]).map_err(|e| format!("signals: {e}"))?;
    // Dispatched manually below rather than via `EventLoop::run`, so the stop
    // request needs its own flag instead of `LoopSignal::stop`.
    let running = Arc::new(AtomicBool::new(true));
    let stop = running.clone();
    event_loop
        .handle()
        .insert_source(signals, move |event, _, _app| {
            info!(signal = ?event.signal(), "termination signal; shutting down");
            stop.store(false, Ordering::Relaxed);
        })
        .map_err(|e| format!("insert signals source: {e}"))?;

    // Wayland traffic, input, udev and page-flips are all event sources now, so
    // the timeout only paces the housekeeping polls below. While animating it
    // stays tight, because the `is_animating` render at the bottom is what
    // primes a frame when no page-flip is in flight and a 50ms gap there would
    // be a visible stutter. Idle, nothing needs that: the long-press threshold
    // and the idle-blank countdown are decided on timescales where 50ms is
    // invisible, and polling faster than that only burns battery.
    const ACTIVE_TIMEOUT: Duration = Duration::from_millis(2);
    const IDLE_TIMEOUT: Duration = Duration::from_millis(50);

    // Scheduler utilization floor for this thread, which is the render thread.
    let mut uclamp = crate::uclamp::Uclamp::new(app.state.uclamp_min);

    while running.load(Ordering::Relaxed) {
        let timeout = if app.state.is_animating(Instant::now()) {
            ACTIVE_TIMEOUT
        } else {
            IDLE_TIMEOUT
        };
        if let Err(e) = event_loop.dispatch(Some(timeout), &mut app) {
            error!("event loop dispatch error: {e}");
            break;
        }

        // Drain the debug-input socket (dev harness) before housekeeping so a
        // synthetic gesture takes effect this tick. `is_animating` reports true
        // while one is in flight, so the render below keeps advancing it.
        if let Some(chan) = &debug_chan {
            crate::debug_input::drain(&mut app.state, chan);
        }
        // Long presses are polled here, not in `render`: page-flips stop when
        // nothing animates, so a frame-driven poll would never fire on an idle
        // screen.
        crate::keybinds::poll(&mut app.state);
        app.state.poll_launching();
        app.state.drain_sensor();
        app.state.sync_keyboard_focus();
        // Idle-blank once the timeout elapses. Reuses the power-button DPMS-off
        // path; a power-button press wakes it and resets the countdown (input
        // routes through handle_input → idle.activity).
        // A visible surface holding a zwp_idle_inhibitor (video playback) keeps
        // the screen on and holds off client idle notifications alike.
        let inhibited = app.state.is_idle_inhibited();
        if inhibited {
            // Keep the countdown fresh rather than merely skipping the check, so
            // the screen doesn't blank the instant the inhibitor is released.
            app.state.idle.activity(Instant::now());
        } else if app.state.idle.should_blank(Instant::now()) && !app.state.blank.is_blanked() {
            app.state.blank.toggle();
        }
        // ext-idle-notify timeouts: same poll, client-driven timeouts.
        app.state.idle_notify.refresh(Instant::now(), inhibited);
        app.apply_blanking();
        app.apply_gamma();
        // Drive frames that no vblank is priming: a fresh commit/input
        // (`needs_render`) or an animation that started on an otherwise idle
        // screen. `render` early-returns on pending_flip, so once a flip is in
        // flight the vblank handler carries the animation and this is a no-op.
        // Raise the floor before rendering, not after: applying it a frame late
        // would miss the first frame of a touch, which is the slowest one and
        // the whole reason this exists.
        let now = Instant::now();
        let drawing = app.state.is_animating(now);
        uclamp.update(drawing, now);
        if drawing {
            app.render();
        }

        // Flush last, and unconditionally: the render above is what emits frame
        // callbacks, and the housekeeping emits configures, so flushing before
        // them would leave a client waiting on a callback stalled until the next
        // wake — up to a full idle timeout away.
        app.display.flush_clients().ok();
    }

    // Loop stopped by SIGTERM/SIGINT: restore DRM state, then let `app` drop —
    // which tears down the renderer, surface, DrmDevice (releases the master)
    // and finally the libseat session, in that order.
    app.shutdown();

    Ok(())
}

/// Aggregate owned by the calloop loop.
struct App {
    state: State,
    drm: Drm,
    display: Display<State>,
    listener: ListeningSocket,
    last_frame: Instant,
    /// Monotonic clock for screencopy frame presentation timestamps.
    clock: Clock<Monotonic>,
}

impl App {
    /// Restore DRM state before the compositor exits, so the greeter / next
    /// compositor inherits a sane device: panel powered on, original gamma ramp.
    /// The heavy lifting (releasing the DRM master, closing the seat) happens as
    /// `App` drops right after this returns.
    fn shutdown(&mut self) {
        info!("restoring DRM state for clean handoff");
        // Never hand over a blanked panel — nor a dark external one.
        self.drm.set_primary_dpms(true);
        self.drm.set_mirror_dpms(true);
        // Undo any gamma-control client's ramp.
        if let Some([r, g, b]) = &self.drm.orig_gamma {
            if let Err(e) = self.drm.device_fd.set_gamma(self.drm.crtc, r, g, b) {
                warn!("restore gamma on shutdown: {e}");
            }
        }
    }

    fn handle_input(&mut self, event: InputEvent<LibinputInputBackend>) {
        // Any input may change on-screen state (or the app it's forwarded to
        // will commit in response). Prime a render for the next loop wake.
        self.state.needs_render = true;
        // Any input is activity: restart the idle-blank countdown and resume any
        // client that we told had gone idle.
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
            // libinput reports a frame after every batch of simultaneous touch
            // changes; that — not the individual events — is when the client
            // gets its `wl_touch.frame`.
            InputEvent::TouchFrame { .. } => {
                crate::touch::frame(&mut self.state);
            }
            // Palm rejection (and friends) can abandon a sequence with no `up`.
            // Dropping it strands the slot: see `touch::cancel`.
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
            // The phone panel powers down either way. An external display is a
            // second screen the user is still looking at (video out, a
            // presentation), so it keeps its power *and* its frames: the scene
            // is still composited, just never flipped to the dark panel. See
            // `render` and `present_mirrors`.
            let mirroring = self.drm.mirroring();
            info!(mirroring, "blanking panel");
            self.drm.pending_flip = false;
            self.drm.set_primary_dpms(false);
            if mirroring {
                // Mirrors keep flipping, so their vblanks become the frame
                // clock; prime the first one.
                self.render();
            } else {
                for m in &mut self.drm.mirrors {
                    m.pending_flip = false;
                }
                self.drm.set_mirror_dpms(false);
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
        if !self.drm.active || self.drm.pending_flip {
            return;
        }
        // Blanked with nothing else attached: no target at all, so no frame. With
        // a mirror attached the scene is still composited for it — the phone's
        // own flip is what gets skipped, further down.
        if self.state.blank.is_blanked() {
            // `pending_flip` on the primary is what normally rate-limits this;
            // with the panel dark there is none, so the mirrors' own in-flight
            // state is the backstop. Every mirror still waiting means the frame
            // would be composited and then dropped by `present_mirrors`.
            if !self.drm.mirroring() || self.drm.mirrors.iter().all(|m| m.pending_flip) {
                return;
            }
        }
        // Consuming the request now: this frame reflects current state. A commit
        // arriving after this point re-sets the flag and gets its own render.
        self.state.needs_render = false;
        let frame_start = Instant::now();

        // Variable step, clamped so a long stall can't fling the springs.
        let dt = self.last_frame.elapsed().as_secs_f32().min(1.0 / 30.0);
        self.last_frame = Instant::now();
        let prep = self.state.advance_frame(dt);

        // Partial page-flip damage is only safe when nothing but the app surface
        // could have changed: fullscreen app, no switcher/home/OSD/OSK, no touch
        // markers, and the bar not mid-fade. Any of these repaint via Skia
        // (untracked) and would leave stale pixels if excluded from the damage
        // hint — for the touch overlay that means the marks never reach scanout.
        // A locked session draws its own (full-damage) frame and never the app,
        // so the app-shaped fast path does not apply to it.
        let report_partial = prep.lock_view == crate::session_lock::LockView::Unlocked
            && prep.scene.window_covers_screen()
            && prep.app_surface.is_some()
            && prep.scene.cards.is_empty()
            && !prep.scene.show_home
            && prep.osd_view.is_none()
            && !self.state.bar_fading()
            && prep.touch_marks.is_empty()
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

        let flip_damage = match self.draw_scene_into(&mut framebuffer, &prep, report_partial) {
            Some(damage) => damage,
            None => return,
        };
        drop(framebuffer);

        // Fence the frame: block until smithay + Skia GL work has completed so
        // the page-flip never scans out a half-rendered buffer (tearing). We
        // have ~6ms of slack under the 11ms 90Hz budget, so the stall is free.
        self.state.skia.finish_gpu();

        // Queue the page-flip; vblank fires the next render.
        //
        // Skipped while the panel is blanked and only a mirror is watching: the
        // primary CRTC is powered down, so a flip on it would error and its
        // vblank would never arrive. `next_buffer` keeps handing back this same
        // un-queued slot, which is exactly the buffer the mirrors read below.
        if self.state.blank.is_blanked() {
            debug!(target: "springchick::debug", "mirror-only frame (panel blanked)");
        } else {
            match self
                .drm
                .gbm_surface
                .queue_buffer(None, Some(flip_damage), ())
            {
                Ok(()) => self.drm.pending_flip = true,
                Err(e) => warn!("queue_buffer failed: {e}"),
            }
        }

        // Mirror the frame we just composited onto every external display. Done
        // after the primary's flip is queued so an external panel can never
        // delay the phone's, and after `finish_gpu` so the texture we sample is
        // complete. `dmabuf` is still the buffer we rendered into — queueing the
        // flip does not invalidate it for reading.
        self.present_mirrors(&dmabuf);

        self.state.record_and_log_frame(frame_start);

        // Screencopy: satisfy any pending capture requests from the same scene.
        self.capture_pending_frames(&prep);
        self.wlr_capture_pending_frames(&prep);
    }

    /// Copy the primary's just-composited scanout buffer onto every external
    /// display. No-op when nothing is plugged in, which is the common case.
    ///
    /// All mirrors are drawn first, then fenced once, then flipped — a single
    /// `finish_gpu` covers the batch. A mirror still waiting on its own vblank
    /// skips this frame rather than holding the phone back.
    ///
    /// The current app rotation goes with it: a fullscreen app turned landscape
    /// is drawn sideways in the phone's portrait buffer, and an external panel
    /// that is not being held sideways has to have that undone.
    fn present_mirrors(&mut self, src: &smithay::backend::allocator::dmabuf::Dmabuf) {
        if self.drm.mirrors.is_empty() {
            return;
        }
        let size = self.drm.output_size;
        let rotation = self.state.rotation;
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

    /// Re-derive the mirror set after a display hotplug, then repaint so a
    /// freshly attached panel shows the current frame instead of staying black
    /// until something else animates.
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

    /// Compose the current scene (`prep`) into `framebuffer`. Shared by the
    /// scanout path and screencopy so a captured frame is pixel-identical to what
    /// is presented. Returns the damage, or `None` if the draw failed (logged).
    fn draw_scene_into(
        &mut self,
        framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
        prep: &crate::FramePrep,
        report_partial: bool,
    ) -> Option<Vec<Rectangle<i32, Physical>>> {
        let size = self.drm.output_size;

        // The GBM scanout buffer has the opposite Y-origin from Skia's
        // BottomLeft surface, hence `skia_flip_y`.
        let mut ctx = self.state.draw_ctx(
            prep,
            self.drm.transform,
            true,
            report_partial,
            &self.drm.rounded_tex_shader,
        );
        match crate::render::draw_scene(&mut self.drm.renderer, framebuffer, size, &mut ctx) {
            Ok(damage) => Some(damage),
            Err(e) => {
                warn!("draw_scene failed: {e}");
                None
            }
        }
    }

    /// Blit the just-composited scene into each pending screencopy frame's
    /// dmabuf and signal completion. Runs only while a recorder is attached, so
    /// it costs nothing on the idle path. dmabuf capture keeps the GPU→CPU
    /// download in the recorder process, off our render thread; shm buffers are
    /// rejected (a dmabuf-first recorder falls back on its own).
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
                    // Not a dmabuf: grim and friends allocate shm, so take the
                    // readback path rather than failing the frame.
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
                // Fence so the recorder never maps a half-written buffer.
                self.state.skia.finish_gpu();
                frame.success(transform, None, present);
            } else {
                frame.fail(CaptureFailureReason::Unknown);
            }
        }
    }

    /// shm capture: draw the scene into an offscreen texture and read it back
    /// into the client's pool. `None` means the buffer isn't usable shm at all
    /// (report as a constraints failure), `Some(false)` a real failure.
    fn capture_frame_shm(
        &mut self,
        buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
        prep: &crate::FramePrep,
    ) -> Option<bool> {
        let target = crate::capture::shm_target(buffer)?;
        let src = Rectangle::from_size(target.size);
        self.capture_region_shm(buffer, prep, &target, src)
    }

    /// The shared half of both capture protocols: compose the scene into an
    /// offscreen texture the size of the output, then read `src` out of it into
    /// the client's shm buffer.
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

    /// Serve pending wlr-screencopy copies from the same composited scene.
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
                // Fence so the client never maps a half-written buffer.
                self.state.skia.finish_gpu();
                frame.success(present);
            } else {
                frame.failed();
            }
        }
    }
}

/// Group a renderer's flat dmabuf format set into the `(fourcc, [modifiers])`
/// shape the screencopy dmabuf constraints expect.
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

/// Snapshot the CRTC's current gamma ramp so it can be restored when a
/// gamma-control client releases the output. Returns `None` if the CRTC has no
/// LUT or the read fails.
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
    let first = crate::mirror::scan(drm, &[], &[])?
        .into_iter()
        .next()
        .ok_or("no connected connector with a usable crtc")?;
    Ok((first.connector, first.crtc, first.mode))
}
