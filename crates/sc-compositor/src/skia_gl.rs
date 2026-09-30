//! Skia (Ganesh GL) drawing into smithay's GLES context. The Skia `Surface`
//! is cached on (fboid, width, height) and recreated only on change.

use crate::render::HomeView;
use crate::rotation::Rotation;
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
    images, BlurStyle, ClipOp, Color, ColorType, Font, FontMgr, FontStyle, Image, ImageInfo,
    MaskFilter, Matrix, Paint, PathBuilder, RRect, Rect, Surface, TextBlob, TileMode,
};

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::c_int;

use tracing::{debug, warn};

const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
type GlGetIntegerv = unsafe extern "system" fn(pname: u32, params: *mut c_int);
type GlFinish = unsafe extern "system" fn();
type GlFlush = unsafe extern "system" fn();

pub struct SkiaGl {
    context: Option<DirectContext>,
    gl_get_integerv: Option<GlGetIntegerv>,
    gl_finish: Option<GlFinish>,
    gl_flush: Option<GlFlush>,
    setup_failed: bool,
    cached_surface: Option<CachedSurface>,
    icon_images: HashMap<String, Image>,
    icon_gen: u64,
    font: Option<Font>,
    /// Every draw takes view-space sizes; set per frame by [`Self::set_view`].
    view: Rotation,
}

/// A switcher card's rect in physical px (top-left origin, y down), plus its
/// drop shadow and depth scrim.
#[derive(Clone, Copy, Debug)]
pub struct CardDecor {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub radius: f32,
    /// Both cues fade with it.
    pub alpha: f32,
    pub dim: f32,
    /// Chrome (badge, title) fade, multiplied by `alpha`. The shadow and scrim
    /// don't ride it.
    pub chrome: f32,
    pub dpi: f32,
}

/// Fractions of card width. The deck fans left, so the shadow is cast left
/// only and kept tight enough to stay inside the neighbour's exposed strip;
/// wider reads as dirt.
const SHADOW_SIGMA_FRAC: f32 = 0.005;
const SHADOW_DX_FRAC: f32 = 0.006;
const SHADOW_ALPHA: f32 = 0.30;

/// Badge side as a fraction of card width, sitting above the card's top edge
/// with nothing over the client's pixels.
const CARD_ICON_FRAC: f32 = 0.20;
/// Cap in logical px, so a wide low-dpi panel doesn't get a huge badge.
const CARD_ICON_MAX_LOGICAL: f32 = 45.0;
const CARD_ICON_INSET_FRAC: f32 = 0.05;
const CARD_ICON_GAP_FRAC: f32 = 0.3;
const CARD_ICON_SHADOW_OFFSET_FRAC: f32 = 0.06;
const CARD_ICON_SHADOW_SIGMA_FRAC: f32 = 0.13;
const CARD_ICON_SHADOW_ALPHA: f32 = 0.45;

/// Fractions of the badge side.
const CARD_TITLE_GAP_FRAC: f32 = 0.28;
const CARD_TITLE_SIZE_FRAC: f32 = 0.34;
const CARD_TITLE_FADE_FRAC: f32 = 0.25;

impl CardDecor {
    fn rrect(&self) -> RRect {
        RRect::new_rect_xy(
            Rect::from_xywh(self.x, self.y, self.w, self.h),
            self.radius,
            self.radius,
        )
    }
}

/// Clipped to outside the card: apps are often translucent, and an unclipped
/// shadow shows through them as a dark wash.
fn draw_card_shadow_rrect(canvas: &skia_safe::Canvas, card: &CardDecor) {
    let sigma = (card.w * SHADOW_SIGMA_FRAC).max(1.0);
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_argb(
        (SHADOW_ALPHA * card.alpha * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8,
        0,
        0,
        0,
    ));
    // `Normal`, not `Outer`: Outer leaves a bright line of the covered card
    // right at the edge. The clip keeps the shadow off the card.
    paint.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, sigma, false));
    canvas.save();
    canvas.clip_rrect(card.rrect(), Some(ClipOp::Difference), Some(true));
    canvas.translate((-card.w * SHADOW_DX_FRAC, 0.0));
    canvas.draw_rrect(card.rrect(), &paint);
    canvas.restore();
}

#[derive(Clone, Copy)]
struct IconAssets<'a> {
    icon_images: &'a HashMap<String, Image>,
    font: &'a Option<Font>,
    app_catalog: &'a HashMap<String, AppEntry>,
}

#[derive(Clone, Copy)]
struct IconCues {
    pressed: bool,
    launching: Option<f32>,
    running: bool,
}

/// Shared by the badge and title so they can't drift apart.
fn card_icon_rect(card: &CardDecor) -> Rect {
    let side = (card.w * CARD_ICON_FRAC)
        .min(CARD_ICON_MAX_LOGICAL * card.dpi)
        .max(1.0);
    Rect::from_xywh(
        card.x + card.w * CARD_ICON_INSET_FRAC,
        card.y - side - side * CARD_ICON_GAP_FRAC,
        side,
        side,
    )
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
            gl_flush: None,
            setup_failed: false,
            cached_surface: None,
            icon_images: HashMap::new(),
            icon_gen: 0,
            font: None,
            view: Rotation::None,
        }
    }

    pub fn set_view(&mut self, view: Rotation) {
        self.view = view;
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

        let flush_ptr = loader("glFlush");
        if !flush_ptr.is_null() {
            self.gl_flush =
                Some(unsafe { std::mem::transmute::<*const c_void, GlFlush>(flush_ptr) });
        }

        self.context = Some(context);
        self.gl_get_integerv = Some(gl_get_integerv);
        debug!("Skia Ganesh-GL context initialized");
        true
    }

    /// Blocks until all GL commands complete. The DRM fallback when no fence
    /// can be exported.
    pub fn finish_gpu(&self) {
        if let Some(finish) = self.gl_finish {
            unsafe { finish() };
        }
    }

    /// A fence only becomes signalable once the commands before it are flushed.
    pub fn flush_gpu(&self) {
        if let Some(flush) = self.gl_flush {
            unsafe { flush() };
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
            let mut font = Font::from_typeface(typeface, 28.0);
            font.set_subpixel(true);
            self.font = Some(font);
        }
    }

    /// Manual surface acquisition, not `with_overlay_canvas`: the rows need
    /// `&self.font` while the canvas borrows `&mut self.cached_surface`.
    pub fn draw_icon_menu(
        &mut self,
        width: i32,
        height: i32,
        menu: &crate::render::MenuView,
        flip_y: bool,
    ) {
        self.ensure_font();
        if !self.ensure_surface(width, height) {
            return;
        }
        let view = self.view;
        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        canvas.save();
        orient(canvas, view, width, height, flip_y);
        let p = menu.progress.clamp(0.0, 1.0);
        let scale = 0.85 + 0.15 * p;
        canvas.translate((menu.anchor.0, menu.anchor.1));
        canvas.scale((scale, scale));
        canvas.translate((-menu.anchor.0, -menu.anchor.1));
        draw_menu_panel(canvas, menu, &self.font, p);
        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// Manual surface acquisition, as in `draw_icon_menu`.
    pub fn draw_folder_panel(
        &mut self,
        width: i32,
        height: i32,
        folder: &crate::render::FolderView,
        icon_cache: &HashMap<String, IconPixels>,
        app_catalog: &HashMap<String, AppEntry>,
        flip_y: bool,
    ) {
        self.ensure_font();
        for (app_id, pixels) in icon_cache {
            if !self.icon_images.contains_key(app_id) {
                self.get_or_upload_icon(app_id, pixels);
            }
        }
        if !self.ensure_surface(width, height) {
            return;
        }
        let view = self.view;
        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        // One restore unwinds the flip, zoom, alpha layer and clip.
        let base = canvas.save();
        orient(canvas, view, width, height, flip_y);

        let open = folder.progress.clamp(0.0, 1.0);
        let mut dim = Paint::default();
        dim.set_color(Color::from_argb((140.0 * open) as u8, 0, 0, 0));
        canvas.draw_rect(Rect::new(0.0, 0.0, width as f32, height as f32), &dim);

        let scale = 0.4 + 0.6 * open;
        canvas.translate((folder.anchor.0, folder.anchor.1));
        canvas.scale((scale, scale));
        canvas.translate((-folder.anchor.0, -folder.anchor.1));
        canvas.save_layer_alpha_f(None, open);

        let p = folder.layout.panel;
        let card = Rect::new(p.x, p.y, p.x + p.w, p.y + p.h);
        let radius = p.w * 0.06;
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::from_argb(235, 28, 28, 32));
        canvas.draw_rrect(RRect::new_rect_xy(card, radius, radius), &paint);

        if let Some(f) = &self.font {
            let mut text = Paint::default();
            text.set_color(Color::WHITE);
            if let Some(blob) = TextBlob::new(&folder.title, f) {
                let tw = f.measure_str(&folder.title, None).0;
                let t = folder.layout.title_rect;
                canvas.draw_text_blob(&blob, (t.x + (t.w - tw) / 2.0, t.y + t.h * 0.7), &text);
            }
        }

        // Same rect the hit-test uses, so tappable == drawn.
        let title = folder.layout.title_rect;
        canvas.clip_rect(
            Rect::new(
                p.x,
                title.y + title.h,
                p.x + p.w,
                title.y + title.h + folder.layout.view_h,
            ),
            None,
            Some(true),
        );
        let assets = IconAssets {
            icon_images: &self.icon_images,
            font: &self.font,
            app_catalog,
        };
        for (i, slot) in folder.layout.apps.iter().enumerate() {
            draw_icon_slot(
                canvas,
                slot,
                assets,
                IconCues {
                    pressed: folder.pressed == Some(i),
                    launching: None,
                    running: false,
                },
            );
        }
        canvas.restore_to_count(base);

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// Icons are keyed by app id; a rescan must drop them or re-themed apps keep
    /// old pixels. Call once per frame before drawing icons.
    pub fn sync_catalog_gen(&mut self, gen: u64) {
        if self.icon_gen != gen {
            self.icon_gen = gen;
            self.icon_images.clear();
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

    /// Grid icons are drawn from `grid_positions`, so paging and reflow are icon
    /// motion.
    pub fn draw_home(&mut self, width: i32, height: i32, flip_y: bool, view: &HomeView<'_>) {
        let &HomeView {
            page,
            model,
            icon_cache,
            app_catalog,
            pressed_app,
            launch_pulses,
            running_apps,
            arrange,
            grid_positions,
            dock_positions,
            library,
            top_inset,
            lift,
            shift,
        } = view;
        self.ensure_font();

        for (app_id, pixels) in icon_cache {
            if !self.icon_images.contains_key(app_id) {
                self.get_or_upload_icon(app_id, pixels);
            }
        }

        // Manual acquisition: the body needs `&self.icon_images` and `&self.font`.
        if !self.ensure_surface(width, height) {
            return;
        }

        let view = self.view;
        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        // Save/restore so the cached surface's matrix doesn't accumulate.
        canvas.save();
        orient(canvas, view, width, height, flip_y);
        // The lift's sign follows the flip.
        if lift != 0.0 || shift != 0.0 {
            canvas.translate((shift, if flip_y { lift } else { -lift }));
        }

        // Deterministic model order (see `visible_grid_slots`); reused for the
        // arrange badges so they track the sliding icons.
        let anim_slots = visible_grid_slots(model, grid_positions, width as f32, height as f32);
        let assets = IconAssets {
            icon_images: &self.icon_images,
            font: &self.font,
            app_catalog,
        };
        let cues = |slot: &IconSlot| IconCues {
            pressed: pressed_app == Some(slot.app_id.as_str()),
            launching: launch_pulse(launch_pulses, &slot.app_id),
            running: running_apps.contains(&slot.app_id),
        };
        for slot in &anim_slots {
            draw_icon_slot(canvas, slot, assets, cues(slot));
        }

        if let Some(lib) = library {
            canvas.save();
            canvas.translate((lib.x_offset, 0.0));
            for (tile, preview) in lib.tiles.iter().zip(lib.previews.iter()) {
                draw_folder_tile(canvas, tile, preview, &self.icon_images, &self.font);
            }
            canvas.restore();
        }

        let mut current_layout = sc_layout::compute(width as f32, height as f32, page, model);
        current_layout.shift_done_below(top_inset);

        let dock_slots =
            visible_dock_slots(&current_layout, dock_positions, width as f32, height as f32);
        for slot in &dock_slots {
            draw_icon_slot(canvas, slot, assets, cues(slot));
        }

        draw_dots(canvas, &current_layout, page);

        // No bar here: the chrome overlay draws the pill.

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

    /// Takes the view size; the render target is the panel's. On false,
    /// `cached_surface` must not be touched.
    fn ensure_surface(&mut self, width: i32, height: i32) -> bool {
        let (width, height) = self.view.app_size((width, height));
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

        let view = self.view;
        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();

        canvas.save();
        orient(canvas, view, width, height, flip_y);
        f(canvas);
        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// `card` is the drawn rect the pill rides; `None` puts it in the bar band.
    pub fn draw_bar_overlay(
        &mut self,
        width: i32,
        height: i32,
        alpha: f32,
        flip_y: bool,
        card: Option<sc_layout::Rect>,
    ) {
        if alpha <= 0.0 {
            return;
        }
        let pill = match card {
            Some(c) => sc_layout::pill_under(c, width as f32, height as f32),
            None => sc_layout::pill_rect(width as f32, height as f32),
        };
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            draw_pill(canvas, pill, alpha)
        });
    }

    /// Right edge, top third, next to the physical rockers. `level` 1.0 = 100%.
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

    /// ext-background-effect-v1: blur what's already drawn inside `rects`. Call
    /// after everything behind the surface and before the surface itself. The
    /// snapshot is in surface space, so it is drawn unflipped and the clip is
    /// flipped instead; flipping both mirrors the blur.
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

        let view = self.view;
        let (pw, ph) = view.app_size((width, height));
        let to_canvas = |r: &crate::background_effect::BlurRect| {
            // Inverse of `Rotation::map_input`.
            let (x, y, w, h) = match view {
                Rotation::None => (r.x, r.y, r.w, r.h),
                Rotation::LeftUp => (r.y, ph as f32 - r.x - r.w, r.h, r.w),
                Rotation::RightUp => (pw as f32 - r.y - r.h, r.x, r.h, r.w),
            };
            let y = if flip_y { ph as f32 - (y + h) } else { y };
            Rect::from_xywh(x, y, w, h)
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

    /// Call right before the card's own draw: the deck overlaps, so one shadow
    /// pre-pass would be hidden by the cards in front.
    pub fn draw_card_shadow(&mut self, width: i32, height: i32, card: &CardDecor, flip_y: bool) {
        if card.alpha <= 0.0 || card.w <= 0.0 || card.h <= 0.0 {
            return;
        }
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            draw_card_shadow_rrect(canvas, card)
        });
    }

    /// Drawn right after the card so it tints the client's pixels.
    pub fn draw_card_dim(&mut self, width: i32, height: i32, card: &CardDecor, flip_y: bool) {
        let a = card.dim * card.alpha;
        if a <= 0.0 || card.w <= 0.0 || card.h <= 0.0 {
            return;
        }
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            let mut paint = Paint::default();
            paint.set_anti_alias(true);
            paint.set_color(Color::from_argb(
                (a * 255.0).round().clamp(0.0, 255.0) as u8,
                0,
                0,
                0,
            ));
            canvas.draw_rrect(card.rrect(), &paint);
        });
    }

    /// Floats above the card's top-left. Drawn after the scrim so it stays
    /// legible; opacity is the card's times `card.chrome`.
    pub fn draw_card_icon(
        &mut self,
        width: i32,
        height: i32,
        card: &CardDecor,
        app_id: &str,
        icon_cache: &HashMap<String, IconPixels>,
        flip_y: bool,
    ) {
        let alpha = (card.alpha * card.chrome).clamp(0.0, 1.0);
        if alpha <= 0.0 || card.w <= 0.0 || card.h <= 0.0 {
            return;
        }
        // The deck may be drawn in frames where Home wasn't.
        let image = match icon_cache.get(app_id) {
            Some(pixels) => self.get_or_upload_icon(app_id, pixels),
            None => self.icon_images.get(app_id).cloned(),
        };
        let Some(image) = image else {
            return;
        };
        let dst = card_icon_rect(card);
        let side = dst.width();
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            let mut shadow = Paint::default();
            shadow.set_anti_alias(true);
            shadow.set_color(Color::from_argb(
                (CARD_ICON_SHADOW_ALPHA * alpha * 255.0)
                    .round()
                    .clamp(0.0, 255.0) as u8,
                0,
                0,
                0,
            ));
            shadow.set_mask_filter(MaskFilter::blur(
                BlurStyle::Normal,
                (side * CARD_ICON_SHADOW_SIGMA_FRAC).max(1.0),
                false,
            ));
            let lift = side * CARD_ICON_SHADOW_OFFSET_FRAC;
            canvas.draw_rrect(
                RRect::new_rect_xy(dst.with_offset((lift, lift)), side * 0.22, side * 0.22),
                &shadow,
            );

            let mut paint = Paint::default();
            paint.set_anti_alias(true);
            paint.set_alpha_f(alpha);
            canvas.draw_image_rect(&image, None, dst, &paint);
        });
    }

    /// Not ellipsized: an overrunning title fades out at the card's right edge.
    /// Manual surface acquisition, as in `draw_icon_menu`.
    pub fn draw_card_title(
        &mut self,
        width: i32,
        height: i32,
        card: &CardDecor,
        title: &str,
        alpha: f32,
        flip_y: bool,
    ) {
        let alpha = alpha.clamp(0.0, 1.0);
        if alpha <= 0.0 || title.is_empty() || card.w <= 0.0 {
            return;
        }
        let icon = card_icon_rect(card);
        let x0 = icon.right() + icon.width() * CARD_TITLE_GAP_FRAC;
        let right = card.x + card.w - card.w * CARD_ICON_INSET_FRAC;
        let avail = right - x0;
        if avail <= 0.0 {
            return;
        }

        self.ensure_font();
        if !self.ensure_surface(width, height) {
            return;
        }
        let Some(font) = self
            .font
            .as_ref()
            .and_then(|f| f.with_size(icon.width() * CARD_TITLE_SIZE_FRAC))
        else {
            return;
        };
        let Some(blob) = TextBlob::new(title, &font) else {
            return;
        };
        let text_w = font.measure_str(title, None).0;
        // Metrics are negative above the baseline.
        let (_, metrics) = font.metrics();
        let baseline = icon.center_y() - (metrics.ascent + metrics.descent) / 2.0;

        let view = self.view;
        let surface = &mut self.cached_surface.as_mut().unwrap().surface;
        let canvas = surface.canvas();
        canvas.save();
        orient(canvas, view, width, height, flip_y);
        // A DstIn fade needs its own layer or it eats the card.
        let bounds = Rect::new(x0, icon.top(), right, icon.bottom());
        canvas.save_layer_alpha_f(bounds, 1.0);
        canvas.clip_rect(bounds, None, true);

        let mut shadow = Paint::default();
        shadow.set_anti_alias(true);
        shadow.set_color(Color::from_argb(
            (CARD_ICON_SHADOW_ALPHA * alpha * 255.0)
                .round()
                .clamp(0.0, 255.0) as u8,
            0,
            0,
            0,
        ));
        shadow.set_mask_filter(MaskFilter::blur(
            BlurStyle::Normal,
            (font.size() * 0.12).max(1.0),
            false,
        ));
        canvas.draw_text_blob(&blob, (x0, baseline + font.size() * 0.06), &shadow);

        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::WHITE);
        paint.set_alpha_f(alpha);
        canvas.draw_text_blob(&blob, (x0, baseline), &paint);

        if text_w > avail {
            let fade = (avail * CARD_TITLE_FADE_FRAC).min(icon.width());
            let mut mask = Paint::default();
            mask.set_blend_mode(skia_safe::BlendMode::DstIn);
            let stops = [
                skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
                skia_safe::Color4f::new(1.0, 1.0, 1.0, 0.0),
            ];
            let colors =
                skia_safe::gradient::Colors::new_evenly_spaced(&stops, TileMode::Clamp, None);
            mask.set_shader(skia_safe::gradient::shaders::linear_gradient(
                ((right - fade, 0.0), (right, 0.0)),
                &skia_safe::gradient::Gradient::new(
                    colors,
                    skia_safe::gradient::Interpolation::default(),
                ),
                None,
            ));
            canvas.draw_rect(
                Rect::new(right - fade, bounds.top(), right, bounds.bottom()),
                &mask,
            );
        }
        canvas.restore();
        canvas.restore();

        if let Some(ctx) = self.context.as_mut() {
            ctx.flush_and_submit();
        }
    }

    /// The rotation dip, drawn over everything.
    pub fn draw_screen_dim(&mut self, width: i32, height: i32, alpha: f32, flip_y: bool) {
        if alpha <= 0.0 {
            return;
        }
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            let mut paint = Paint::default();
            paint.set_color(Color::from_argb(
                (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
                0,
                0,
                0,
            ));
            canvas.draw_rect(
                skia_safe::Rect::from_xywh(0.0, 0.0, width as f32, height as f32),
                &paint,
            );
        });
    }

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

    /// `scale` is `dpi`.
    pub fn draw_cursor(
        &mut self,
        width: i32,
        height: i32,
        x: f32,
        y: f32,
        scale: f32,
        flip_y: bool,
    ) {
        self.with_overlay_canvas(width, height, flip_y, |canvas| {
            draw_cursor_arrow(canvas, x, y, scale);
        });
    }
}

/// View space to panel: the inverse of [`Rotation::map_input`], then the DRM
/// y-flip.
fn orient(canvas: &skia_safe::Canvas, view: Rotation, width: i32, height: i32, flip_y: bool) {
    let (pw, ph) = view.app_size((width, height));
    if flip_y {
        canvas.translate((0.0, ph as f32));
        canvas.scale((1.0, -1.0));
    }
    let turn = match view {
        Rotation::None => return,
        Rotation::LeftUp => Matrix::new_all(0.0, 1.0, 0.0, -1.0, 0.0, ph as f32, 0.0, 0.0, 1.0),
        Rotation::RightUp => Matrix::new_all(0.0, -1.0, pw as f32, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0),
    };
    canvas.concat(&turn);
}

/// We draw our own arrow: `cursor_image` is a no-op, so client cursor
/// surfaces are never composited.
fn draw_cursor_arrow(canvas: &skia_safe::Canvas, x: f32, y: f32, scale: f32) {
    const POINTS: [(f32, f32); 7] = [
        (0.0, 0.0),
        (0.0, 16.0),
        (4.0, 12.4),
        (6.8, 19.0),
        (9.6, 17.8),
        (6.9, 11.4),
        (11.8, 11.4),
    ];
    let s = (scale * 8.0).clamp(12.0, 48.0) / 12.0;
    let pts: Vec<skia_safe::Point> = POINTS
        .iter()
        .map(|(px, py)| skia_safe::Point::new(x + px * s, y + py * s))
        .collect();
    let path = skia_safe::Path::polygon(&pts, true, None, None);

    // Black outline under white fill, visible on dark and light.
    let mut outline = Paint::default();
    outline.set_anti_alias(true);
    outline.set_stroke(true);
    outline.set_stroke_width(1.6 * s);
    outline.set_color(Color::from_argb(230, 0, 0, 0));
    canvas.draw_path(&path, &outline);

    let mut fill = Paint::default();
    fill.set_anti_alias(true);
    fill.set_color(Color::from_argb(255, 255, 255, 255));
    canvas.draw_path(&path, &fill);
}

fn draw_touch_marks(canvas: &skia_safe::Canvas, marks: &[crate::touch_viz::TouchMark]) {
    const R: u8 = 255;
    const G: u8 = 205;
    const B: u8 = 70;
    for m in marks {
        let a = |x: f32| (x * m.alpha).round().clamp(0.0, 255.0) as u8;
        if m.filled {
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
            let mut ring = Paint::default();
            ring.set_anti_alias(true);
            ring.set_stroke(true);
            ring.set_stroke_width(m.radius * 0.12);
            ring.set_color(Color::from_argb(a(255.0), R, G, B));
            canvas.draw_circle((m.x, m.y), m.radius, &ring);
        }
    }
}

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
    let y = height * 0.10;
    let radius = track_w / 2.0;

    let mut track = Paint::default();
    track.set_anti_alias(true);
    track.set_color(Color::from_argb(a(90.0), 40, 40, 40));
    let track_rect = Rect::new(x, y, x + track_w, y + track_h);
    canvas.draw_rrect(RRect::new_rect_xy(track_rect, radius, radius), &track);

    let frac = level.clamp(0.0, 1.0);
    let fill_h = track_h * frac;
    let fill_top = y + track_h - fill_h;
    let (r, g, b) = if muted {
        (110, 110, 120)
    } else if level > 1.0 {
        (255, 170, 60)
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

/// Seconds since launch while an app is waiting for its window. With several
/// in flight, the oldest wins.
fn launch_pulse(launch_pulses: &[(String, f32)], app_id: &str) -> Option<f32> {
    launch_pulses
        .iter()
        .filter(|(id, _)| id == app_id)
        .map(|(_, elapsed)| *elapsed)
        .fold(None, |acc: Option<f32>, e| {
            Some(acc.map_or(e, |a| a.max(e)))
        })
}

fn draw_icon_slot(
    canvas: &skia_safe::Canvas,
    slot: &IconSlot,
    assets: IconAssets<'_>,
    cues: IconCues,
) {
    let IconAssets {
        icon_images,
        font,
        app_catalog,
    } = assets;
    let IconCues {
        pressed,
        launching,
        running,
    } = cues;
    let icon_scale = 1.0;
    if let Some(elapsed) = launching {
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

    let cx = slot.icon_rect.x + slot.icon_rect.w / 2.0;
    let cy = slot.icon_rect.y + slot.icon_rect.h / 2.0;
    let hw = slot.icon_rect.w / 2.0 * icon_scale;
    let hh = slot.icon_rect.h / 2.0 * icon_scale;
    let icon_dst = Rect::new(cx - hw, cy - hh, cx + hw, cy + hh);

    if let Some(image) = icon_images.get(&slot.app_id) {
        canvas.draw_image_rect(image, None, icon_dst, &Paint::default());
    } else {
        let mut paint = Paint::default();
        paint.set_color(Color::from_argb(255, 80, 80, 100));
        let rrect = RRect::new_rect_xy(icon_dst, 20.0, 20.0);
        canvas.draw_rrect(rrect, &paint);
    }

    if running {
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(Color::from_argb(200, 255, 255, 255));
        let d = slot.dot_rect;
        canvas.draw_circle((d.center_x(), d.center_y()), d.w / 2.0, &paint);
    }

    if let Some(entry) = app_catalog.get(&slot.app_id) {
        if let Some(f) = font {
            let mut paint = Paint::default();
            paint.set_color(Color::WHITE);
            // Long names would run under their neighbours' labels.
            let name = ellipsize(f, &entry.name, slot.label_rect.w);
            if let Some(blob) = TextBlob::new(&name, f) {
                let text_width = f.measure_str(&name, None).0;
                let x = slot.label_rect.x + (slot.label_rect.w - text_width) / 2.0;
                let y = slot.label_rect.y + slot.label_rect.h * 0.75;
                canvas.draw_text_blob(&blob, (x, y), &paint);
            }
        }
    }
}

fn draw_folder_tile(
    canvas: &skia_safe::Canvas,
    slot: &sc_layout::library::FolderSlot,
    preview: &[String],
    icon_images: &HashMap<String, Image>,
    font: &Option<Font>,
) {
    let t = slot.tile_rect;
    let rect = Rect::new(t.x, t.y, t.x + t.w, t.y + t.h);
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_argb(56, 255, 255, 255));
    canvas.draw_rrect(RRect::new_rect_xy(rect, t.w * 0.22, t.w * 0.22), &paint);

    for (r, app_id) in slot.preview.iter().zip(preview.iter()) {
        let dst = Rect::new(r.x, r.y, r.x + r.w, r.y + r.h);
        if let Some(image) = icon_images.get(app_id) {
            canvas.draw_image_rect(image, None, dst, &Paint::default());
        }
    }

    if let Some(f) = font {
        let mut paint = Paint::default();
        paint.set_color(Color::WHITE);
        if let Some(blob) = TextBlob::new(&slot.name, f) {
            let text_width = f.measure_str(&slot.name, None).0;
            let x = slot.label_rect.x + (slot.label_rect.w - text_width) / 2.0;
            let y = slot.label_rect.y + slot.label_rect.h * 0.75;
            canvas.draw_text_blob(&blob, (x, y), &paint);
        }
    }
}

const MENU_FONT_SCALE: f32 = 1.15;
/// Fraction of the panel width.
const MENU_TEXT_INSET_FRAC: f32 = 0.08;

/// Trims by char, so multi-byte titles aren't split; empty if even the
/// ellipsis doesn't fit.
fn ellipsize(font: &Font, label: &str, max_w: f32) -> String {
    if font.measure_str(label, None).0 <= max_w {
        return label.to_string();
    }
    let mut chars: Vec<char> = label.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>() + "…";
        if font.measure_str(&candidate, None).0 <= max_w {
            return candidate;
        }
    }
    String::new()
}

fn draw_menu_panel(
    canvas: &skia_safe::Canvas,
    menu: &crate::render::MenuView,
    font: &Option<Font>,
    progress: f32,
) {
    let a = |alpha: f32| (alpha * progress).clamp(0.0, 255.0) as u8;
    let panel = &menu.layout.panel;
    let panel_rect = Rect::new(panel.x, panel.y, panel.x + panel.w, panel.y + panel.h);
    let radius = (panel.w * 0.06).min(28.0);

    let mut shadow = Paint::default();
    shadow.set_anti_alias(true);
    shadow.set_color(Color::from_argb(a(90.0), 0, 0, 0));
    shadow.set_mask_filter(skia_safe::MaskFilter::blur(
        skia_safe::BlurStyle::Normal,
        18.0,
        false,
    ));
    canvas.draw_rrect(RRect::new_rect_xy(panel_rect, radius, radius), &shadow);

    // Fully opaque: any bleed-through ghosts the icon labels across the rows.
    let mut bg = Paint::default();
    bg.set_anti_alias(true);
    bg.set_color(Color::from_argb(a(255.0), 28, 30, 36));
    canvas.draw_rrect(RRect::new_rect_xy(panel_rect, radius, radius), &bg);

    for (i, row) in menu.layout.items.iter().enumerate() {
        let rect = Rect::new(row.x, row.y, row.x + row.w, row.y + row.h);
        if menu.pressed == Some(i) {
            let mut hl = Paint::default();
            hl.set_anti_alias(true);
            hl.set_color(Color::from_argb(a(50.0), 255, 255, 255));
            canvas.draw_rect(rect, &hl);
        }
        if i > 0 {
            let mut sep = Paint::default();
            sep.set_color(Color::from_argb(a(40.0), 255, 255, 255));
            let inset = row.w * 0.05;
            canvas.draw_rect(
                Rect::new(row.x + inset, row.y, row.x + row.w - inset, row.y + 1.0),
                &sep,
            );
        }
        let Some((label, destructive)) = menu.items.get(i) else {
            continue;
        };
        let Some(f) = font else { continue };
        let f = f.with_size(f.size() * MENU_FONT_SCALE).unwrap_or(f.clone());
        let x = row.x + row.w * MENU_TEXT_INSET_FRAC;
        let label = ellipsize(&f, label, row.w * (1.0 - 2.0 * MENU_TEXT_INSET_FRAC));
        let Some(blob) = TextBlob::new(&label, &f) else {
            continue;
        };
        let mut paint = Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(if *destructive {
            Color::from_argb(a(255.0), 255, 120, 110)
        } else {
            Color::from_argb(a(255.0), 255, 255, 255)
        });
        let (_, metrics) = f.metrics();
        let y = row.y + row.h / 2.0 - (metrics.ascent + metrics.descent) / 2.0;
        canvas.draw_text_blob(&blob, (x, y), &paint);
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

fn draw_pill(canvas: &skia_safe::Canvas, pill: sc_layout::Rect, alpha: f32) {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    let a = (180.0 * alpha.clamp(0.0, 1.0)).round() as u8;
    paint.set_color(Color::from_argb(a, 255, 255, 255));

    let rect = Rect::new(pill.x, pill.y, pill.x + pill.w, pill.y + pill.h);
    let radius = pill.h / 2.0;
    let rrect = RRect::new_rect_xy(rect, radius, radius);
    canvas.draw_rrect(rrect, &paint);
}

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

fn draw_dock_highlight(canvas: &skia_safe::Canvas, layout: &Layout) {
    let z = &layout.dock_zone;
    let rect = Rect::new(z.x, z.y, z.x + z.w, z.y + z.h);

    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_color(Color::from_argb(70, 255, 255, 255));
    canvas.draw_rect(rect, &paint);
}

/// Skipped if the icon isn't uploaded yet.
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

/// Walks `model.pages` for a stable z-order; iterating the `HashMap` would
/// shimmer as overlapping icons z-fight. Off-screen icons are culled.
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

pub(crate) fn visible_dock_slots(
    layout: &Layout,
    dock_positions: &HashMap<String, (f32, f32)>,
    width: f32,
    height: f32,
) -> Vec<IconSlot> {
    // A dragged dock icon is absent from `dock_positions`; it renders only as
    // the ghost.
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
        gp.insert("off".to_string(), (1224.0 * 2.0, 400.0));
        let slots = visible_grid_slots(&m, &gp, 1224.0, 2700.0);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].app_id, "on");
    }
}
