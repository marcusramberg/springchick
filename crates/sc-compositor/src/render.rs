//! Shared render path. Backends differ only in how they bind the framebuffer
//! and present.

use std::collections::{HashMap, HashSet};

use crate::scene::Scene;
use crate::skia_gl::SkiaGl;
use smithay::wayland::presentation::PresentationFeedbackCallback;

use sc_catalog::AppEntry;
use sc_icons::IconPixels;
use sc_shell_model::ShellModel;

use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, Id as ElementId, Kind};
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{draw_render_elements, CommitCounter};
use smithay::backend::renderer::{
    buffer_type, BufferType, Color32F, Frame, ImportDmaWl, ImportMemWl, Renderer, RendererSuper,
};
use smithay::backend::SwapBuffersError;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{
    with_surface_tree_downward, SurfaceAttributes, TraversalAction,
};

use tracing::warn;

/// ext-background-effect blur, logical px (scaled by dpi).
const BLUR_SIGMA_LOGICAL: f32 = 8.0;

/// The switcher backdrop, logical px. Stronger: it pushes all of Home back.
const BACKDROP_BLUR_SIGMA_LOGICAL: f32 = 18.0;

pub const CLEAR_COLOR: Color32F = Color32F::new(0.06, 0.10, 0.14, 1.0);

/// smithay's `texture.frag` (pin 7ddcd17) plus a rounded-rect SDF mask.
///
/// The mask works in framebuffer px off `gl_FragCoord`, with the card rect as
/// a uniform. `v_coords` can't be used: it folds in the client's viewport
/// crop, so for waydroid (a viewport-mapped gralloc buffer) the mask
/// collapses to a constant, painting the card black or unrounded. Inverting
/// `tex_matrix` in the shader breaks on the phone's precision.
const ROUNDED_TEX_SHADER: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

uniform float corner_radius;
// The card in framebuffer px: origin in `gl_FragCoord` space (y from the
// bottom), then size.
uniform vec4 card_rect;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    // Rounded-rect signed distance field in card-local physical px.
    vec2 p = gl_FragCoord.xy - card_rect.xy;
    vec2 half_size = card_rect.zw * 0.5;
    vec2 q = abs(p - half_size) - (half_size - vec2(corner_radius));
    float dist = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - corner_radius;
    float aa = max(0.5, min(card_rect.z, card_rect.w) * 0.004);
    float mask = 1.0 - smoothstep(-aa, aa, dist);
    color *= mask;

    gl_FragColor = color;
}
"#;

/// Call once per backend after creating the `GlesRenderer`.
pub fn compile_rounded_tex_shader(
    renderer: &mut GlesRenderer,
) -> Result<GlesTexProgram, GlesError> {
    renderer.compile_custom_texture_shader(
        ROUNDED_TEX_SHADER,
        &[
            UniformName::new("corner_radius", UniformType::_1f),
            UniformName::new("card_rect", UniformType::_4f),
        ],
    )
}

pub struct ArrangeView<'a> {
    pub drag_app: Option<&'a str>,
    /// Output pixels.
    pub drag_pos: Option<(f32, f32)>,
    pub over_dock: bool,
}

/// Everything the Skia home pass draws from, assembled by [`pass_home`].
pub struct HomeView<'a> {
    /// Dock/dots page. Grid icons come from `grid_positions`.
    pub page: usize,
    pub model: &'a ShellModel,
    pub icon_cache: &'a HashMap<String, IconPixels>,
    pub app_catalog: &'a HashMap<String, AppEntry>,
    pub pressed_app: Option<&'a str>,
    /// `(app_id, seconds since spawn)` per pending launch.
    pub launch_pulses: &'a [(String, f32)],
    pub running_apps: &'a HashSet<String>,
    pub arrange: Option<&'a ArrangeView<'a>>,
    pub grid_positions: &'a HashMap<String, (f32, f32)>,
    pub dock_positions: &'a HashMap<String, (f32, f32)>,
    /// Drawn with the grid so it pages with it.
    pub library: Option<&'a LibraryView>,
    /// The arrange Done button stays below this.
    pub top_inset: f32,
    /// Home bounce (up) and drag-out (sideways).
    pub lift: f32,
    pub shift: f32,
}

/// Tiles are page-local; `x_offset` slides them with the pages.
pub struct LibraryView {
    pub tiles: Vec<sc_layout::library::FolderSlot>,
    /// Up to four ids per tile, parallel to `tiles`.
    pub previews: Vec<Vec<String>>,
    pub x_offset: f32,
}

pub struct FolderView {
    pub layout: sc_layout::library::PanelLayout,
    pub title: String,
    pub pressed: Option<usize>,
    pub anchor: (f32, f32),
    /// 0→1, and back while closing.
    pub progress: f32,
}

pub struct MenuView {
    /// Output pixels.
    pub layout: sc_layout::menu::MenuLayout,
    /// `(label, destructive)`, parallel to `layout.items`.
    pub items: Vec<(String, bool)>,
    pub pressed: Option<usize>,
    pub anchor: (f32, f32),
    pub progress: f32,
}

#[derive(Default)]
pub struct CardChromeView {
    /// 0 when the deck is down.
    pub icon_alpha: f32,
    /// `None` when no title is on screen.
    pub title: Option<(crate::ui_state::ToplevelId, String)>,
    /// Already multiplied by `icon_alpha`.
    pub title_alpha: f32,
}

/// Obligations for the backend after presenting: unanswered feedback hangs a
/// client, and an unreleased blocker leaves its commit unapplied. See
/// [`crate::presentation`] and [`crate::pacing`].
#[derive(Default)]
pub struct FrameSinks {
    pub presented: Vec<PresentationFeedbackCallback>,
    pub unblocked: Vec<smithay::reexports::wayland_server::Client>,
}

pub struct DrawCtx<'a> {
    pub scene: &'a Scene,
    pub app_surface: Option<&'a WlSurface>,
    pub skia: &'a mut SkiaGl,
    pub model: &'a ShellModel,
    pub icon_cache: &'a HashMap<String, IconPixels>,
    pub app_catalog: &'a HashMap<String, AppEntry>,
    /// A change drops the renderer's uploaded icons.
    pub catalog_gen: u64,
    pub toplevels: &'a Vec<Option<crate::AppToplevel>>,
    /// `dpi`. Apps render oversized buffers, so elements are generated at this
    /// scale to land at physical size.
    pub app_scale: f64,
    /// Usable-area origin: below/right of top/left exclusive zones.
    pub app_origin: (i32, i32),
    /// winit = Flipped180; DRM = connector transform.
    pub transform: Transform,
    /// Composed on `transform` for everything but the upright layers; Skia gets
    /// it via [`SkiaGl::set_view`].
    pub rotation: crate::rotation::Rotation,
    /// DRM scanout is Y-flipped vs Skia's BottomLeft surface; winit is not.
    pub skia_flip_y: bool,
    pub frame_time: u32,
    /// `(level, muted, alpha)`.
    pub osd: Option<(f32, bool, f32)>,
    /// Physical coords.
    pub touches: &'a [crate::touch_viz::TouchMark],
    /// Physical px.
    pub cursor: Option<(f32, f32)>,
    /// Anything but `Unlocked` replaces the scene; see [`draw_locked`].
    pub lock_view: crate::session_lock::LockView,
    pub lock_surface: Option<&'a WlSurface>,
    pub layers_below: &'a [(WlSurface, (i32, i32))],
    pub layers_above: &'a [(WlSurface, (i32, i32))],
    /// The held buffer and its physical rect this frame.
    pub closing: Option<(
        &'a smithay::backend::renderer::utils::Buffer,
        sc_layout::Rect,
    )>,
    /// Root→leaf, clamped physical origins. Above the app, below top layers.
    pub app_popups: &'a [(WlSurface, (i32, i32))],
    /// Root→leaf. Above the layers, below springchick chrome.
    pub layer_popups: &'a [(WlSurface, (i32, i32))],
    pub bar_alpha: f32,
    /// Drawn rect of the card the pill rides, set by the card passes. Drawn
    /// bounds, not the slot: with a top bar the client's pixels end short of the
    /// slot bottom.
    pub pill_anchor: Option<sc_layout::Rect>,
    /// The rotation dip.
    pub dim: f32,
    pub pressed_app: Option<&'a str>,
    /// `(app_id, seconds since spawn)`.
    pub launch_pulses: &'a [(String, f32)],
    pub running_apps: &'a HashSet<String>,
    pub arrange: Option<ArrangeView<'a>>,
    pub icon_menu: Option<&'a MenuView>,
    pub library: Option<&'a LibraryView>,
    pub folder: Option<&'a FolderView>,
    pub card_chrome: &'a CardChromeView,
    /// Screen-space centers from `State.grid_anim`.
    pub grid_positions: &'a HashMap<String, (f32, f32)>,
    /// Screen-space centers from `State.dock_anim`.
    pub dock_positions: &'a HashMap<String, (f32, f32)>,
    /// Only the app surface can have changed, so `draw_scene` may return a
    /// narrowed damage hint. Computed by the backend.
    pub report_partial_damage: bool,
    /// The app surface and its last presented `CommitCounter`. A different
    /// surface resets to full damage.
    pub last_present: &'a mut Option<(WlSurface, CommitCounter)>,
    /// Applied when `corner_radius > 0`.
    pub rounded_tex_shader: &'a GlesTexProgram,
    /// Expected presentation time, for releasing commit-timing commits.
    pub frame_target: smithay::utils::Time<smithay::utils::Monotonic>,
    pub sinks: &'a mut FrameSinks,
}

impl DrawCtx<'_> {
    fn base(&self) -> Transform {
        self.transform + self.rotation.transform()
    }

    /// Element space for [`Self::base`], and what Skia and the scene lay out in.
    fn view(&self, size: Size<i32, Physical>) -> Size<i32, Physical> {
        self.rotation.app_size((size.w, size.h)).into()
    }
}

/// Blur whatever is already drawn behind `surface`, if it asked. Run after
/// everything behind it and before the surface.
fn blur_behind(
    skia: &mut SkiaGl,
    size: Size<i32, Physical>,
    surface: &WlSurface,
    origin: (i32, i32),
    scale: f64,
    flip_y: bool,
) {
    let rects = crate::background_effect::blur_rects(surface, origin, scale);
    if rects.is_empty() {
        return;
    }
    skia.blur_backdrop(
        size.w,
        size.h,
        &rects,
        BLUR_SIGMA_LOGICAL * scale as f32,
        flip_y,
    );
}

fn draw_layer(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    surface: &WlSurface,
    origin: (i32, i32),
    in_view: bool,
) -> Result<(), SwapBuffersError> {
    // Layer clients render at `dpi`, so their buffers land 1:1 like apps.
    let scale = ctx.app_scale;
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(renderer, surface, origin, scale, 1.0, Kind::Unspecified);
    if elements.is_empty() {
        return Ok(());
    }
    // App popups live in the view; layers are always panel-upright.
    let (transform, damage) = if in_view {
        (ctx.base(), Rectangle::from_size(ctx.view(size)))
    } else {
        (ctx.transform, Rectangle::from_size(size))
    };
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, scale, &elements, &[damage]) {
        warn!(?e, "failed to draw layer surface");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// A shrunken app card in physical px.
#[derive(Clone, Copy, Debug)]
struct Card {
    x: i32,
    y: i32,
    /// 1.0 = fullscreen.
    scale: f32,
    /// Physical px. `0` uses the default program.
    corner_radius: f32,
    /// For the shader's SDF.
    size: (f32, f32),
}

impl Card {
    fn centered(
        output: Size<i32, Physical>,
        center_x: f32,
        center_y: f32,
        scale: f32,
        corner_radius: f32,
    ) -> Self {
        let w = output.w as f32 * scale;
        let h = output.h as f32 * scale;
        Card {
            x: (center_x - w / 2.0) as i32,
            y: (center_y - h / 2.0) as i32,
            scale,
            corner_radius,
            size: (w, h),
        }
    }

    /// Not `Transform::invert()`: as rect mappings the flipped quarter-turns are
    /// involutions, and `invert()` pairs them, mis-mapping everything composed
    /// with winit's `Flipped180`. The round-trip test keeps this honest.
    fn inverse_transform(t: Transform) -> Transform {
        match t {
            Transform::_90 => Transform::_270,
            Transform::_270 => Transform::_90,
            other => other,
        }
    }

    /// A card drawn with `card_t` in a view drawn with `base` (a landscape app in
    /// an upright deck, or vice versa). The slot stays where [`Card::centered`]
    /// put it; only the space changes. The rect goes through the output
    /// transform and back through the composed one, since winit (`Flipped180`)
    /// and DRM (`Normal`) disagree and a hardcoded mapping mirrors on one.
    #[allow(clippy::too_many_arguments)]
    fn placed(
        view: Size<i32, Physical>,
        center_x: f32,
        center_y: f32,
        scale: f32,
        corner_radius: f32,
        base: Transform,
        card_t: Transform,
    ) -> Self {
        let slot = Card::centered(view, center_x, center_y, scale, corner_radius);
        if card_t == base {
            return slot;
        }
        let rect: Rectangle<i32, Physical> = Rectangle::new(
            slot.origin(),
            (slot.size.0 as i32, slot.size.1 as i32).into(),
        );
        let framebuffer = base.transform_size(view);
        let on_screen = base.transform_rect_in(rect, &view);
        let turned = Card::inverse_transform(card_t).transform_rect_in(on_screen, &framebuffer);
        Card {
            x: turned.loc.x,
            y: turned.loc.y,
            scale,
            corner_radius,
            size: (turned.size.w as f32, turned.size.h as f32),
        }
    }

    fn origin(&self) -> Point<i32, Physical> {
        Point::<i32, Physical>::from((self.x, self.y))
    }

    /// Where the root surface actually lands: a backgrounded toplevel keeps its
    /// configured size, so it may be smaller or offset from the card. Shadow,
    /// scrim and the shader mask all use this. The root is `elements.last()`:
    /// the slice is front-to-back.
    fn drawn_bounds(
        &self,
        elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
        app_scale: f64,
    ) -> (f32, f32, f32, f32) {
        let Some(root) = elements.last() else {
            return (self.x as f32, self.y as f32, self.size.0, self.size.1);
        };
        let g = root.geometry(Scale::from(app_scale));
        (
            self.x as f32 + g.loc.x as f32 * self.scale,
            self.y as f32 + g.loc.y as f32 * self.scale,
            g.size.w as f32 * self.scale,
            g.size.h as f32 * self.scale,
        )
    }

    /// The drawn rect in `gl_FragCoord` terms (origin bottom-left), via the same
    /// projection `GlesFrame::render` builds. Hand-rolling "transform then flip"
    /// is right on winit and upside-down on DRM.
    fn fb_rect(
        &self,
        elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
        app_scale: f64,
        pass_transform: Transform,
        fb_size: Size<i32, Physical>,
    ) -> [f32; 4] {
        let (x, y, w, h) = self.drawn_bounds(elements, app_scale);
        // Axis-swapped for a quarter turn, as `GlesFrame::render` does.
        let elem = pass_transform.transform_size(fb_size);
        let (ew, eh) = (elem.w as f32, elem.h as f32);
        let (fw, fh) = (fb_size.w as f32, fb_size.h as f32);
        let m = pass_transform.matrix().to_cols_array();

        let project = |px: f32, py: f32| -> (f32, f32) {
            let (nx, ny) = (2.0 * px / ew - 1.0, 1.0 - 2.0 * py / eh);
            let tx = m[0] * nx + m[2] * ny + m[4];
            let ty = m[1] * nx + m[3] * ny + m[5];
            // GL's y flip, after the transform like the renderer's `flip180`.
            ((tx + 1.0) * 0.5 * fw, (-ty + 1.0) * 0.5 * fh)
        };

        let (x0, y0) = project(x, y);
        let (x1, y1) = project(x + w, y + h);
        [x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs()]
    }
}

/// Relocate and rescale the tree to the card, then draw it. Rounded cards go
/// through the shader in one `draw_render_elements` call: the slice is
/// front-to-back and reversed internally, so splitting it drew waydroid's
/// opaque root over its content (a black card). Every element gets the mask,
/// since it's in framebuffer coords.
fn draw_scaled_card(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    card: Card,
    pass_transform: Transform,
) -> Result<(), SwapBuffersError> {
    if elements.is_empty() {
        return Ok(());
    }
    let app_scale = ctx.app_scale;
    // Before the elements are consumed.
    let card_rect = card.fb_rect(&elements, app_scale, pass_transform, size);
    let damage = Rectangle::from_size(pass_transform.transform_size(size));
    let scaled: Vec<
        RescaleRenderElement<RelocateRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    > = elements
        .into_iter()
        .map(|e| {
            let relocated =
                RelocateRenderElement::from_element(e, card.origin(), Relocate::Relative);
            RescaleRenderElement::from_element(
                relocated,
                card.origin(),
                Scale::from(card.scale as f64),
            )
        })
        .collect();

    let mut frame = renderer
        .render(framebuffer, size, pass_transform)
        .map_err(SwapBuffersError::from)?;

    if card.corner_radius > 0.5 {
        let uniforms = vec![
            Uniform::new("corner_radius", card.corner_radius),
            Uniform::new("card_rect", card_rect),
        ];
        frame.override_default_tex_program(ctx.rounded_tex_shader.clone(), uniforms);
    }
    if let Err(e) = draw_render_elements(&mut frame, app_scale, &scaled, &[damage]) {
        warn!(?e, "failed to draw scaled card elements");
    }
    frame.clear_tex_program_override();

    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Element collection borrows the renderer, so it happens before any pass.
struct ScenePlan {
    app_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    below_elements: Vec<Vec<WaylandSurfaceRenderElement<GlesRenderer>>>,
    /// `None` when fullscreen or absent.
    window_transform: Option<crate::scene::WindowTransform>,
    is_fullscreen: bool,
    rotated: bool,
    /// A fullscreen-scaled window with no content yet must still show Home.
    app_fills_screen: bool,
    app_blur: Vec<crate::background_effect::BlurRect>,
    /// Translucent, so Home is drawn and blurred behind it.
    app_blurred: bool,
}

fn plan_scene(renderer: &mut GlesRenderer, ctx: &DrawCtx<'_>) -> ScenePlan {
    let scene = ctx.scene;
    let is_fullscreen = scene.window_covers_screen();
    let rotated = ctx.rotation.swaps_axes();

    let app_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        if let Some(wl_surface) = ctx.app_surface {
            render_elements_from_surface_tree(
                renderer,
                wl_surface,
                if is_fullscreen {
                    ctx.app_origin
                } else {
                    (0, 0)
                },
                ctx.app_scale,
                1.0,
                Kind::Unspecified,
            )
        } else {
            Vec::new()
        };

    let below_elements: Vec<Vec<WaylandSurfaceRenderElement<GlesRenderer>>> = ctx
        .layers_below
        .iter()
        .map(|(surface, origin)| {
            render_elements_from_surface_tree(
                renderer,
                surface,
                *origin,
                ctx.app_scale,
                1.0,
                Kind::Unspecified,
            )
        })
        .filter(|e| !e.is_empty())
        .collect();

    let app_fills_screen = is_fullscreen && !app_elements.is_empty();
    let app_blur = ctx
        .app_surface
        .map(|s| crate::background_effect::blur_rects(s, ctx.app_origin, ctx.app_scale))
        .unwrap_or_default();

    ScenePlan {
        app_blurred: app_fills_screen && !app_blur.is_empty(),
        app_elements,
        below_elements,
        window_transform: scene.window.as_ref().map(|(_, t)| *t),
        is_fullscreen,
        rotated,
        app_fills_screen,
        app_blur,
    }
}

/// Full output by default. Narrowed only for a quiet fullscreen app that is a
/// single element at the origin. Any blur region on screen disqualifies it.
fn flip_damage(
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    plan: &ScenePlan,
) -> Vec<Rectangle<i32, Physical>> {
    let full_damage = Rectangle::from_size(size);
    let any_blur = plan.app_blurred
        || ctx.scene.backdrop_blur > 0.0
        || ctx
            .layers_above
            .iter()
            .chain(ctx.app_popups)
            .chain(ctx.layer_popups)
            .any(|(surface, _)| crate::background_effect::has_blur_region(surface));

    if !(ctx.report_partial_damage
        && !any_blur
        && plan.app_fills_screen
        && plan.app_elements.len() == 1)
    {
        return vec![full_damage];
    }

    let app_wl = ctx
        .app_surface
        .expect("app_fills_screen implies app_surface");
    let elem = &plan.app_elements[0];
    let same_surface = ctx.last_present.as_ref().is_some_and(|(s, _)| s == app_wl);
    let since = same_surface
        .then(|| ctx.last_present.as_ref().map(|(_, c)| *c))
        .flatten();
    let damage = elem.damage_since(Scale::from(ctx.app_scale), since);
    *ctx.last_present = Some((app_wl.clone(), elem.current_commit()));
    // No baseline on a newly focused surface: repaint all.
    if same_surface {
        let turn = ctx.rotation.transform();
        let view = ctx.view(size);
        damage
            .iter()
            .map(|r| turn.transform_rect_in(*r, &view))
            .collect()
    } else {
        vec![full_damage]
    }
}

/// Pass 1: clear, below-app layers, and the app if opaquely fullscreen.
/// Rotated or blurred apps get their own passes.
fn pass_background(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    plan: &ScenePlan,
) -> Result<(), SwapBuffersError> {
    let damage = Rectangle::from_size(size);
    let mut frame = renderer
        .render(framebuffer, size, ctx.transform)
        .map_err(SwapBuffersError::from)?;
    frame
        .clear(CLEAR_COLOR, &[damage])
        .map_err(SwapBuffersError::from)?;

    for elements in &plan.below_elements {
        if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, elements, &[damage]) {
            warn!(?e, "failed to draw background layer surface");
        }
    }

    if plan.app_fills_screen && !plan.app_blurred && !plan.rotated {
        if let Err(e) =
            draw_render_elements(&mut frame, ctx.app_scale, &plan.app_elements, &[damage])
        {
            warn!(?e, "failed to draw app elements");
        }
    }

    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Skipped once the app has actually covered the screen, or Home flashes
/// over it before the state settles. Gating on scale alone blanks the app
/// before its first frame. A blurred app needs Home even in `App`.
fn pass_home(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>, plan: &ScenePlan) {
    let scene = ctx.scene;
    if !(plan.app_blurred || (scene.show_home && !plan.app_fills_screen)) {
        return;
    }
    let view = HomeView {
        page: scene.home_page,
        model: ctx.model,
        icon_cache: ctx.icon_cache,
        app_catalog: ctx.app_catalog,
        pressed_app: ctx.pressed_app,
        launch_pulses: ctx.launch_pulses,
        running_apps: ctx.running_apps,
        arrange: ctx.arrange.as_ref(),
        grid_positions: ctx.grid_positions,
        dock_positions: ctx.dock_positions,
        library: ctx.library,
        top_inset: ctx.app_origin.1 as f32,
        lift: scene.home_lift,
        shift: scene.home_shift,
    };
    let v = ctx.view(size);
    ctx.skia.draw_home(v.w, v.h, ctx.skia_flip_y, &view);
}

/// Only when Home itself is drawn.
fn pass_icon_menu(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>, plan: &ScenePlan) {
    let Some(menu) = ctx.icon_menu else {
        return;
    };
    if !ctx.scene.show_home || plan.app_fills_screen {
        return;
    }
    let v = ctx.view(size);
    ctx.skia.draw_icon_menu(v.w, v.h, menu, ctx.skia_flip_y);
}

fn pass_folder(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>, plan: &ScenePlan) {
    let Some(folder) = ctx.folder else {
        return;
    };
    if !ctx.scene.show_home || plan.app_fills_screen {
        return;
    }
    let (icon_cache, app_catalog) = (ctx.icon_cache, ctx.app_catalog);
    let v = ctx.view(size);
    ctx.skia
        .draw_folder_panel(v.w, v.h, folder, icon_cache, app_catalog, ctx.skia_flip_y);
}

/// `pass_background` draws panel-upright for the layers, so a turned app
/// needs its own pass.
fn pass_rotated_app(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    plan: &ScenePlan,
) -> Result<(), SwapBuffersError> {
    // Blurred apps are drawn by `pass_blurred_app`; the three fullscreen-app
    // passes must stay exclusive or a rotated ghost appears.
    if !(plan.app_fills_screen && plan.rotated) || plan.app_blurred {
        return Ok(());
    }
    let (transform, app_damage) = app_pass_geometry(size, ctx);
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) =
        draw_render_elements(&mut frame, ctx.app_scale, &plan.app_elements, &[app_damage])
    {
        warn!(?e, "failed to draw rotated app elements");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

fn app_pass_geometry(
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
) -> (Transform, Rectangle<i32, Physical>) {
    (ctx.base(), Rectangle::from_size(ctx.view(size)))
}

fn pass_blurred_app(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    plan: &ScenePlan,
) -> Result<(), SwapBuffersError> {
    if !plan.app_blurred {
        return Ok(());
    }
    let v = ctx.view(size);
    ctx.skia.blur_backdrop(
        v.w,
        v.h,
        &plan.app_blur,
        BLUR_SIGMA_LOGICAL * ctx.app_scale as f32,
        ctx.skia_flip_y,
    );
    let (transform, damage) = app_pass_geometry(size, ctx);
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, &plan.app_elements, &[damage]) {
        warn!(?e, "failed to draw blurred app elements");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Portrait if not a tracked toplevel.
fn surface_rotation(ctx: &DrawCtx<'_>, surface: &WlSurface) -> crate::rotation::Rotation {
    ctx.toplevels
        .iter()
        .flatten()
        .find(|tl| tl.surface.wl_surface() == surface)
        .map(|tl| tl.rotation)
        .unwrap_or_default()
}

/// Physical px.
fn drawn_size(
    elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
    app_scale: f64,
) -> Option<Size<i32, Physical>> {
    let root = elements.first()?;
    Some(root.geometry(Scale::from(app_scale)).size)
}

/// The stored rotation only says which way; the committed buffer decides
/// whether. A background window can be reconfigured back to portrait by any
/// layer change, and turning it then breaks the card.
fn card_rotation(
    stored: crate::rotation::Rotation,
    buffer: Option<Size<i32, Physical>>,
    output: Size<i32, Physical>,
) -> crate::rotation::Rotation {
    let Some(buffer) = buffer else {
        return crate::rotation::Rotation::None;
    };
    let turned = buffer.w != buffer.h
        && output.w != output.h
        && (buffer.w > buffer.h) != (output.w > output.h);
    if stored.swaps_axes() && turned {
        stored
    } else {
        crate::rotation::Rotation::None
    }
}

/// Independent of the view's rotation.
fn card_transform(
    ctx: &DrawCtx<'_>,
    stored: crate::rotation::Rotation,
    elements: &[WaylandSurfaceRenderElement<GlesRenderer>],
    size: Size<i32, Physical>,
) -> Transform {
    ctx.transform + card_rotation(stored, drawn_size(elements, ctx.app_scale), size).transform()
}

/// Pass 2: the scaled app card over Home. Consumes `plan.app_elements`.
fn pass_app_card(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    plan: &mut ScenePlan,
) -> Result<(), SwapBuffersError> {
    if plan.is_fullscreen || plan.app_elements.is_empty() {
        return Ok(());
    }
    let Some(t) = plan.window_transform else {
        return Ok(());
    };
    // The card follows the window's orientation, not necessarily the view's.
    let card_t = card_transform(
        ctx,
        ctx.app_surface
            .map(|s| surface_rotation(ctx, s))
            .unwrap_or_default(),
        &plan.app_elements,
        size,
    );
    let upright = card_t == ctx.base();
    let card = Card::placed(
        ctx.view(size),
        t.center_x,
        t.center_y,
        t.scale,
        t.corner_radius,
        ctx.base(),
        card_t,
    );
    // Skipped for a card turned against the view: its rect isn't in Skia's
    // view space.
    if let (Some(surface), true) = (ctx.app_surface, upright) {
        blur_behind(
            ctx.skia,
            ctx.view(size),
            surface,
            (card.x, card.y),
            ctx.app_scale * t.scale as f64,
            ctx.skia_flip_y,
        );
    }
    if upright {
        let (x, y, w, h) = card.drawn_bounds(&plan.app_elements, ctx.app_scale);
        ctx.pill_anchor = Some(sc_layout::Rect { x, y, w, h });
    }
    let elements = std::mem::take(&mut plan.app_elements);
    draw_scaled_card(renderer, framebuffer, size, ctx, elements, card, card_t)
}

/// Frost Home before the deck. Ramps with `scene.backdrop_blur`; popping to
/// full reads as a cut.
fn pass_backdrop_blur(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>) {
    let strength = ctx.scene.backdrop_blur.clamp(0.0, 1.0);
    if strength <= 0.0 {
        return;
    }
    let v = ctx.view(size);
    let full = [crate::background_effect::BlurRect {
        x: 0.0,
        y: 0.0,
        w: v.w as f32,
        h: v.h as f32,
        add: true,
    }];
    ctx.skia.blur_backdrop(
        v.w,
        v.h,
        &full,
        BACKDROP_BLUR_SIGMA_LOGICAL * ctx.app_scale as f32 * strength,
        ctx.skia_flip_y,
    );
}

/// Back-to-front. Each card sits between its Skia shadow and scrim, so the
/// shadow lands over the card behind and under this one.
fn pass_switcher_cards(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
) -> Result<(), SwapBuffersError> {
    let cards: Vec<crate::switcher::CardRect> = ctx.scene.cards.clone();
    for card in cards {
        let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) else {
            continue;
        };
        let surface = tl.surface.wl_surface().clone();
        let stored = tl.rotation;
        let app_id = tl.app_id.clone();
        // Whether a card is turned depends on its buffer, so collect first.
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                &surface,
                (0, 0),
                ctx.app_scale,
                card.alpha,
                Kind::Unspecified,
            );
        if elements.is_empty() {
            continue;
        }
        let card_t = card_transform(ctx, stored, &elements, size);
        let upright = card_t == ctx.base();
        let view = ctx.view(size);
        let placement = Card::placed(
            view,
            card.center_x,
            card.center_y,
            card.scale,
            card.corner_radius,
            ctx.base(),
            card_t,
        );
        // Use the real drawn bounds: a backgrounded app keeps its old size. A card
        // turned against the view fills its slot, and its placement is in its own
        // space, so use the view-space slot.
        let (dx, dy, dw, dh) = if upright {
            placement.drawn_bounds(&elements, ctx.app_scale)
        } else {
            let slot = Card::centered(
                view,
                card.center_x,
                card.center_y,
                card.scale,
                card.corner_radius,
            );
            (slot.x as f32, slot.y as f32, slot.size.0, slot.size.1)
        };
        if upright {
            // Ascending z: the front card's rect is what's left for the pill.
            ctx.pill_anchor = Some(sc_layout::Rect {
                x: dx,
                y: dy,
                w: dw,
                h: dh,
            });
        }
        let decor = crate::skia_gl::CardDecor {
            x: dx,
            y: dy,
            w: dw,
            h: dh,
            radius: placement.corner_radius,
            alpha: card.alpha,
            dim: card.dim,
            chrome: ctx.card_chrome.icon_alpha,
            dpi: ctx.app_scale as f32,
        };
        ctx.skia
            .draw_card_shadow(view.w, view.h, &decor, ctx.skia_flip_y);
        draw_scaled_card(
            renderer,
            framebuffer,
            size,
            ctx,
            elements,
            placement,
            card_t,
        )?;
        ctx.skia
            .draw_card_dim(view.w, view.h, &decor, ctx.skia_flip_y);
        ctx.skia.draw_card_icon(
            view.w,
            view.h,
            &decor,
            &app_id,
            ctx.icon_cache,
            ctx.skia_flip_y,
        );
        if let Some((tid, title)) = &ctx.card_chrome.title {
            if *tid == card.toplevel {
                ctx.skia.draw_card_title(
                    view.w,
                    view.h,
                    &decor,
                    title,
                    card.alpha * ctx.card_chrome.title_alpha,
                    ctx.skia_flip_y,
                );
            }
        }
    }
    Ok(())
}

/// Back-to-front: app popups (in the view), top/overlay layers, then their
/// popups. Layers and their popups are hidden while turned;
/// `touch::surface_under` skips them too.
fn pass_overlays(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    rotated: bool,
) -> Result<(), SwapBuffersError> {
    let (app_popups, layers_above, layer_popups) =
        (ctx.app_popups, ctx.layers_above, ctx.layer_popups);
    for (surface, origin) in app_popups {
        draw_overlay(renderer, framebuffer, size, ctx, surface, *origin, true)?;
    }
    if rotated {
        return Ok(());
    }
    for (surface, origin) in layers_above {
        draw_overlay(renderer, framebuffer, size, ctx, surface, *origin, false)?;
    }
    // The hidden OSK is gone from the render lists; draw its held buffer.
    if let Some((buffer, rect)) = ctx.closing {
        draw_closing(renderer, framebuffer, size, ctx, buffer, rect)?;
    }
    for (surface, origin) in layer_popups {
        draw_overlay(renderer, framebuffer, size, ctx, surface, *origin, false)?;
    }
    Ok(())
}

fn draw_overlay(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    surface: &WlSurface,
    origin: (i32, i32),
    in_view: bool,
) -> Result<(), SwapBuffersError> {
    blur_behind(
        ctx.skia,
        ctx.view(size),
        surface,
        origin,
        ctx.app_scale,
        ctx.skia_flip_y,
    );
    draw_layer(renderer, framebuffer, size, ctx, surface, origin, in_view)
}

/// A full upload every frame (the buffer may belong to a destroyed surface,
/// ruling out smithay's cache); ~0.5ms for the ~0.18s slide. Non-dmabuf
/// buffers fail the import and just vanish.
fn draw_closing(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    buffer: &smithay::backend::renderer::utils::Buffer,
    rect: sc_layout::Rect,
) -> Result<(), SwapBuffersError> {
    let texture = match buffer_type(buffer) {
        Some(BufferType::Shm) => renderer.import_shm_buffer(buffer, None, &[]).ok(),
        Some(BufferType::Dma) => renderer.import_dma_buffer(buffer, None, &[]).ok(),
        _ => None,
    };
    let Some(texture) = texture else {
        return Ok(());
    };
    // 1:1: the layer client renders at `dpi`.
    let elements = vec![TextureRenderElement::from_static_texture(
        ElementId::new(),
        renderer.context_id(),
        (rect.x as f64, rect.y as f64),
        texture,
        1,
        Transform::Normal,
        Some(1.0),
        None,
        None,
        None,
        Kind::Unspecified,
    )];
    let mut frame = renderer
        .render(framebuffer, size, ctx.transform)
        .map_err(SwapBuffersError::from)?;
    let damage = [Rectangle::from_size(size)];
    if let Err(e) = draw_render_elements::<
        GlesRenderer,
        f64,
        TextureRenderElement<smithay::backend::renderer::gles::GlesTexture>,
    >(&mut frame, 1.0, &elements, &damage)
    {
        warn!(?e, "failed to draw closing layer surface");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

fn pass_chrome(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>) {
    let size = ctx.view(size);
    // Always on top: it's the only way out of a fullscreen app.
    ctx.skia.draw_bar_overlay(
        size.w,
        size.h,
        ctx.bar_alpha,
        ctx.skia_flip_y,
        ctx.pill_anchor,
    );

    if let Some((level, muted, alpha)) = ctx.osd {
        ctx.skia
            .draw_osd_overlay(size.w, size.h, level, muted, alpha, ctx.skia_flip_y);
    }

    if !ctx.touches.is_empty() {
        ctx.skia
            .draw_touches_overlay(size.w, size.h, ctx.touches, ctx.skia_flip_y);
    }

    if let Some((x, y)) = ctx.cursor {
        ctx.skia
            .draw_cursor(size.w, size.h, x, y, ctx.app_scale as f32, ctx.skia_flip_y);
    }
}

/// Every drawn surface gets a callback, including switcher cards, or
/// backgrounded clients stop presenting and their cards go blank. The same
/// surfaces have their presentation feedback taken.
fn send_frame_callbacks(ctx: &mut DrawCtx<'_>) {
    let mut drawn: Vec<&WlSurface> = Vec::new();
    if let Some(wl_surface) = ctx.app_surface {
        drawn.push(wl_surface);
    }
    for card in &ctx.scene.cards {
        if let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) {
            drawn.push(tl.surface.wl_surface());
        }
    }
    drawn.extend(
        ctx.layers_below
            .iter()
            .chain(ctx.layers_above)
            .chain(ctx.app_popups)
            .chain(ctx.layer_popups)
            .map(|(surface, _)| surface),
    );

    for surface in drawn {
        send_frames_surface_tree(surface, ctx.frame_time);
        crate::presentation::take_feedback(surface, &mut ctx.sinks.presented);
        // Its content is in this frame, so the fifo barrier is honoured.
        crate::pacing::signal_fifo(surface, &mut ctx.sinks.unblocked);
        crate::pacing::signal_commit_timers(surface, ctx.frame_target, &mut ctx.sinks.unblocked);
    }
}

/// Returns the KMS damage hint; presenting is the caller's job. Passes run
/// strictly back-to-front; the blur passes sample what's already drawn.
pub fn draw_scene(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
) -> Result<Vec<Rectangle<i32, Physical>>, SwapBuffersError> {
    // Before planning, so no client content can reach the framebuffer.
    if ctx.lock_view != crate::session_lock::LockView::Unlocked {
        return draw_locked(renderer, framebuffer, size, ctx);
    }

    ctx.skia.sync_catalog_gen(ctx.catalog_gen);
    ctx.skia.set_view(ctx.rotation);

    let mut plan = plan_scene(renderer, ctx);
    // Before drawing: reads the pre-draw commit counter.
    let damage_hint = flip_damage(size, ctx, &plan);

    pass_background(renderer, &mut *framebuffer, size, ctx, &plan)?;
    pass_home(size, ctx, &plan);
    pass_folder(size, ctx, &plan);
    pass_icon_menu(size, ctx, &plan);
    // Before any card pass, or a dragged card gets blurred too.
    pass_backdrop_blur(size, ctx);
    pass_rotated_app(renderer, &mut *framebuffer, size, ctx, &plan)?;
    pass_blurred_app(renderer, &mut *framebuffer, size, ctx, &plan)?;
    pass_app_card(renderer, &mut *framebuffer, size, ctx, &mut plan)?;
    pass_switcher_cards(renderer, &mut *framebuffer, size, ctx)?;
    pass_overlays(renderer, &mut *framebuffer, size, ctx, plan.rotated)?;
    pass_chrome(size, ctx);
    let v = ctx.view(size);
    ctx.skia.draw_screen_dim(v.w, v.h, ctx.dim, ctx.skia_flip_y);

    send_frame_callbacks(ctx);
    Ok(damage_hint)
}

/// Black plus the lock surface. Only the OSD, touch marks and cursor are
/// kept; none shows session content. Always full damage.
fn draw_locked(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
) -> Result<Vec<Rectangle<i32, Physical>>, SwapBuffersError> {
    let damage = Rectangle::from_size(size);
    ctx.skia.set_view(crate::rotation::Rotation::None);

    // Configured at the output's logical size, so drawn from the origin.
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = ctx
        .lock_surface
        .map(|surface| {
            render_elements_from_surface_tree(
                renderer,
                surface,
                (0, 0),
                ctx.app_scale,
                1.0,
                Kind::Unspecified,
            )
        })
        .unwrap_or_default();

    let mut frame = renderer
        .render(framebuffer, size, ctx.transform)
        .map_err(SwapBuffersError::from)?;
    // Black, not `CLEAR_COLOR`: an idle-looking backdrop would hide a dead
    // lock client.
    frame
        .clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[damage])
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, &elements, &[damage]) {
        warn!(?e, "failed to draw lock surface");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;

    if let Some((level, muted, alpha)) = ctx.osd {
        ctx.skia
            .draw_osd_overlay(size.w, size.h, level, muted, alpha, ctx.skia_flip_y);
    }
    if !ctx.touches.is_empty() {
        ctx.skia
            .draw_touches_overlay(size.w, size.h, ctx.touches, ctx.skia_flip_y);
    }
    if let Some((x, y)) = ctx.cursor {
        ctx.skia
            .draw_cursor(size.w, size.h, x, y, ctx.app_scale as f32, ctx.skia_flip_y);
    }

    if let Some(surface) = ctx.lock_surface {
        send_frames_surface_tree(surface, ctx.frame_time);
    }
    Ok(vec![damage])
}

pub fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::Rotation;

    fn output() -> Size<i32, Physical> {
        Size::from((1224, 2700))
    }

    fn on_framebuffer(card: &Card, transform: Transform) -> Rectangle<i32, Physical> {
        let rect: Rectangle<i32, Physical> = Rectangle::new(
            card.origin(),
            (card.size.0 as i32, card.size.1 as i32).into(),
        );
        let element_space = transform.invert().transform_size(output());
        transform.transform_rect_in(rect, &element_space)
    }

    /// winit renders `Flipped180`, DRM `Normal`; a mapping right for one draws
    /// mirrored on the other.
    const OUT_TRANSFORMS: [Transform; 2] = [Transform::Normal, Transform::Flipped180];

    #[test]
    fn a_turned_card_lands_on_the_same_pixels_as_its_upright_slot() {
        // Off-centre: a centred card passes even with inverted handedness.
        let (cx, cy, scale) = (500.0_f32, 900.0_f32, 0.62_f32);
        for out in OUT_TRANSFORMS {
            let slot = Card::centered(output(), cx, cy, scale, 40.0);
            let want = on_framebuffer(&slot, out);
            for rotation in [Rotation::LeftUp, Rotation::RightUp] {
                let card_t = out + rotation.transform();
                let turned = Card::placed(output(), cx, cy, scale, 40.0, out, card_t);
                let got = on_framebuffer(&turned, card_t);
                assert!(
                    (got.loc.x - want.loc.x).abs() <= 1 && (got.loc.y - want.loc.y).abs() <= 1,
                    "{out:?} + {rotation:?}: landed {:?}, slot {:?}",
                    got.loc,
                    want.loc
                );
                assert!(
                    (got.size.w - want.size.w).abs() <= 1 && (got.size.h - want.size.h).abs() <= 1,
                    "{out:?} + {rotation:?}: size {:?} vs {:?}",
                    got.size,
                    want.size
                );
            }
        }
    }

    #[test]
    fn a_turned_card_is_the_slot_swapped_into_app_space() {
        let (cx, cy, scale) = (500.0_f32, 900.0_f32, 0.62_f32);
        let slot = Card::centered(output(), cx, cy, scale, 40.0);
        for out in OUT_TRANSFORMS {
            for rotation in [Rotation::LeftUp, Rotation::RightUp] {
                let turned = Card::placed(
                    output(),
                    cx,
                    cy,
                    scale,
                    40.0,
                    out,
                    out + rotation.transform(),
                );
                assert!(
                    (turned.size.0 - slot.size.1).abs() <= 1.0
                        && (turned.size.1 - slot.size.0).abs() <= 1.0,
                    "{out:?} + {rotation:?}: turned {:?} vs slot {:?}",
                    turned.size,
                    slot.size
                );
            }
        }
    }

    #[test]
    fn the_inverse_table_really_inverts_every_transform() {
        // Guards `Card::inverse_transform` against `Transform::invert()`.
        let area = output();
        let rect: Rectangle<i32, Physical> = Rectangle::new((37, 91).into(), (240, 410).into());
        for t in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let mapped = t.transform_rect_in(rect, &area);
            let back =
                Card::inverse_transform(t).transform_rect_in(mapped, &t.transform_size(area));
            assert_eq!(back, rect, "{t:?} did not round-trip");
        }
    }

    #[test]
    fn a_card_turns_only_while_its_buffer_is_actually_landscape() {
        let portrait = output();
        let landscape: Size<i32, Physical> = Size::from((2700, 1224));
        assert_eq!(
            card_rotation(Rotation::LeftUp, Some(landscape), portrait),
            Rotation::LeftUp
        );
        // Reconfigured back to portrait while the stored value says LeftUp.
        assert_eq!(
            card_rotation(Rotation::LeftUp, Some(portrait), portrait),
            Rotation::None
        );
        assert_eq!(
            card_rotation(Rotation::None, Some(landscape), portrait),
            Rotation::None
        );
        assert_eq!(
            card_rotation(Rotation::LeftUp, None, portrait),
            Rotation::None
        );
    }

    #[test]
    fn an_upright_card_is_untouched_by_the_rotated_constructor() {
        let plain = Card::centered(output(), 700.0, 900.0, 0.5, 24.0);
        let same = Card::placed(
            output(),
            700.0,
            900.0,
            0.5,
            24.0,
            Transform::Normal,
            Transform::Normal,
        );
        assert_eq!((plain.x, plain.y), (same.x, same.y));
        assert_eq!(plain.size, same.size);
    }

    #[test]
    fn an_upright_card_in_a_turned_view_lands_on_its_view_slot() {
        let (cx, cy, scale) = (900.0_f32, 500.0_f32, 0.62_f32);
        for out in OUT_TRANSFORMS {
            for rotation in [Rotation::LeftUp, Rotation::RightUp] {
                let base = out + rotation.transform();
                let view = rotation.app_size((output().w, output().h)).into();
                let slot = Card::centered(view, cx, cy, scale, 40.0);
                let want = on_framebuffer(&slot, base);
                let card = Card::placed(view, cx, cy, scale, 40.0, base, out);
                let got = on_framebuffer(&card, out);
                assert!(
                    (got.loc.x - want.loc.x).abs() <= 1
                        && (got.loc.y - want.loc.y).abs() <= 1
                        && (got.size.w - want.size.w).abs() <= 1
                        && (got.size.h - want.size.h).abs() <= 1,
                    "{out:?} + {rotation:?}: landed {got:?}, slot {want:?}",
                );
            }
        }
    }
}
