//! Skia-on-Smithay-GLES spike (Foundation Task 8) — the architecture go/no-go gate.
//!
//! This module proves the single-graphics-API claim the whole springchick
//! architecture rests on: Skia's Ganesh **GL** backend can draw directly into the
//! *same* GLES/EGL context that Smithay's winit backend already renders with. No
//! second GL context, no dmabuf import, no cross-API sync — Skia and Smithay share
//! one context and one framebuffer.
//!
//! ## How the context handoff is wired (read this before Milestone 2)
//!
//! 1. **GL function loading / `Interface`.** Skia needs a `gpu::gl::Interface`
//!    (a table of GL entry points). We first try [`Interface::new_native`], which
//!    asks Skia to assemble the interface from whatever GL is current. On Mesa/EGL
//!    that frequently returns `None`, so we fall back to
//!    [`Interface::new_load_with`] driven by Smithay's own EGL proc loader,
//!    [`smithay::backend::egl::get_proc_address`]. That loader calls
//!    `eglGetProcAddress` under the hood and is valid *only while an EGL context is
//!    current* — which is exactly the case during `render_frame` after
//!    `backend.bind()`. This is why `SkiaGl` is lazily initialized on the first
//!    `draw_spike` call rather than in `new()`: at `new()` time no context is
//!    current yet.
//!
//! 2. **`DirectContext`.** Built once via
//!    [`direct_contexts::make_gl`] from that interface and cached for the lifetime
//!    of the process. It wraps Skia's GPU command stream targeting the shared GL
//!    context.
//!
//! 3. **Retrieving the bound FBO id.** Smithay may render into FBO 0 (the EGL
//!    window surface) or an internal FBO; rather than assume, we query the live
//!    `GL_FRAMEBUFFER_BINDING` with a `glGetIntegerv` pointer obtained through the
//!    *same* EGL proc loader. Whatever is currently bound is what Skia wraps, so
//!    Skia always draws into the exact framebuffer Smithay is about to present.
//!
//! 4. **Wrapping the framebuffer as a Skia `Surface`.** We build a
//!    [`gl::FramebufferInfo`] (`fboid` = the queried id, `format` = `GL_RGBA8`),
//!    wrap it in a [`BackendRenderTarget`], then
//!    [`surfaces::wrap_backend_render_target`] with:
//!      - origin `SurfaceOrigin::BottomLeft` — GL's framebuffer origin is
//!        bottom-left, and Smithay presents with `Transform::Flipped180`, so
//!        bottom-left keeps Skia's content the same way up as Smithay's.
//!      - color type `ColorType::RGBA8888`.
//!        No `ColorSpace` / `SurfaceProps` (both `None`) — linear sRGB is fine for the
//!        spike.
//!
//! 5. **GL state hygiene (gotcha).** Smithay's `GlesRenderer` mutates global GL
//!    state every frame (bound program, blend, VAO, etc.). Skia caches its view of
//!    that state, so before drawing we call [`DirectContext::reset`] with `None` to
//!    invalidate *all* cached state and force Skia to re-establish what it needs.
//!    After drawing we `flush_and_submit()` so the rrect is in the framebuffer
//!    before Smithay's `submit()` runs `eglSwapBuffers`. Skia leaves GL state
//!    changed on return; that is fine here because the only thing Smithay does
//!    after the Skia draw this frame is swap buffers.
//!
//! The draw itself is the spike payload: one iOS-style blurred, rounded rectangle
//! (`MaskFilter::blur(BlurStyle::Normal, sigma)`).
//!
//! Because the CI/dev shell has no display, this code cannot be visually verified
//! here. It is structured to fail *only* on a missing display, not in the
//! Skia/GL binding logic. The human does the visual go/no-go on a real desktop.

use skia_safe::gpu::gl::{Format, FramebufferInfo, Interface};
use skia_safe::gpu::{
    backend_render_targets, direct_contexts, surfaces, DirectContext, SurfaceOrigin,
};
use skia_safe::{BlurStyle, Color, ColorType, MaskFilter, Paint, RRect, Rect};

use std::ffi::c_void;
use std::os::raw::c_int;

use tracing::{debug, warn};

/// `GL_FRAMEBUFFER_BINDING` — query the currently bound draw framebuffer.
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;

/// `glGetIntegerv` signature (linux GL uses the C/`system` ABI).
type GlGetIntegerv = unsafe extern "system" fn(pname: u32, params: *mut c_int);

/// Skia Ganesh-GL renderer bound to Smithay's existing GLES/EGL context.
///
/// Created once and reused across frames. The expensive GL objects
/// (`Interface`, `DirectContext`, `glGetIntegerv` pointer) are built lazily on the
/// first [`draw_spike`](Self::draw_spike), when an EGL context is guaranteed
/// current.
pub struct SkiaGl {
    context: Option<DirectContext>,
    gl_get_integerv: Option<GlGetIntegerv>,
    /// Whether we already logged the one-time setup failure, to avoid spamming.
    setup_failed: bool,
}

impl SkiaGl {
    /// Construct without touching GL. Real initialization is deferred to the first
    /// [`draw_spike`](Self::draw_spike) call, where an EGL context is current.
    pub fn new() -> Self {
        Self {
            context: None,
            gl_get_integerv: None,
            setup_failed: false,
        }
    }

    /// Lazily build the GL `Interface` + `DirectContext` from the *current* EGL
    /// context. Returns `false` if the context could not be established.
    fn ensure_context(&mut self) -> bool {
        if self.context.is_some() {
            return true;
        }
        if self.setup_failed {
            return false;
        }

        // Proc loader backed by Smithay's EGL. Valid only with a current context.
        let loader = |symbol: &str| -> *const c_void {
            // SAFETY: called from render_frame after backend.bind(), so an EGL
            // context is current — the documented precondition.
            unsafe { smithay::backend::egl::get_proc_address(symbol) }
        };

        // 1. Build the GL interface: native first, EGL proc loader as fallback.
        let interface = Interface::new_native()
            .inspect(|_| {
                tracing::debug!("Skia GL Interface built via new_native");
            })
            .or_else(|| {
                debug!("Skia GL Interface::new_native() returned None; using EGL proc loader");
                Interface::new_load_with(loader)
            });
        let Some(interface) = interface else {
            warn!("failed to build Skia GL Interface from current context");
            self.setup_failed = true;
            return false;
        };

        // 2. Create the Ganesh DirectContext on the shared GL context.
        let Some(context) = direct_contexts::make_gl(interface, None) else {
            warn!("skia_safe::gpu::direct_contexts::make_gl returned None");
            self.setup_failed = true;
            return false;
        };

        // 3. Cache a glGetIntegerv pointer for querying the bound FBO each frame.
        let getter_ptr = loader("glGetIntegerv");
        if getter_ptr.is_null() {
            warn!("could not load glGetIntegerv via EGL proc loader");
            self.setup_failed = true;
            return false;
        }
        // SAFETY: glGetIntegerv has the C ABI we declared in `GlGetIntegerv`.
        let gl_get_integerv: GlGetIntegerv = unsafe { std::mem::transmute(getter_ptr) };

        self.context = Some(context);
        self.gl_get_integerv = Some(gl_get_integerv);
        debug!("Skia Ganesh-GL context initialized on Smithay's GLES/EGL context");
        true
    }

    /// Query the currently bound draw framebuffer id.
    ///
    /// An EGL context must be current and the GL proc loader initialized;
    /// returns 0 otherwise.
    fn current_fbo(&self) -> u32 {
        let Some(get) = self.gl_get_integerv else {
            return 0;
        };
        let mut fbo: c_int = 0;
        // SAFETY: GL_FRAMEBUFFER_BINDING writes exactly one integer.
        unsafe { get(GL_FRAMEBUFFER_BINDING, &mut fbo as *mut c_int) };
        fbo.max(0) as u32
    }

    /// Draw the spike: one blurred, rounded rectangle into the framebuffer Smithay
    /// is about to present. Must be called while Smithay's target framebuffer is
    /// bound and the EGL context is current. Errors are logged and swallowed so a
    /// Skia hiccup never tears down the compositor loop.
    pub fn draw_spike(&mut self, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        if !self.ensure_context() {
            return;
        }

        let fboid = self.current_fbo();

        let context = match self.context.as_mut() {
            Some(c) => c,
            None => return,
        };

        // Smithay just mutated GL state; drop Skia's cached view of it.
        context.reset(None);

        // SPIKE ONLY: the BackendRenderTarget + Surface are rebuilt every frame for
        // simplicity. M2 must cache these keyed on (fboid, width, height) and only
        // recreate when the fbo id or dimensions change.
        let fb_info = FramebufferInfo {
            fboid,
            // ASSUMPTION: the EGL surface is RGBA8. This may need to be BGRA8 or
            // queried on the phone target — revisit during M2/device bring-up.
            format: Format::RGBA8.into(),
            ..Default::default()
        };

        let render_target = backend_render_targets::make_gl(
            (width, height),
            None, // sample count: single-sampled
            // ASSUMPTION: framebuffer has 8 stencil bits. Not queried here. The blur spike
            // uses no stencil clips so this is harmless now, but M2 must query GL_STENCIL_BITS
            // (or guarantee the EGL config requests stencil) before doing stencil clipping.
            8,
            fb_info,
        );

        let Some(mut surface) = surfaces::wrap_backend_render_target(
            context,
            &render_target,
            SurfaceOrigin::BottomLeft,
            ColorType::RGBA8888,
            None,
            None,
        ) else {
            warn!("wrap_backend_render_target returned None (fboid={fboid})");
            return;
        };

        // --- spike payload: an iOS-style blurred rounded rectangle ---
        let canvas = surface.canvas();

        let inset = (width.min(height) as f32) * 0.18;
        let rect = Rect::new(
            inset,
            inset,
            width as f32 - inset,
            height as f32 - inset,
        );
        let rrect = RRect::new_rect_xy(rect, 48.0, 48.0);

        let sigma = (width.min(height) as f32) * 0.02;
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::from_argb(0xCC, 0x4F, 0xC3, 0xF7));
        if let Some(blur) = MaskFilter::blur(BlurStyle::Normal, sigma, false) {
            paint.set_mask_filter(blur);
        }

        canvas.draw_rrect(rrect, &paint);

        // Make sure the draw lands in the framebuffer before Smithay swaps buffers.
        context.flush_and_submit();
    }
}

impl Default for SkiaGl {
    fn default() -> Self {
        Self::new()
    }
}
