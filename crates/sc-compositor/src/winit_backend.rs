//! The winit dev backend: a windowed compositor for desktop development. Runs
//! the same shell as the DRM backend, differing only in event pumping and how a
//! frame is presented.

use std::time::Duration;

use smithay::backend::input::{Event, InputEvent, KeyboardKeyEvent};
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram};
use smithay::backend::renderer::ImportDma;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::backend::SwapBuffersError;
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::reexports::winit::window::Window as WinitWindow;
use smithay::utils::{Rectangle, Transform};

use sc_shell_model::persist;

use tracing::{error, info, warn};

use crate::session::{accept_client, create_display, publish_wayland_display};
use crate::state::State;
use crate::{backend, debug_input, ipc, keybinds, render, touch};

pub(crate) fn run_winit() {
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
    let rounded_tex_shader = match render::compile_rounded_tex_shader(gfx_backend.renderer()) {
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

    // Control/IPC socket (`springchick ipc …`). Always listening; the client
    // connects to the same path. Shared setup with the DRM backend.
    let debug_chan = debug_input::spawn_listener(state.output_size);

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
        if let Some(chan) = &debug_chan {
            debug_input::drain(&mut state, chan);
        }

        // ext-idle-notify timeouts (polled; see `idle_notify`).
        let inhibited = state.is_idle_inhibited();
        state.idle_notify.refresh(std::time::Instant::now(), inhibited);

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

    // Remove the control socket file (best-effort). `spawn` also unlinks any
    // stale socket before binding, so this is just tidy-up.
    if debug_chan.is_some() {
        let _ = std::fs::remove_file(ipc::socket_path());
    }

    // Save state.
    if let Err(e) = persist::save(&state.model, &persist::state_path()) {
        warn!(%e, "failed to save shell model");
    }

    info!("compositor shut down");
}

/// Handle input events from the winit backend.
fn handle_winit_input(
    state: &mut State,
    event: InputEvent<smithay::backend::winit::WinitInput>,
) {
    use smithay::backend::input::{AbsolutePositionEvent, ButtonState, PointerButtonEvent};

    // Any input resumes clients we told had gone idle (ext-idle-notify).
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
    {
        // winit presents an already-correct framebuffer (no Skia y-flip) and
        // submits full damage, so no partial hint.
        let mut ctx = state.draw_ctx(&prep, Transform::Flipped180, false, false, rounded_tex_shader);
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
