//! Nested winit backend for development.

use std::time::Duration;

use smithay::backend::input::{Event, InputEvent, KeyboardKeyEvent};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram};
use smithay::backend::renderer::ImportDma;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::backend::SwapBuffersError;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::event_loop::pump_events::PumpStatus;
use smithay::reexports::winit::window::WindowAttributes;
use smithay::utils::{Clock, Monotonic, Rectangle, Transform};

use sc_shell_model::persist;

use tracing::{error, info, warn};

use crate::session::{accept_client, create_display, publish_wayland_display};
use crate::state::State;
use crate::{backend, debug_input, ipc, keybinds, render, touch};

pub(crate) fn run_winit() {
    let (win_w, win_h) = backend::dev_window_size();
    info!(width = win_w, height = win_h, "starting winit dev backend");

    let attributes = WindowAttributes::default()
        .with_title("springchick")
        .with_surface_size(LogicalSize::new(f64::from(win_w), f64::from(win_h)))
        .with_visible(true);

    let (mut gfx_backend, mut winit_evt) =
        match winit::init_from_attributes::<GlesRenderer>(attributes) {
            Ok(ret) => ret,
            Err(err) => {
                error!(err = ?err, "failed to initialize winit backend");
                return;
            }
        };

    let (mut display, listener, socket_name) = create_display().expect("create wayland display");
    // Don't touch the host session's user services.
    publish_wayland_display(&socket_name, false);

    let rounded_tex_shader = match render::compile_rounded_tex_shader(gfx_backend.renderer()) {
        Ok(prog) => prog,
        Err(err) => {
            error!(err = ?err, "failed to compile rounded-corner shader");
            return;
        }
    };

    let actual_size = gfx_backend.window_size();
    info!(w = actual_size.w, h = actual_size.h, "actual output size");
    let mut state = State::new(
        &display,
        socket_name.clone(),
        (actual_size.w, actual_size.h),
    );
    // dmabuf v4 when EGL resolves a render node: recorders bind v4
    // unconditionally and die on a v3 global. Falls back to v3 (llvmpipe).
    let main_device = smithay::backend::egl::EGLDevice::device_for_display(
        gfx_backend.renderer().egl_context().display(),
    )
    .ok()
    .and_then(|device| device.try_get_render_node().ok().flatten())
    .map(|node| node.dev_id());
    if main_device.is_none() {
        info!("no EGL render node; advertising zwp_linux_dmabuf v3");
    }
    state.init_dmabuf_global(
        &display.handle(),
        gfx_backend.renderer().dmabuf_formats(),
        main_device,
    );

    let debug_chan = debug_input::spawn_listener(state.panel_size);
    let catalog_dirty = crate::catalog_watch::spawn();

    info!("entering frame loop");

    while state.running {
        accept_client(&display, &listener);

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

        if let Some(chan) = &debug_chan {
            debug_input::drain(&mut state, chan);
        }

        if crate::catalog_watch::take(&catalog_dirty) {
            state.reload_catalog();
        }

        let inhibited = state.is_idle_inhibited();
        state
            .idle_notify
            .refresh(std::time::Instant::now(), inhibited);

        let blanked = state.blank.is_blanked();
        state.output_power.sync(blanked);

        display.dispatch_clients(&mut state).ok();
        display.flush_clients().ok();

        if let Err(err) = render_frame(&mut gfx_backend, &mut state, &rounded_tex_shader) {
            match err {
                SwapBuffersError::ContextLost(err) => {
                    error!(%err, "context lost, exiting");
                    break;
                }
                other => warn!(%other, "transient render error"),
            }
        }

        // TODO: switch to a calloop timer.
        std::thread::sleep(Duration::from_millis(1));
    }

    if debug_chan.is_some() {
        let _ = std::fs::remove_file(ipc::socket_path());
    }

    if let Err(e) = persist::save(&state.model, &persist::state_path()) {
        warn!(%e, "failed to save shell model");
    }

    info!("compositor shut down");
}

/// `None` = not usable shm.
fn capture_frame_shm(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    state: &mut State,
    prep: &crate::FramePrep,
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    rounded_tex_shader: &GlesTexProgram,
) -> Option<bool> {
    let target = crate::capture::shm_target(buffer)?;
    let src = Rectangle::from_size(target.size);
    capture_region_shm(
        backend,
        state,
        prep,
        buffer,
        &target,
        src,
        rounded_tex_shader,
    )
}

fn capture_region_shm(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    state: &mut State,
    prep: &crate::FramePrep,
    buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    target: &crate::capture::ShmTarget,
    src: Rectangle<i32, smithay::utils::Buffer>,
    rounded_tex_shader: &GlesTexProgram,
) -> Option<bool> {
    use smithay::backend::renderer::Bind;

    let size = backend.window_size();
    let renderer = backend.renderer();
    let mut tex = crate::capture::offscreen(renderer, target.fourcc, (size.w, size.h).into())?;
    let mut fb = match renderer.bind(&mut tex) {
        Ok(fb) => fb,
        Err(e) => {
            warn!("screencopy: offscreen bind failed: {e}");
            return Some(false);
        }
    };
    {
        // Our own FBO is Y-flipped relative to winit's surface: Skia takes the DRM
        // flip and the app pass drops Flipped180. Capture feedback is discarded.
        let mut sinks = render::FrameSinks::default();
        let mut ctx = state.draw_ctx(
            prep,
            Transform::Normal,
            true,
            false,
            rounded_tex_shader,
            &mut sinks,
        );
        let draw = render::draw_scene(renderer, &mut fb, size, &mut ctx);
        crate::presentation::discard(sinks.presented);
        crate::pacing::clear_blockers(state, sinks.unblocked);
        if let Err(e) = draw {
            warn!("screencopy: draw_scene failed: {e}");
            return Some(false);
        }
    }
    Some(crate::capture::readback_into_shm(
        renderer, &fb, buffer, target, src,
    ))
}

fn take_screenshot(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    state: &mut State,
    prep: &crate::FramePrep,
    rounded_tex_shader: &GlesTexProgram,
) {
    use smithay::backend::renderer::Bind;

    let size = backend.window_size();
    let buf_size: smithay::utils::Size<i32, smithay::utils::Buffer> = (size.w, size.h).into();
    let renderer = backend.renderer();
    let Some(mut tex) = crate::capture::offscreen(
        renderer,
        smithay::backend::allocator::Fourcc::Xrgb8888,
        buf_size,
    ) else {
        return;
    };
    let pixels = {
        let mut fb = match renderer.bind(&mut tex) {
            Ok(fb) => fb,
            Err(e) => {
                warn!("screenshot: offscreen bind failed: {e}");
                return;
            }
        };
        let mut sinks = render::FrameSinks::default();
        let mut ctx = state.draw_ctx(
            prep,
            Transform::Normal,
            true,
            false,
            rounded_tex_shader,
            &mut sinks,
        );
        let draw = render::draw_scene(renderer, &mut fb, size, &mut ctx);
        crate::presentation::discard(sinks.presented);
        crate::pacing::clear_blockers(state, sinks.unblocked);
        if let Err(e) = draw {
            warn!("screenshot: draw_scene failed: {e}");
            return;
        }
        crate::capture::readback_rgba(renderer, &fb, buf_size)
    };
    if let Some(pixels) = pixels {
        crate::screenshot::to_clipboard(state, &pixels, buf_size);
    }
}

fn handle_winit_input(state: &mut State, event: InputEvent<winit::WinitInput>) {
    use smithay::backend::input::{AbsolutePositionEvent, ButtonState, PointerButtonEvent};

    state.idle_notify.activity(std::time::Instant::now());

    match event {
        InputEvent::Keyboard { event } => {
            keybinds::on_key_event(state, event.key_code(), event.state(), event.time_msec());
        }
        InputEvent::PointerButton { event } => {
            let pressed = event.state() == ButtonState::Pressed;
            touch::pointer_button(state, pressed, event.button_code(), event.time_msec());
        }
        InputEvent::PointerMotionAbsolute { event } => {
            let x = event.x_transformed(state.panel_size.0) as f32;
            let y = event.y_transformed(state.panel_size.1) as f32;
            touch::pointer_motion(state, x, y, event.time_msec());
        }
        InputEvent::PointerAxis { event } => {
            let time = event.time_msec();
            touch::pointer_axis_event::<winit::WinitInput, _>(state, &event, time);
        }
        _ => {}
    }
}

fn render_frame(
    backend: &mut WinitGraphicsBackend<GlesRenderer>,
    state: &mut State,
    rounded_tex_shader: &GlesTexProgram,
) -> Result<(), SwapBuffersError> {
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);
    let frame_start = std::time::Instant::now();

    let prep = state.advance_frame(1.0 / 90.0);

    keybinds::poll(state);
    state.poll_launching();
    state.drain_sensor();
    state.sync_keyboard_focus();

    let (renderer, mut framebuffer) = backend.bind()?;
    let mut sinks = render::FrameSinks::default();
    {
        // winit presents an already-correct framebuffer with full damage.
        let mut ctx = state.draw_ctx(
            &prep,
            Transform::Flipped180,
            false,
            false,
            rounded_tex_shader,
            &mut sinks,
        );
        render::draw_scene(renderer, &mut framebuffer, size, &mut ctx)?;
    }
    crate::pacing::clear_blockers(state, sinks.unblocked);

    drop(framebuffer);
    let result = backend.submit(Some(&[damage]));

    // Nested: no vblank or sequence, so our clock and a software-present flag.
    if result.is_ok() {
        crate::presentation::present(
            sinks.presented,
            &state.output,
            Clock::<Monotonic>::new().now().into(),
            smithay::wayland::presentation::Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );
    } else {
        crate::presentation::discard(sinks.presented);
    }

    state.record_and_log_frame(frame_start);

    if !state.wlr_captures.is_empty() {
        let present = smithay::utils::Clock::<smithay::utils::Monotonic>::new().now();
        for frame in std::mem::take(&mut state.wlr_captures) {
            let target = frame.target();
            let ok = capture_region_shm(
                backend,
                state,
                &prep,
                &frame.buffer,
                &target,
                frame.region,
                rounded_tex_shader,
            )
            .unwrap_or(false);
            if ok {
                frame.success(present);
            } else {
                frame.failed();
            }
        }
    }

    if std::mem::take(&mut state.screenshot_pending) {
        take_screenshot(backend, state, &prep, rounded_tex_shader);
    }

    if state.pending_captures.is_empty() {
        return result;
    }
    let present = smithay::utils::Clock::<smithay::utils::Monotonic>::new().now();
    for frame in std::mem::take(&mut state.pending_captures) {
        let buffer = frame.buffer();
        let done = capture_frame_shm(backend, state, &prep, &buffer, rounded_tex_shader);
        match done {
            Some(true) => frame.success(Transform::Normal, None, present),
            Some(false) => {
                frame.fail(smithay::wayland::image_copy_capture::CaptureFailureReason::Unknown)
            }
            None => frame.fail(
                smithay::wayland::image_copy_capture::CaptureFailureReason::BufferConstraints,
            ),
        }
    }

    result
}
