//! Skia-on-Smithay-GLES rendering for springchick.
//!
//! Reuses the M1 spike's Ganesh-GL shared-context approach with the M2 fix:
//! caches the Skia `Surface` + `BackendRenderTarget` keyed on (fboid, width, height),
//! recreating only on change.

use sc_config::AppEntry;
use sc_icons::IconPixels;
use sc_layout::{self, IconSlot, Layout};
use sc_shell_model::ShellModel;

use skia_safe::gpu::gl::{Format, FramebufferInfo, Interface};
use skia_safe::gpu::{backend_render_targets, direct_contexts, surfaces, DirectContext, SurfaceOrigin};
use skia_safe::{
    Color, ColorType, Font, FontMgr, FontStyle, Image, ImageInfo, Paint, RRect, Rect, Surface,
    TextBlob, images,
};

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::c_int;

use tracing::{debug, warn};

const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
type GlGetIntegerv = unsafe extern "system" fn(pname: u32, params: *mut c_int);
type GlFinish = unsafe extern "system" fn();

/// Skia Ganesh-GL renderer bound to Smithay's existing GLES/EGL context.
pub struct SkiaGl {
    context: Option<DirectContext>,
    gl_get_integerv: Option<GlGetIntegerv>,
    gl_finish: Option<GlFinish>,
    setup_failed: bool,
    cached_surface: Option<CachedSurface>,
    icon_images: HashMap<String, Image>,
    font: Option<Font>,
}

struct CachedSurface {
    surface: Surface,
    fboid: u32,
    width: i32,
    height: i32,
}

impl SkiaGl {
    pub fn new() -> Self {
        Self {
            context: None,
            gl_get_integerv: None,
            gl_finish: None,
            setup_failed: false,
            cached_surface: None,
            icon_images: HashMap::new(),
            font: None,
        }
    }

    fn ensure_context(&mut self) -> bool {
        if self.context.is_some() {
            return true;
        }
        if self.setup_failed {
            return false;
        }

        let loader = |symbol: &str| -> *const c_void {
            unsafe { smithay::backend::egl::get_proc_address(symbol) }
        };

        let interface = Interface::new_native().or_else(|| {
            debug!("Interface::new_native() None; using EGL proc loader");
            Interface::new_load_with(loader)
        });
        let Some(interface) = interface else {
            warn!("failed to build Skia GL Interface");
            self.setup_failed = true;
            return false;
        };

        let Some(context) = direct_contexts::make_gl(interface, None) else {
            warn!("make_gl returned None");
            self.setup_failed = true;
            return false;
        };

        let getter_ptr = loader("glGetIntegerv");
        if getter_ptr.is_null() {
            warn!("could not load glGetIntegerv");
            self.setup_failed = true;
            return false;
        }
        let gl_get_integerv: GlGetIntegerv = unsafe { std::mem::transmute(getter_ptr) };

        let finish_ptr = loader("glFinish");
        if !finish_ptr.is_null() {
            self.gl_finish = Some(unsafe { std::mem::transmute::<*const c_void, GlFinish>(finish_ptr) });
        }

        self.context = Some(context);
        self.gl_get_integerv = Some(gl_get_integerv);
        debug!("Skia Ganesh-GL context initialized");
        true
    }

    /// Block until all submitted GL commands (smithay + Skia) have completed.
    /// Used by the DRM backend to fence the frame before a page-flip so scanout
    /// never shows a partially-rendered buffer (tearing).
    pub fn finish_gpu(&self) {
        if let Some(finish) = self.gl_finish {
            unsafe { finish() };
        }
    }

    fn current_fbo(&self) -> u32 {
        let Some(get) = self.gl_get_integerv else {
            return 0;
        };
        let mut fbo: c_int = 0;
        unsafe { get(GL_FRAMEBUFFER_BINDING, &mut fbo as *mut c_int) };
        fbo.max(0) as u32
    }

    fn ensure_font(&mut self) {
        if self.font.is_none() {
            let mgr = FontMgr::default();
            let typeface = mgr
                .match_family_style("sans-serif", FontStyle::normal())
                .unwrap_or_else(|| mgr.legacy_make_typeface(None, FontStyle::normal()).unwrap());
            self.font = Some(Font::from_typeface(typeface, 28.0));
        }
    }

    fn get_or_upload_icon(&mut self, app_id: &str, pixels: &IconPixels) -> Option<Image> {
        if let Some(img) = self.icon_images.get(app_id) {
            return Some(img.clone());
        }

        let info = ImageInfo::new(
            (pixels.width as i32, pixels.height as i32),
            ColorType::RGBA8888,
            skia_safe::AlphaType::Unpremul,
            None,
        );
        let row_bytes = pixels.width as usize * 4;
        let image = images::raster_from_data(&info, skia_safe::Data::new_copy(&pixels.data), row_bytes)?;
        self.icon_images.insert(app_id.to_string(), image.clone());
        Some(image)
    }

    /// Draw the home screen (grid + dock + dots + bar).
    /// Draw the home screen. `page_offset` is a fractional pixel offset for smooth swiping
    /// (0 = page aligned, negative = swiping left to next page).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_home(
        &mut self,
        width: i32,
        height: i32,
        page: usize,
        page_offset: f32,
        model: &ShellModel,
        icon_cache: &HashMap<String, IconPixels>,
        app_catalog: &HashMap<String, AppEntry>,
        flip_y: bool,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }
        if !self.ensure_context() {
            return;
        }
        self.ensure_font();

        // Upload any icons we haven't uploaded yet.
        for (app_id, pixels) in icon_cache {
            if !self.icon_images.contains_key(app_id) {
                self.get_or_upload_icon(app_id, pixels);
            }
        }

        // Acquire surface.
        let fboid = self.current_fbo();
        let context = match self.context.as_mut() {
            Some(c) => c,
            None => return,
        };
        context.reset(None);

        let needs_recreate = match &self.cached_surface {
            Some(c) => c.fboid != fboid || c.width != width || c.height != height,
            None => true,
        };
        if needs_recreate {
            let fb_info = FramebufferInfo {
                fboid,
                format: Format::RGBA8.into(),
                ..Default::default()
            };
            let render_target =
                backend_render_targets::make_gl((width, height), None, 8, fb_info);
            let Some(surface) = surfaces::wrap_backend_render_target(
                context,
                &render_target,
                SurfaceOrigin::BottomLeft,
                ColorType::RGBA8888,
                None,
                None,
            ) else {
                warn!("wrap_backend_render_target returned None");
                return;
            };
            self.cached_surface = Some(CachedSurface {
                surface,
                fboid,
                width,
                height,
            });
        }

        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        // Panel-orientation flip: the DRM/GBM scanout buffer has the opposite
        // Y-origin from Skia's BottomLeft surface, so the home/bar render
        // upside-down on the panel. Mirror vertically for the DRM path.
        // Save/restore so the cached surface's matrix doesn't accumulate.
        canvas.save();
        if flip_y {
            canvas.translate((0.0, height as f32));
            canvas.scale((1.0, -1.0));
        }

        let page_count = model.pages.len().max(1);

        // Draw current page and adjacent page(s) for smooth swiping.
        // page_offset: negative = swiping left (toward next page)
        let pages_to_draw: Vec<(usize, f32)> = {
            let mut pages = vec![(page, page_offset)];
            // If offset is negative (swiping left), draw next page to the right.
            if page_offset < 0.0 && page + 1 < page_count {
                pages.push((page + 1, page_offset + width as f32));
            }
            // If offset is positive (swiping right), draw prev page to the left.
            if page_offset > 0.0 && page > 0 {
                pages.push((page - 1, page_offset - width as f32));
            }
            pages
        };

        for (pg, offset_x) in &pages_to_draw {
            let layout = sc_layout::compute(width as f32, height as f32, *pg, model);

            canvas.save();
            canvas.translate((*offset_x, 0.0));

            // Draw grid icons.
            for slot in &layout.grid {
                draw_icon_slot(canvas, slot, &self.icon_images, &self.font, app_catalog);
            }

            canvas.restore();
        }

        // Dock and dots don't scroll with pages.
        let current_layout = sc_layout::compute(width as f32, height as f32, page, model);

        for slot in &current_layout.dock {
            draw_icon_slot(canvas, slot, &self.icon_images, &self.font, app_catalog);
        }

        // Draw page indicator dots.
        draw_dots(canvas, &current_layout, page);

        // Draw bar.
        draw_bar(canvas, &current_layout);

        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// Draw bar overlay on top of the app (return-home affordance).
    pub fn draw_bar_overlay(&mut self, width: i32, height: i32, flip_y: bool) {
        if width <= 0 || height <= 0 {
            return;
        }
        if !self.ensure_context() {
            return;
        }

        let model = ShellModel::default();
        let layout = sc_layout::compute(width as f32, height as f32, 0, &model);

        // Acquire surface.
        let fboid = self.current_fbo();
        let context = match self.context.as_mut() {
            Some(c) => c,
            None => return,
        };
        context.reset(None);

        let needs_recreate = match &self.cached_surface {
            Some(c) => c.fboid != fboid || c.width != width || c.height != height,
            None => true,
        };
        if needs_recreate {
            let fb_info = FramebufferInfo {
                fboid,
                format: Format::RGBA8.into(),
                ..Default::default()
            };
            let render_target =
                backend_render_targets::make_gl((width, height), None, 8, fb_info);
            let Some(surface) = surfaces::wrap_backend_render_target(
                context,
                &render_target,
                SurfaceOrigin::BottomLeft,
                ColorType::RGBA8888,
                None,
                None,
            ) else {
                warn!("wrap_backend_render_target returned None");
                return;
            };
            self.cached_surface = Some(CachedSurface {
                surface,
                fboid,
                width,
                height,
            });
        }

        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        canvas.save();
        if flip_y {
            canvas.translate((0.0, height as f32));
            canvas.scale((1.0, -1.0));
        }
        draw_bar(canvas, &layout);
        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }
}

impl Default for SkiaGl {
    fn default() -> Self {
        Self::new()
    }
}

// --- Free functions for drawing (avoids borrow issues with &mut self + canvas) ---

fn draw_icon_slot(
    canvas: &skia_safe::Canvas,
    slot: &IconSlot,
    icon_images: &HashMap<String, Image>,
    font: &Option<Font>,
    app_catalog: &HashMap<String, AppEntry>,
) {
    // Draw icon image.
    if let Some(image) = icon_images.get(&slot.app_id) {
        let dst = Rect::new(
            slot.icon_rect.x,
            slot.icon_rect.y,
            slot.icon_rect.x + slot.icon_rect.w,
            slot.icon_rect.y + slot.icon_rect.h,
        );
        canvas.draw_image_rect(image, None, dst, &Paint::default());
    } else {
        // Placeholder rect.
        let mut paint = Paint::default();
        paint.set_color(Color::from_argb(255, 80, 80, 100));
        let rect = Rect::new(
            slot.icon_rect.x,
            slot.icon_rect.y,
            slot.icon_rect.x + slot.icon_rect.w,
            slot.icon_rect.y + slot.icon_rect.h,
        );
        let rrect = RRect::new_rect_xy(rect, 20.0, 20.0);
        canvas.draw_rrect(rrect, &paint);
    }

    // Draw label.
    if let Some(entry) = app_catalog.get(&slot.app_id) {
        if let Some(f) = font {
            let mut paint = Paint::default();
            paint.set_color(Color::WHITE);
            if let Some(blob) = TextBlob::new(&entry.name, f) {
                let text_width = f.measure_str(&entry.name, None).0;
                let x = slot.label_rect.x + (slot.label_rect.w - text_width) / 2.0;
                let y = slot.label_rect.y + slot.label_rect.h * 0.75;
                canvas.draw_text_blob(&blob, (x, y), &paint);
            }
        }
    }
}

fn draw_dots(canvas: &skia_safe::Canvas, layout: &Layout, current_page: usize) {
    let dot_radius = 6.0_f32;
    let dot_spacing = 20.0_f32;
    let total_width = layout.page_count as f32 * dot_spacing;
    let start_x = layout.dots_rect.x + (layout.dots_rect.w - total_width) / 2.0;
    let cy = layout.dots_rect.y + layout.dots_rect.h / 2.0;

    for i in 0..layout.page_count {
        let cx = start_x + i as f32 * dot_spacing + dot_spacing / 2.0;
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        if i == current_page {
            paint.set_color(Color::WHITE);
        } else {
            paint.set_color(Color::from_argb(128, 255, 255, 255));
        }
        canvas.draw_circle((cx, cy), dot_radius, &paint);
    }
}

fn draw_bar(canvas: &skia_safe::Canvas, layout: &Layout) {
    let bar = &layout.bar_rect;
    let pill_w = bar.w * 0.35;
    let pill_h = 8.0_f32;
    let pill_x = bar.x + (bar.w - pill_w) / 2.0;
    let pill_y = bar.y + (bar.h - pill_h) / 2.0;

    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_argb(180, 255, 255, 255));

    let rect = Rect::new(pill_x, pill_y, pill_x + pill_w, pill_y + pill_h);
    let rrect = RRect::new_rect_xy(rect, pill_h / 2.0, pill_h / 2.0);
    canvas.draw_rrect(rrect, &paint);
}
