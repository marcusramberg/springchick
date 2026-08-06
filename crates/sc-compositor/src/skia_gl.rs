//! Skia-on-Smithay-GLES rendering for springchick.
//!
//! Reuses the M1 spike's Ganesh-GL shared-context approach with the M2 fix:
//! caches the Skia `Surface` + `BackendRenderTarget` keyed on (fboid, width, height),
//! recreating only on change.

use crate::render::ArrangeView;
use sc_catalog::AppEntry;
use sc_icons::IconPixels;
use sc_layout::{self, IconSlot, Layout};
use sc_shell_model::ShellModel;

use skia_safe::gpu::gl::{Format, FramebufferInfo, Interface};
use skia_safe::gpu::{
    backend_render_targets, direct_contexts, surfaces, DirectContext, SurfaceOrigin,
};
use skia_safe::image_filters;
use skia_safe::PathOp;
use skia_safe::{
    images, Color, ColorType, Font, FontMgr, FontStyle, Image, ImageInfo, Paint, PathBuilder,
    RRect, Rect, Surface, TextBlob, TileMode,
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
            self.gl_finish =
                Some(unsafe { std::mem::transmute::<*const c_void, GlFinish>(finish_ptr) });
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
        let image =
            images::raster_from_data(&info, skia_safe::Data::new_copy(&pixels.data), row_bytes)?;
        self.icon_images.insert(app_id.to_string(), image.clone());
        Some(image)
    }

    /// Draw the home screen (grid + dock + dots + bar). Grid icons are drawn
    /// from `grid_positions` (animated screen-space centers), so paging and
    /// reflow both play out as icon motion rather than a page-offset scroll.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_home(
        &mut self,
        width: i32,
        height: i32,
        page: usize,
        model: &ShellModel,
        icon_cache: &HashMap<String, IconPixels>,
        app_catalog: &HashMap<String, AppEntry>,
        flip_y: bool,
        pressed_app: Option<&str>,
        launching_app: Option<&str>,
        launching_elapsed: f32,
        arrange: Option<&ArrangeView>,
        grid_positions: &HashMap<String, (f32, f32)>,
        dock_positions: &HashMap<String, (f32, f32)>,
        top_inset: f32,
    ) {
        self.ensure_font();

        // Upload any icons we haven't uploaded yet.
        for (app_id, pixels) in icon_cache {
            if !self.icon_images.contains_key(app_id) {
                self.get_or_upload_icon(app_id, pixels);
            }
        }

        // Same surface acquisition as the overlay draws; `with_overlay_canvas`
        // isn't usable here because the body needs `&self.icon_images` and
        // `&self.font` while the canvas holds `&mut self.cached_surface`.
        if !self.ensure_surface(width, height) {
            return;
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

        // Grid icons: build the animated slot set once, in deterministic model
        // (page, slot) order — see `visible_grid_slots`. Reused below for arrange
        // badges so they track the sliding icons instead of the static layout.
        let anim_slots = visible_grid_slots(model, grid_positions, width as f32, height as f32);
        for slot in &anim_slots {
            draw_icon_slot(
                canvas,
                slot,
                &self.icon_images,
                &self.font,
                app_catalog,
                pressed_app == Some(slot.app_id.as_str()),
                launch_pulse(launching_app, launching_elapsed, &slot.app_id),
            );
        }

        // Dock and dots don't scroll with pages.
        let mut current_layout = sc_layout::compute(width as f32, height as f32, page, model);
        // Keep the arrange-mode Done button below a top exclusive-zone bar.
        current_layout.shift_done_below(top_inset);

        let dock_slots =
            visible_dock_slots(&current_layout, dock_positions, width as f32, height as f32);
        for slot in &dock_slots {
            draw_icon_slot(
                canvas,
                slot,
                &self.icon_images,
                &self.font,
                app_catalog,
                pressed_app == Some(slot.app_id.as_str()),
                launch_pulse(launching_app, launching_elapsed, &slot.app_id),
            );
        }

        // Draw page indicator dots.
        draw_dots(canvas, &current_layout, page);

        // Draw bar.
        draw_bar(canvas, &current_layout, 1.0);

        // Arrange mode: remove-badges, Done button, dock drop highlight, and
        // the lifted (dragged) icon on top of everything else.
        if let Some(view) = arrange {
            for slot in anim_slots.iter().chain(dock_slots.iter()) {
                draw_remove_badge(canvas, slot);
            }
            draw_done_button(canvas, &current_layout, &self.font);

            if view.over_dock {
                draw_dock_highlight(canvas, &current_layout);
            }

            if let (Some(app_id), Some(pos)) = (view.drag_app, view.drag_pos) {
                draw_drag_ghost(canvas, app_id, pos, &current_layout, &self.icon_images);
            }
        }

        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// Acquire (or reuse) the Skia surface wrapping the currently bound
    /// framebuffer. Returns false if the GL context or the wrap failed, in which
    /// case `cached_surface` must not be touched.
    fn ensure_surface(&mut self, width: i32, height: i32) -> bool {
        if width <= 0 || height <= 0 {
            return false;
        }
        if !self.ensure_context() {
            return false;
        }

        let fboid = self.current_fbo();
        let context = match self.context.as_mut() {
            Some(c) => c,
            None => return false,
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
            let render_target = backend_render_targets::make_gl((width, height), None, 8, fb_info);
            let Some(surface) = surfaces::wrap_backend_render_target(
                context,
                &render_target,
                SurfaceOrigin::BottomLeft,
                ColorType::RGBA8888,
                None,
                None,
            ) else {
                warn!("wrap_backend_render_target returned None");
                return false;
            };
            self.cached_surface = Some(CachedSurface {
                surface,
                fboid,
                width,
                height,
            });
        }
        true
    }

    /// Acquire the cached GL-backed Skia surface and run `f` against its canvas,
    /// applying the y-flip if needed and flushing afterward. Shared by the
    /// overlay draws so they don't each repeat the surface-acquire dance.
    fn with_overlay_canvas<F: FnOnce(&skia_safe::Canvas)>(
        &mut self,
        width: i32,
        height: i32,
        flip_y: bool,
        f: F,
    ) {
        if !self.ensure_surface(width, height) {
            return;
        }

        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        canvas.save();
        if flip_y {
            canvas.translate((0.0, height as f32));
            canvas.scale((1.0, -1.0));
        }
        f(canvas);
        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// Draw bar overlay on top of the app (return-home affordance).
    /// Draw the home-affordance bar over the app. `alpha` (0..=1) fades it out
    /// when an exclusive-zone layer surface (e.g. the OSK) covers the bottom.
    pub fn draw_bar_overlay(&mut self, width: i32, height: i32, alpha: f32, flip_y: bool) {
        if alpha <= 0.0 {
            return;
        }
        let model = ShellModel::default();
        let layout = sc_layout::compute(width as f32, height as f32, 0, &model);
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            draw_bar(canvas, &layout, alpha)
        });
    }

    /// Draw the volume OSD: a vertical bar on the right edge, in the top third,
    /// next to the physical rockers. `level` is 0.0..=~1.5 (1.0 = 100%);
    /// `alpha` fades it in/out.
    pub fn draw_osd_overlay(
        &mut self,
        width: i32,
        height: i32,
        level: f32,
        muted: bool,
        alpha: f32,
        flip_y: bool,
    ) {
        if alpha <= 0.0 {
            return;
        }
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            draw_volume_osd(canvas, width as f32, height as f32, level, muted, alpha);
        });
    }

    /// Blur what is already in the framebuffer, inside `rects`
    /// (ext-background-effect-v1). Call it after everything *behind* the
    /// blurring surface is drawn and before the surface itself.
    ///
    /// Works by snapshotting the framebuffer and drawing that snapshot straight
    /// back through a blur filter, clipped to the region. The snapshot lives in
    /// the surface's own coordinate space, so it is drawn with no y-flip and the
    /// clip rectangles are flipped into that space instead — flipping both would
    /// mirror the blurred content against the pixels it came from.
    pub fn blur_backdrop(
        &mut self,
        width: i32,
        height: i32,
        rects: &[crate::background_effect::BlurRect],
        sigma: f32,
        flip_y: bool,
    ) {
        if rects.is_empty() || sigma <= 0.0 {
            return;
        }
        if !self.ensure_surface(width, height) {
            return;
        }

        // Region → clip path: Add rects union, Subtract rects punch holes.
        let to_canvas = |r: &crate::background_effect::BlurRect| {
            let y = if flip_y {
                height as f32 - (r.y + r.h)
            } else {
                r.y
            };
            Rect::from_xywh(r.x, y, r.w, r.h)
        };
        let mut add = PathBuilder::new();
        let mut sub = PathBuilder::new();
        let mut any_add = false;
        let mut any_sub = false;
        for r in rects {
            if r.add {
                add.add_rect(to_canvas(r), None, None);
                any_add = true;
            } else {
                sub.add_rect(to_canvas(r), None, None);
                any_sub = true;
            }
        }
        if !any_add {
            return;
        }
        let add = add.detach();
        let clip = if any_sub {
            skia_safe::op(&add, &sub.detach(), PathOp::Difference).unwrap_or(add)
        } else {
            add
        };

        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let snapshot = surface.image_snapshot();
        let canvas = surface.canvas();

        let mut paint = Paint::default();
        paint.set_image_filter(image_filters::blur(
            (sigma, sigma),
            TileMode::Clamp,
            None,
            None,
        ));

        canvas.save();
        canvas.clip_path(&clip, None, true);
        canvas.draw_image(&snapshot, (0.0, 0.0), Some(&paint));
        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// Draw the touch indicator marks on top of everything (demo recordings).
    pub fn draw_touches_overlay(
        &mut self,
        width: i32,
        height: i32,
        marks: &[crate::touch_viz::TouchMark],
        flip_y: bool,
    ) {
        if marks.is_empty() {
            return;
        }
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            draw_touch_marks(canvas, marks);
        });
    }
}

/// Draw touch indicator marks: a soft filled disc under each held finger, with
/// a brighter rim, and an expanding stroked ring for each releasing contact.
/// Colors are a warm accent so contacts stand out over both light and dark UI.
fn draw_touch_marks(canvas: &skia_safe::Canvas, marks: &[crate::touch_viz::TouchMark]) {
    // Accent (springchick warm yellow), tuned for legibility on recordings.
    const R: u8 = 255;
    const G: u8 = 205;
    const B: u8 = 70;
    for m in marks {
        let a = |x: f32| (x * m.alpha).round().clamp(0.0, 255.0) as u8;
        if m.filled {
            // Solid fingertip fill plus a crisper rim for definition.
            let mut fill = Paint::default();
            fill.set_anti_alias(true);
            fill.set_color(Color::from_argb(a(255.0), R, G, B));
            canvas.draw_circle((m.x, m.y), m.radius, &fill);

            let mut rim = Paint::default();
            rim.set_anti_alias(true);
            rim.set_stroke(true);
            rim.set_stroke_width(m.radius * 0.10);
            rim.set_color(Color::from_argb(a(255.0), 255, 235, 170));
            canvas.draw_circle((m.x, m.y), m.radius, &rim);
        } else {
            // Expanding release ring.
            let mut ring = Paint::default();
            ring.set_anti_alias(true);
            ring.set_stroke(true);
            ring.set_stroke_width(m.radius * 0.12);
            ring.set_color(Color::from_argb(a(255.0), R, G, B));
            canvas.draw_circle((m.x, m.y), m.radius, &ring);
        }
    }
}

/// Draw the volume OSD track + fill. Geometry is a slim vertical pill hugging
/// the right edge, vertically centered in the top third of the screen.
fn draw_volume_osd(
    canvas: &skia_safe::Canvas,
    width: f32,
    height: f32,
    level: f32,
    muted: bool,
    alpha: f32,
) {
    let a = |x: f32| (x * alpha).round().clamp(0.0, 255.0) as u8;

    let track_w = (width * 0.020).clamp(10.0, 22.0);
    let track_h = height * 0.24;
    let margin = width * 0.03;
    let x = width - margin - track_w;
    let y = height * 0.10; // top third, a little below the top edge
    let radius = track_w / 2.0;

    // Track (dim background).
    let mut track = Paint::default();
    track.set_anti_alias(true);
    track.set_color(Color::from_argb(a(90.0), 40, 40, 40));
    let track_rect = Rect::new(x, y, x + track_w, y + track_h);
    canvas.draw_rrect(RRect::new_rect_xy(track_rect, radius, radius), &track);

    // Fill grows from the bottom. Over 100% is tinted; muted is greyed.
    let frac = level.clamp(0.0, 1.0);
    let fill_h = track_h * frac;
    let fill_top = y + track_h - fill_h;
    let (r, g, b) = if muted {
        (110, 110, 120)
    } else if level > 1.0 {
        (255, 170, 60) // overdriven past 100%
    } else {
        (255, 255, 255)
    };
    let mut fill = Paint::default();
    fill.set_anti_alias(true);
    fill.set_color(Color::from_argb(a(235.0), r, g, b));
    if fill_h > 0.5 {
        let fill_rect = Rect::new(x, fill_top, x + track_w, y + track_h);
        canvas.draw_rrect(RRect::new_rect_xy(fill_rect, radius, radius), &fill);
    }

    // A slash across the pill signals muted.
    if muted {
        let mut slash = Paint::default();
        slash.set_anti_alias(true);
        slash.set_color(Color::from_argb(a(235.0), 235, 90, 90));
        slash.set_stroke_width(track_w * 0.22);
        slash.set_stroke(true);
        let cx = x + track_w / 2.0;
        let cy = y + track_h / 2.0;
        let r = track_w * 1.1;
        canvas.draw_line((cx - r, cy - r), (cx + r, cy + r), &slash);
    }
}

impl Default for SkiaGl {
    fn default() -> Self {
        Self::new()
    }
}

// --- Free functions for drawing (avoids borrow issues with &mut self + canvas) ---

/// Seconds-since-launch for `app_id` iff it is the currently launching app,
/// else `None`. Feeds the breathing pulse phase in [`draw_icon_slot`].
fn launch_pulse(launching_app: Option<&str>, elapsed: f32, app_id: &str) -> Option<f32> {
    (launching_app == Some(app_id)).then_some(elapsed)
}

fn draw_icon_slot(
    canvas: &skia_safe::Canvas,
    slot: &IconSlot,
    icon_images: &HashMap<String, Image>,
    font: &Option<Font>,
    app_catalog: &HashMap<String, AppEntry>,
    pressed: bool,
    launching: Option<f32>,
) {
    // Launch pulse: while the app is spawning (before its window maps) the icon
    // breathes — a halo that swells and fades  — so the tap
    // has visible, ongoing feedback instead of a dead icon. Takes over from the
    // static press highlight.
    let icon_scale = 1.0;
    if let Some(elapsed) = launching {
        // ~0.7 Hz breath: halo alpha pulses in place
        let phase = sc_anim::pulse(elapsed, 4.4);
        let alpha = (40.0 + 70.0 * phase) as u8;
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::from_argb(alpha, 255, 255, 255));
        let pad = slot.icon_rect.w * 0.1;
        let rect = Rect::new(
            slot.icon_rect.x - pad,
            slot.icon_rect.y - pad,
            slot.icon_rect.x + slot.icon_rect.w + pad,
            slot.icon_rect.y + slot.icon_rect.h + pad,
        );
        let rrect = RRect::new_rect_xy(rect, 28.0, 28.0);
        canvas.draw_rrect(rrect, &paint);
    } else if pressed {
        // Press highlight: a translucent rounded backing behind the icon so a
        // tap reads as "launching" before the zoom animation begins.
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::from_argb(60, 255, 255, 255));
        let pad = slot.icon_rect.w * 0.12;
        let rect = Rect::new(
            slot.icon_rect.x - pad,
            slot.icon_rect.y - pad,
            slot.icon_rect.x + slot.icon_rect.w + pad,
            slot.icon_rect.y + slot.icon_rect.h + pad,
        );
        let rrect = RRect::new_rect_xy(rect, 24.0, 24.0);
        canvas.draw_rrect(rrect, &paint);
    }

    // Icon rect, grown about its center by the launch pulse scale.
    let cx = slot.icon_rect.x + slot.icon_rect.w / 2.0;
    let cy = slot.icon_rect.y + slot.icon_rect.h / 2.0;
    let hw = slot.icon_rect.w / 2.0 * icon_scale;
    let hh = slot.icon_rect.h / 2.0 * icon_scale;
    let icon_dst = Rect::new(cx - hw, cy - hh, cx + hw, cy + hh);

    // Draw icon image.
    if let Some(image) = icon_images.get(&slot.app_id) {
        canvas.draw_image_rect(image, None, icon_dst, &Paint::default());
    } else {
        // Placeholder rect.
        let mut paint = Paint::default();
        paint.set_color(Color::from_argb(255, 80, 80, 100));
        let rrect = RRect::new_rect_xy(icon_dst, 20.0, 20.0);
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

fn draw_bar(canvas: &skia_safe::Canvas, layout: &Layout, alpha: f32) {
    let pill = sc_layout::pill_in_bar(layout.bar_rect);

    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    let a = (180.0 * alpha.clamp(0.0, 1.0)).round() as u8;
    paint.set_color(Color::from_argb(a, 255, 255, 255));

    let rect = Rect::new(pill.x, pill.y, pill.x + pill.w, pill.y + pill.h);
    let radius = pill.h / 2.0;
    let rrect = RRect::new_rect_xy(rect, radius, radius);
    canvas.draw_rrect(rrect, &paint);
}

/// Arrange-mode "remove" badge: a filled red circle with a white '-' glyph,
/// drawn at the slot's precomputed `badge_rect`.
fn draw_remove_badge(canvas: &skia_safe::Canvas, slot: &IconSlot) {
    let r = &slot.badge_rect;
    let cx = r.center_x();
    let cy = r.center_y();
    let radius = r.w.min(r.h) / 2.0;

    let mut fill = Paint::default();
    fill.set_anti_alias(true);
    fill.set_color(Color::from_argb(255, 220, 50, 50));
    canvas.draw_circle((cx, cy), radius, &fill);

    let mut stroke = Paint::default();
    stroke.set_anti_alias(true);
    stroke.set_color(Color::WHITE);
    stroke.set_stroke_width((radius * 0.22).max(1.5));
    let half = radius * 0.5;
    canvas.draw_line((cx - half, cy), (cx + half, cy), &stroke);
}

/// Arrange-mode "Done" button: a translucent rounded rect with centered text.
fn draw_done_button(canvas: &skia_safe::Canvas, layout: &Layout, font: &Option<Font>) {
    let r = &layout.done_button;
    let rect = Rect::new(r.x, r.y, r.x + r.w, r.y + r.h);
    let radius = (r.h * 0.4).max(4.0);

    let mut fill = Paint::default();
    fill.set_anti_alias(true);
    fill.set_color(Color::from_argb(200, 40, 120, 220));
    let rrect = RRect::new_rect_xy(rect, radius, radius);
    canvas.draw_rrect(rrect, &fill);

    if let Some(f) = font {
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::WHITE);
        if let Some(blob) = TextBlob::new("Done", f) {
            let text_width = f.measure_str("Done", None).0;
            let x = r.x + (r.w - text_width) / 2.0;
            let y = r.y + r.h * 0.7;
            canvas.draw_text_blob(&blob, (x, y), &paint);
        }
    }
}

/// Highlight the dock zone as a drop target while a dragged icon hovers over
/// it (semi-transparent fill).
fn draw_dock_highlight(canvas: &skia_safe::Canvas, layout: &Layout) {
    let z = &layout.dock_zone;
    let rect = Rect::new(z.x, z.y, z.x + z.w, z.y + z.h);

    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_argb(70, 255, 255, 255));
    canvas.draw_rect(rect, &paint);
}

/// The lifted (dragged) icon, drawn last so it floats above everything else.
/// Looked up in `layout` (grid + dock) for its natural icon size, scaled up
/// ~1.2x, centered on the live drag position. Silently skipped if the icon
/// isn't in the upload cache yet — never worth a panic mid-drag.
fn draw_drag_ghost(
    canvas: &skia_safe::Canvas,
    app_id: &str,
    pos: (f32, f32),
    layout: &Layout,
    icon_images: &HashMap<String, Image>,
) {
    let Some(image) = icon_images.get(app_id) else {
        return;
    };
    let base_size = layout
        .grid
        .iter()
        .chain(layout.dock.iter())
        .find(|s| s.app_id == app_id)
        .map(|s| s.icon_rect.w)
        .unwrap_or(64.0);
    let size = base_size * 1.2;
    let (cx, cy) = pos;
    let dst = Rect::new(
        cx - size / 2.0,
        cy - size / 2.0,
        cx + size / 2.0,
        cy + size / 2.0,
    );
    canvas.draw_image_rect(image, None, dst, &Paint::default());
}

/// Grid icon slots to draw, at their animated screen positions, in deterministic
/// model (page, slot) order.
///
/// Iterating `grid_positions` (a `HashMap` rebuilt every frame with a fresh random
/// seed) would draw in a different z-order each frame, making overlapping
/// labels/icons z-fight and shimmer. Walking `model.pages` fixes the order to the
/// stable page/slot layout the old per-page renderer used. Off-screen icons are
/// culled.
pub(crate) fn visible_grid_slots(
    model: &ShellModel,
    grid_positions: &HashMap<String, (f32, f32)>,
    width: f32,
    height: f32,
) -> Vec<IconSlot> {
    let mut out = Vec::new();
    for page in &model.pages {
        for app in page {
            if let Some((sx, sy)) = grid_positions.get(app) {
                if *sx < -width * 0.3 || *sx > width * 1.3 {
                    continue;
                }
                out.push(sc_layout::slot_at_center(
                    app.clone(),
                    *sx,
                    *sy,
                    width,
                    height,
                ));
            }
        }
    }
    out
}

/// Dock icon slots positioned from animated `dock_positions` (falling back to
/// the static layout center for a not-yet-seeded app), so the dock reflows.
pub(crate) fn visible_dock_slots(
    layout: &sc_layout::Layout,
    dock_positions: &HashMap<String, (f32, f32)>,
    width: f32,
    height: f32,
) -> Vec<sc_layout::IconSlot> {
    // Skip apps absent from `dock_positions` (mirrors `visible_grid_slots`): a
    // dock icon being dragged is dropped from `dock_anim`, so it renders only as
    // the ghost, not doubled in its dock cell.
    layout
        .dock
        .iter()
        .filter_map(|slot| {
            dock_positions.get(&slot.app_id).map(|&(cx, cy)| {
                sc_layout::slot_at_center(slot.app_id.clone(), cx, cy, width, height)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_shell_model::ShellModel;
    use std::collections::HashMap;

    fn on_screen_positions(apps: &[&str]) -> HashMap<String, (f32, f32)> {
        // Insert in a different order than `apps` so a HashMap-order bug would
        // surface; values are all comfortably on-screen at 1224x2700.
        let mut gp = HashMap::new();
        for (i, a) in apps.iter().enumerate().rev() {
            gp.insert((*a).to_string(), (100.0 + i as f32 * 50.0, 400.0));
        }
        gp
    }

    #[test]
    fn visible_grid_slots_follow_model_order_deterministically() {
        let m = ShellModel {
            pages: vec![vec!["a".into(), "b".into(), "c".into()]],
            ..Default::default()
        };
        // Rebuild the map many times (fresh RandomState each) and confirm the
        // draw order is always the model order, never the HashMap's.
        for _ in 0..25 {
            let gp = on_screen_positions(&["a", "b", "c"]);
            let order: Vec<String> = visible_grid_slots(&m, &gp, 1224.0, 2700.0)
                .iter()
                .map(|s| s.app_id.clone())
                .collect();
            assert_eq!(order, vec!["a", "b", "c"]);
        }
    }

    #[test]
    fn visible_grid_slots_culls_offscreen() {
        let m = ShellModel {
            pages: vec![vec!["on".into(), "off".into()]],
            ..Default::default()
        };
        let mut gp = HashMap::new();
        gp.insert("on".to_string(), (600.0, 400.0));
        gp.insert("off".to_string(), (1224.0 * 2.0, 400.0)); // far right, culled
        let slots = visible_grid_slots(&m, &gp, 1224.0, 2700.0);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].app_id, "on");
    }
}
