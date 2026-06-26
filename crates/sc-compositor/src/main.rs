//! springchick compositor binary.
//!
//! Task 7 integration spike: a winit dev backend that opens a window sized to the
//! Fairphone 5 logical geometry, clears it to a solid color every frame, and exits
//! cleanly on window close.
//!
//! Task 8 (architecture go/no-go): each frame Skia's Ganesh-GL backend draws one
//! blurred rounded rectangle into the *same* framebuffer Smithay presents, sharing
//! Smithay's GLES/EGL context. See [`skia_gl`].

mod backend;
mod skia_gl;

use backend::{BackendKind, FP5_HEIGHT, FP5_WIDTH};
use skia_gl::SkiaGl;

use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Color32F, Frame, Renderer};
use smithay::backend::winit::{self, WinitEvent};
use smithay::backend::SwapBuffersError;
use smithay::reexports::winit::dpi::LogicalSize;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::reexports::winit::window::Window as WinitWindow;
use smithay::utils::{Rectangle, Transform};

use tracing::{error, info, trace, warn};

/// Background clear color (springchick teal-ish), premultiplied RGBA in [0, 1].
const CLEAR_COLOR: Color32F = Color32F::new(0.06, 0.10, 0.14, 1.0);

/// Target frame duration for the winit dev backend (~90 Hz).
/// Placeholder until real DRM vblank pacing lands in Milestone 5.
const FRAME_BUDGET: std::time::Duration = std::time::Duration::from_micros(11_111);

fn main() {
    init_tracing();

    match BackendKind::from_env() {
        BackendKind::Winit => run_winit(),
        BackendKind::Drm => {
            todo!("device backend — Milestone 5");
        }
    }
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

    let (mut backend, mut winit) = match winit::init_from_attributes::<GlesRenderer>(attributes) {
        Ok(ret) => ret,
        Err(err) => {
            error!(err = ?err, "failed to initialize winit backend");
            let mut source = std::error::Error::source(&err);
            while let Some(e) = source {
                error!("  caused by: {e}  [{e:?}]");
                source = e.source();
            }
            return;
        }
    };

    info!("winit backend initialized, entering frame loop");

    let mut running = true;
    let mut frame_count: u64 = 0;
    // Created once, reused every frame; lazily binds to the GL context on first draw.
    let mut skia = SkiaGl::new();

    while running {
        let frame_start = std::time::Instant::now();

        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::CloseRequested => {
                info!("window close requested");
                running = false;
            }
            WinitEvent::Resized { size, .. } => {
                trace!(?size, "window resized");
            }
            WinitEvent::Focus(focused) => {
                trace!(focused, "focus changed");
            }
            WinitEvent::Input(_) => {
                // Input wiring lands with the input/seat work (Task 9+).
            }
            WinitEvent::Redraw => {
                // rendering unconditionally per tick; Redraw unused until render-on-demand
            }
        });

        if let PumpStatus::Exit(_) = status {
            info!("winit event loop requested exit");
            break;
        }

        if !running {
            break;
        }

        if let Err(err) = render_frame(&mut backend, &mut skia) {
            match err {
                SwapBuffersError::ContextLost(err) => {
                    error!(%err, "critical rendering error, exiting");
                    break;
                }
                other => warn!(%other, "transient rendering error"),
            }
        }
        trace!(frame = frame_count, "frame submitted");
        frame_count = frame_count.wrapping_add(1);

        // Sleep the remainder of the frame budget to cap the loop at ~90 Hz.
        if let Some(remaining) = FRAME_BUDGET.checked_sub(frame_start.elapsed()) {
            std::thread::sleep(remaining);
        }
    }

    info!(frames = frame_count, "winit backend shut down cleanly");
}

/// Clear the whole output to [`CLEAR_COLOR`], let Skia draw the spike on top into
/// the same framebuffer, and submit the buffer.
fn render_frame(
    backend: &mut winit::WinitGraphicsBackend<GlesRenderer>,
    skia: &mut SkiaGl,
) -> Result<(), SwapBuffersError> {
    let size = backend.window_size();
    let damage = Rectangle::from_size(size);

    let (renderer, mut framebuffer) = backend.bind()?;
    {
        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Flipped180)
            .map_err(SwapBuffersError::from)?;
        frame
            .clear(CLEAR_COLOR, &[damage])
            .map_err(SwapBuffersError::from)?;
        let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    }
    // Smithay's frame is finished but the target framebuffer is still bound and the
    // EGL context current: Skia draws the spike into that same FBO before swap.
    skia.draw_spike(size.w, size.h);

    // `renderer`/`framebuffer` borrows of `backend` end here; safe to submit.
    drop(framebuffer);

    backend.submit(Some(&[damage]))
}
