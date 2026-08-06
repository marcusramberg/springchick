//! Shared render path for the winit and DRM backends.
//!
//! Both backends own a `GlesRenderer` and differ only in how they acquire the
//! framebuffer (`bind`) and present (`submit`/page-flip). Everything between —
//! clearing, the transformed two-pass app composite, and the Skia home/bar
//! overlay — is identical and lives here.

use std::collections::HashMap;

use crate::scene::Scene;
use crate::skia_gl::SkiaGl;

use sc_catalog::AppEntry;
use sc_icons::IconPixels;
use sc_shell_model::ShellModel;

use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, Kind};
use smithay::backend::renderer::gles::{
    GlesError, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType,
};
use smithay::backend::renderer::utils::{draw_render_elements, CommitCounter};
use smithay::backend::renderer::{Color32F, Frame, Renderer, RendererSuper};
use smithay::backend::SwapBuffersError;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{
    with_surface_tree_downward, SurfaceAttributes, TraversalAction,
};

use tracing::warn;

/// Blur radius for ext-background-effect blur regions, in logical px (scaled by
/// output dpi at use). Roughly matches the frosted-glass look phone panels draw
/// against; the protocol leaves the algorithm entirely to the compositor.
const BLUR_SIGMA_LOGICAL: f32 = 8.0;

/// Background clear color.
pub const CLEAR_COLOR: Color32F = Color32F::new(0.06, 0.10, 0.14, 1.0);

/// Rounded-rect texture fragment shader. Derived verbatim from smithay's default
/// `texture.frag` (pin ff5fa7d) — same `//_DEFINES_` placeholder and
/// `NO_ALPHA`/`EXTERNAL`/`DEBUG_FLAGS` variants — with two extra uniforms
/// (`corner_radius`, `card_size`) and a rounded-rect signed-distance-field mask
/// applied to the premultiplied output. Mask is computed in card-local physical
/// px via `v_coords` (0..1 spans the drawn card), so it is independent of the
/// framebuffer orientation.
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
uniform vec2 card_size;

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
    vec2 p = v_coords * card_size;
    vec2 half_size = card_size * 0.5;
    vec2 q = abs(p - half_size) - (half_size - vec2(corner_radius));
    float dist = length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - corner_radius;
    float aa = max(0.5, min(card_size.x, card_size.y) * 0.004);
    float mask = 1.0 - smoothstep(-aa, aa, dist);
    color *= mask;

    gl_FragColor = color;
}
"#;

/// Compile the rounded-corner texture shader. Each backend calls this once,
/// right after it creates its `GlesRenderer`, and stores the resulting program
/// to hand to `draw_scene` via `DrawCtx::rounded_tex_shader`.
pub fn compile_rounded_tex_shader(
    renderer: &mut GlesRenderer,
) -> Result<GlesTexProgram, GlesError> {
    renderer.compile_custom_texture_shader(
        ROUNDED_TEX_SHADER,
        &[
            UniformName::new("corner_radius", UniformType::_1f),
            UniformName::new("card_size", UniformType::_2f),
        ],
    )
}

/// Render-only view of arrange-mode drag state, threaded through `DrawCtx`
/// the same way `pressed_app` is: `main.rs`/`drm_backend.rs` derive it from
/// live compositor state (`State::arrange`), and `draw_home` only reads it.
pub struct ArrangeView<'a> {
    /// App id currently being dragged, if any.
    pub drag_app: Option<&'a str>,
    /// Live finger/pointer position for the dragged icon (output pixels).
    pub drag_pos: Option<(f32, f32)>,
    /// Whether the drag position is currently over the dock drop zone.
    pub over_dock: bool,
}

/// Everything the shared draw needs beyond the renderer + framebuffer.
pub struct DrawCtx<'a> {
    pub scene: &'a Scene,
    pub app_surface: Option<&'a WlSurface>,
    pub skia: &'a mut SkiaGl,
    pub model: &'a ShellModel,
    pub icon_cache: &'a HashMap<String, IconPixels>,
    pub app_catalog: &'a HashMap<String, AppEntry>,
    /// Toplevels for switcher card rendering.
    pub toplevels: &'a Vec<Option<crate::AppToplevel>>,
    /// Output scale (`[main].dpi`). App surfaces are configured at physical/dpi
    /// logical size and render an oversized buffer, so their render elements are
    /// generated at this scale to land back at physical size.
    pub app_scale: f64,
    /// Physical top-left where the fullscreen app surface is drawn: the origin
    /// of the usable area (output minus top/left exclusive-zone reservations,
    /// e.g. a top bar). Zero for bottom/right-only reservations.
    pub app_origin: (i32, i32),
    /// Output transform (winit = Flipped180; DRM = connector transform).
    pub transform: Transform,
    /// Rotation of the fullscreen app (see [`crate::rotation`]). Composed on top
    /// of `transform` for the app pass only — chrome stays portrait.
    pub rotation: crate::rotation::Rotation,
    /// Mirror the Skia home/bar vertically — the DRM/GBM scanout buffer has the
    /// opposite Y-origin from Skia's BottomLeft surface. winit presents
    /// already-correct, so false.
    pub skia_flip_y: bool,
    /// Time in ms for frame callbacks.
    pub frame_time: u32,
    /// Volume OSD to overlay: `(level, muted, alpha)`. `None` when inactive.
    pub osd: Option<(f32, bool, f32)>,
    /// Touch indicator marks (physical coords) drawn on top of everything for
    /// demo recordings. Empty unless `[main].show_touches` is set.
    pub touches: &'a [crate::touch_viz::TouchMark],
    /// Session-lock view. Anything but `Unlocked` replaces the entire scene —
    /// see [`draw_locked`].
    pub lock_view: crate::session_lock::LockView,
    /// The lock client's surface, drawn when `lock_view` is `Surface`.
    pub lock_surface: Option<&'a WlSurface>,
    /// Layer-shell surfaces below the app (background/bottom): `(surface, origin)`.
    pub layers_below: &'a [(WlSurface, (i32, i32))],
    /// Layer-shell surfaces above the app (top/overlay): `(surface, origin)`.
    pub layers_above: &'a [(WlSurface, (i32, i32))],
    /// xdg_popups whose root is the fullscreen app, ordered root→leaf. Each is a
    /// popup surface + its clamped physical origin. Drawn above the app, below
    /// the top/overlay layers.
    pub app_popups: &'a [(WlSurface, (i32, i32))],
    /// xdg_popups whose root is a top/overlay layer surface (e.g. an OSK menu),
    /// ordered root→leaf. Drawn above the layers, below springchick chrome.
    pub layer_popups: &'a [(WlSurface, (i32, i32))],
    /// Home-bar opacity (faded out when the OSK covers it).
    pub bar_alpha: f32,
    /// App id of the icon currently pressed on Home (draws a press highlight).
    pub pressed_app: Option<&'a str>,
    /// App id of an icon whose app is launching but hasn't mapped a window yet
    /// (draws a breathing pulse). `None` when nothing is launching.
    pub launching_app: Option<&'a str>,
    /// Seconds since the current launch began — drives the pulse phase.
    pub launching_elapsed: f32,
    /// Arrange-mode view (badges/Done/drag ghost). `None` when arrange mode
    /// is inactive.
    pub arrange: Option<ArrangeView<'a>>,
    /// Screen-space animated center `(x, y)` for each grid app, driven by
    /// `State.grid_anim` springs. Used to render the grid so icons slide to
    /// their reflow targets instead of snapping.
    pub grid_positions: &'a HashMap<String, (f32, f32)>,
    /// Screen-space animated center `(x, y)` for each dock app, driven by
    /// `State.dock_anim` springs. Used to render the dock so icons slide to
    /// their reflow targets instead of snapping.
    pub dock_positions: &'a HashMap<String, (f32, f32)>,
    /// When true, the frame is a "quiet fullscreen app" (nothing but the app
    /// surface can have changed) and `draw_scene` may return a narrowed KMS
    /// page-flip damage hint instead of the full rect. The backend computes
    /// this; when false the hint is always the full output.
    pub report_partial_damage: bool,
    /// Per-app commit cursor for `report_partial_damage`. Holds the app surface
    /// and the `CommitCounter` last presented for it, so `damage_since` returns
    /// only what changed since. Reset (→ full damage) when the surface differs.
    pub last_present: &'a mut Option<(WlSurface, CommitCounter)>,
    /// Rounded-corner texture program, compiled once per backend. Applied to a
    /// card's root surface when its `corner_radius > 0` so shrunken app cards
    /// (drag-up / switcher deck) render with rounded corners.
    pub rounded_tex_shader: &'a GlesTexProgram,
}

/// Render a layer surface's tree at `origin` in its own pass. Used for both the
/// below-app and above-app layers.
/// Blur the backdrop of `surface` (ext-background-effect-v1), if it asked for
/// one. Must run after everything behind the surface is drawn and before the
/// surface itself, since it blurs whatever is currently in the framebuffer.
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
) -> Result<(), SwapBuffersError> {
    // `origin` is physical; `app_scale` (= output `dpi`) scales the surface's
    // logical geometry to physical. Layer clients render at fractional scale
    // `dpi`, so their buffer is physical-sized and lands 1:1 — same model as app
    // surfaces.
    let scale = ctx.app_scale;
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(renderer, surface, origin, scale, 1.0, Kind::Unspecified);
    if elements.is_empty() {
        return Ok(());
    }
    let damage = Rectangle::from_size(size);
    let mut frame = renderer
        .render(framebuffer, size, ctx.transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, scale, &elements, &[damage]) {
        warn!(?e, "failed to draw layer surface");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Where a shrunken app card lands, in physical output pixels. Both card
/// producers (the drag-up/zoom window and the switcher deck) derive the same
/// four numbers from a centre + scale, so the arithmetic lives here once.
#[derive(Clone, Copy, Debug)]
struct Card {
    /// Top-left corner.
    x: i32,
    y: i32,
    /// Uniform scale applied to the surface tree (1.0 = fullscreen).
    scale: f32,
    /// Corner radius in physical px. `0` draws through the default program.
    corner_radius: f32,
    /// Drawn size, needed by the rounded-rect SDF in the shader.
    size: (f32, f32),
}

impl Card {
    /// A card of `scale` × the output, centred on `(center_x, center_y)`.
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

    /// Top-left as a render-element point.
    fn origin(&self) -> Point<i32, Physical> {
        Point::<i32, Physical>::from((self.x, self.y))
    }
}

/// Draw a shrunken app "card" (drag-up window or switcher-deck card): relocate +
/// rescale the surface tree to the card rect, then draw it in its own pass.
///
/// When `corner_radius > 0`, the card's root surface (`elements[0]`, always first
/// in the pre-order surface-tree walk) is drawn through the rounded-corner
/// texture program so the card's outer shape is rounded; any subsurfaces
/// (`elements[1..]`, interior content) draw unrounded. At `corner_radius == 0`
/// the whole tree draws through the default program unchanged.
fn draw_scaled_card(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    card: Card,
) -> Result<(), SwapBuffersError> {
    if elements.is_empty() {
        return Ok(());
    }
    let app_scale = ctx.app_scale;
    let damage = Rectangle::from_size(size);
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
        .render(framebuffer, size, ctx.transform)
        .map_err(SwapBuffersError::from)?;

    if card.corner_radius > 0.5 {
        let uniforms = vec![
            Uniform::new("corner_radius", card.corner_radius),
            Uniform::new("card_size", card.size),
        ];
        frame.override_default_tex_program(ctx.rounded_tex_shader.clone(), uniforms);
        if let Err(e) = draw_render_elements(&mut frame, app_scale, &scaled[..1], &[damage]) {
            warn!(?e, "failed to draw rounded card root surface");
        }
        frame.clear_tex_program_override();
        if scaled.len() > 1 {
            if let Err(e) = draw_render_elements(&mut frame, app_scale, &scaled[1..], &[damage]) {
                warn!(?e, "failed to draw card subsurfaces");
            }
        }
    } else if let Err(e) = draw_render_elements(&mut frame, app_scale, &scaled, &[damage]) {
        warn!(?e, "failed to draw scaled card elements");
    }

    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Render elements and derived facts about one frame, collected once before any
/// pass runs — element collection borrows the renderer, so it cannot be
/// interleaved with the render passes that also borrow it.
struct ScenePlan {
    /// The app surface's tree. Empty when there is no app surface.
    app_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
    /// Background/bottom layer-shell trees, one entry per non-empty surface.
    below_elements: Vec<Vec<WaylandSurfaceRenderElement<GlesRenderer>>>,
    /// The app's card placement while it is scaled (drag-up / zoom). `None` when
    /// the app is fullscreen or absent.
    window_transform: Option<crate::scene::WindowTransform>,
    /// The scene says the window covers the screen.
    is_fullscreen: bool,
    /// The app is turned a quarter-turn (landscape video).
    rotated: bool,
    /// Fullscreen *and* the client actually has something to draw. The extra
    /// condition matters: a fullscreen-scaled window with no content yet must
    /// still show Home behind it.
    app_fills_screen: bool,
    /// Blur regions a fullscreen app asked for via ext-background-effect.
    app_blur: Vec<crate::background_effect::BlurRect>,
    /// Fullscreen app with a blurred backdrop — translucent, so it needs Home
    /// drawn and blurred behind it instead of the usual single opaque pass.
    app_blurred: bool,
}

/// Collect every surface tree this frame needs and derive the flags the passes
/// branch on.
fn plan_scene(renderer: &mut GlesRenderer, ctx: &DrawCtx<'_>) -> ScenePlan {
    let scene = ctx.scene;
    let is_fullscreen = scene.window_covers_screen();
    // A rotated app covers the whole output, so it starts at the origin of its
    // own (rotated) space rather than at the usable-area origin.
    let rotated = ctx.rotation.swaps_axes();

    let app_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        if let Some(wl_surface) = ctx.app_surface {
            render_elements_from_surface_tree(
                renderer,
                wl_surface,
                if is_fullscreen && !rotated {
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

/// The KMS page-flip damage hint.
///
/// Default: the whole output (always correct — drivers without FB_DAMAGE_CLIPS
/// ignore it anyway). Narrow it only when the backend says this is a quiet
/// fullscreen app AND the app is a single render element at the origin, so
/// element-space damage equals output-space damage. Anything else (subsurfaces,
/// animation, chrome) stays full to avoid leaving stale pixels the driver would
/// skip. A blurred surface samples the pixels behind it, so damage below it must
/// be repainted through the blur: any blur region on screen disqualifies the
/// fast path entirely.
fn flip_damage(
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    plan: &ScenePlan,
) -> Vec<Rectangle<i32, Physical>> {
    let full_damage = Rectangle::from_size(size);
    let any_blur = plan.app_blurred
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
    // First frame on a freshly-focused surface has no baseline: repaint all.
    if same_surface {
        damage.to_vec()
    } else {
        vec![full_damage]
    }
}

/// Pass 1: clear the background, draw the below-app layer surfaces, and draw the
/// app itself when it is opaquely fullscreen (nothing goes behind it). A rotated
/// or blurred app is skipped here — each gets its own pass further down.
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

/// Skia: the home screen, drawn behind a shrinking window during transitions.
///
/// Skipped once the app has actually been drawn covering the screen — painting
/// home on top of that would flash home over the finished window for the last
/// few frames before the state machine formally settles into `UiState::App`.
/// Gating on scale alone (without requiring content) blanks home instead of the
/// app during the window where the animation has reached fullscreen scale but
/// the client hasn't painted its first frame yet. A blurred app is translucent,
/// so it needs a backdrop even in `UiState::App` (where `show_home` is false
/// because an opaque app hides Home entirely).
fn pass_home(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>, plan: &ScenePlan) {
    let scene = ctx.scene;
    if !(plan.app_blurred || (scene.show_home && !plan.app_fills_screen)) {
        return;
    }
    ctx.skia.draw_home(
        size.w,
        size.h,
        scene.home_page,
        ctx.model,
        ctx.icon_cache,
        ctx.app_catalog,
        ctx.skia_flip_y,
        ctx.pressed_app,
        ctx.launching_app,
        ctx.launching_elapsed,
        ctx.arrange.as_ref(),
        ctx.grid_positions,
        ctx.dock_positions,
        ctx.app_origin.1 as f32,
    );
}

/// Rotated fullscreen app: its own pass, with the rotation composed on top of
/// the output transform. The renderer swaps the projection's axes for a
/// quarter-turn transform, so element space is the landscape one the client was
/// configured at, while the framebuffer stays portrait.
fn pass_rotated_app(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    plan: &ScenePlan,
) -> Result<(), SwapBuffersError> {
    // A blurred app is drawn by `pass_blurred_app` instead — it has to wait for
    // Home and the blur to land underneath it first. Skipping it here is what
    // keeps the three fullscreen-app passes mutually exclusive: an app that was
    // both rotated *and* blurred used to be drawn by both, leaving a rotated
    // ghost of it on screen next to the upright copy.
    if !(plan.app_fills_screen && plan.rotated) || plan.app_blurred {
        return Ok(());
    }
    let (transform, app_damage) = app_pass_geometry(size, ctx, plan);
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

/// The transform and damage a fullscreen app pass draws with. A rotated app
/// composes its quarter-turn on top of the output transform and lives in the
/// (axis-swapped) space it was configured at, so its damage is that size too.
fn app_pass_geometry(
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
    plan: &ScenePlan,
) -> (Transform, Rectangle<i32, Physical>) {
    if plan.rotated {
        let app_size: Size<i32, Physical> = ctx.rotation.app_size((size.w, size.h)).into();
        (
            ctx.transform + ctx.rotation.transform(),
            Rectangle::from_size(app_size),
        )
    } else {
        (ctx.transform, Rectangle::from_size(size))
    }
}

/// Blurred fullscreen app: home is behind it by now, so blur that backdrop and
/// draw the app over it in its own pass.
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
    ctx.skia.blur_backdrop(
        size.w,
        size.h,
        &plan.app_blur,
        BLUR_SIGMA_LOGICAL * ctx.app_scale as f32,
        ctx.skia_flip_y,
    );
    // Carries the rotation too, since this is now the only pass that draws a
    // blurred fullscreen app whether or not it is turned.
    let (transform, damage) = app_pass_geometry(size, ctx, plan);
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, &plan.app_elements, &[damage]) {
        warn!(?e, "failed to draw blurred app elements");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Pass 2: draw the scaled app card ON TOP of home (no clear), with rounded
/// corners when the transform carries a non-zero radius (drag-up / zoom).
/// Consumes `plan.app_elements` — no later pass uses them.
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
    let card = Card::centered(size, t.center_x, t.center_y, t.scale, t.corner_radius);
    // Same blur as a layer surface, but the card is scaled: the surface-local
    // region scales with it and lands at the card's origin.
    if let Some(surface) = ctx.app_surface {
        blur_behind(
            ctx.skia,
            size,
            surface,
            (card.x, card.y),
            ctx.app_scale * t.scale as f64,
            ctx.skia_flip_y,
        );
    }
    let elements = std::mem::take(&mut plan.app_elements);
    draw_scaled_card(renderer, framebuffer, size, ctx, elements, card)
}

/// Switcher deck: each card back-to-front (the scene sorts them ascending z).
/// `close_progress` lifts a card upward via the layout as it slides off-screen
/// to close, so the deck itself needs no extra scaling here.
fn pass_switcher_cards(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &DrawCtx<'_>,
) -> Result<(), SwapBuffersError> {
    for card in &ctx.scene.cards {
        let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) else {
            continue;
        };
        let placement = Card::centered(
            size,
            card.center_x,
            card.center_y,
            card.scale,
            card.corner_radius,
        );
        let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            render_elements_from_surface_tree(
                renderer,
                tl.surface.wl_surface(),
                (0, 0),
                ctx.app_scale,
                card.alpha,
                Kind::Unspecified,
            );
        draw_scaled_card(renderer, framebuffer, size, ctx, elements, placement)?;
    }
    Ok(())
}

/// Everything that draws above the app and below springchick's own chrome, in
/// back-to-front order. Each renders like a layer surface: its tree at a
/// physical origin, scaled by dpi, with its backdrop blurred first.
///
/// 1. App-parented popups (menus, dropdowns), root→leaf so submenus draw over
///    their parents. Drawn whatever the rotation — they belong to the app.
/// 2. Top/overlay layer surfaces (a status panel, the on-screen keyboard).
/// 3. Popups parented to one of those layer surfaces.
///
/// (2) and (3) are hidden while the app is rotated: portrait chrome across a
/// landscape app is worse than no chrome. `touch::surface_under` skips them in
/// the same condition, so a hidden panel never eats taps meant for the app.
fn pass_overlays(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
    rotated: bool,
) -> Result<(), SwapBuffersError> {
    // Borrowed, not collected: this runs every frame, and `ctx.skia` below is a
    // disjoint field from the three slices being chained.
    let overlays = ctx.app_popups.iter().chain(
        ctx.layers_above
            .iter()
            .chain(ctx.layer_popups)
            .filter(|_| !rotated),
    );
    for (surface, origin) in overlays {
        blur_behind(
            ctx.skia,
            size,
            surface,
            *origin,
            ctx.app_scale,
            ctx.skia_flip_y,
        );
        draw_layer(renderer, framebuffer, size, ctx, surface, *origin)?;
    }
    Ok(())
}

/// springchick's own chrome, drawn last: the home bar, the volume OSD, and the
/// touch indicators.
fn pass_chrome(size: Size<i32, Physical>, ctx: &mut DrawCtx<'_>, rotated: bool) {
    // Always draw the bar on top: it is the only way back out of a fullscreen
    // app, so it stays even while rotated (where it reads as a side handle).
    ctx.skia
        .draw_bar_overlay(size.w, size.h, ctx.bar_alpha, ctx.skia_flip_y);

    // The OSD sits above everything, including a fullscreen app — but it is
    // drawn portrait, so it is suppressed while the app is rotated rather than
    // laid sideways across landscape video.
    if let Some((level, muted, alpha)) = ctx.osd.filter(|_| !rotated) {
        ctx.skia
            .draw_osd_overlay(size.w, size.h, level, muted, alpha, ctx.skia_flip_y);
    }

    // Touch indicators sit on the very top so recordings show them over any
    // chrome or app.
    if !ctx.touches.is_empty() {
        ctx.skia
            .draw_touches_overlay(size.w, size.h, ctx.touches, ctx.skia_flip_y);
    }
}

/// Drive every client that drew this frame. The foreground app always gets a
/// callback; in the switcher every card surface must also be driven, otherwise
/// backgrounded clients (which throttle drawing to frame callbacks) stop
/// presenting and their card renders blank after the entry animation settles.
/// Layer surfaces and popups throttle the same way.
fn send_frame_callbacks(ctx: &DrawCtx<'_>) {
    if let Some(wl_surface) = ctx.app_surface {
        send_frames_surface_tree(wl_surface, ctx.frame_time);
    }
    for card in &ctx.scene.cards {
        if let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) {
            send_frames_surface_tree(tl.surface.wl_surface(), ctx.frame_time);
        }
    }
    for (surface, _) in ctx
        .layers_below
        .iter()
        .chain(ctx.layers_above)
        .chain(ctx.app_popups)
        .chain(ctx.layer_popups)
    {
        send_frames_surface_tree(surface, ctx.frame_time);
    }
}

/// Execute the full scene draw against an already-bound framebuffer, returning
/// the KMS page-flip damage hint. Presentation (`submit`/page-flip) is the
/// caller's job.
///
/// The passes below run in strict back-to-front order and each one assumes the
/// ones before it have already landed in the framebuffer — the blur passes in
/// particular sample whatever is currently there, so moving one earlier would
/// blur the wrong thing.
pub fn draw_scene(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
) -> Result<Vec<Rectangle<i32, Physical>>, SwapBuffersError> {
    // The session lock replaces the scene entirely — no app, no shell, no
    // layers. Checked before anything else is even planned so no client content
    // can reach the framebuffer while locked.
    if ctx.lock_view != crate::session_lock::LockView::Unlocked {
        return draw_locked(renderer, framebuffer, size, ctx);
    }

    let mut plan = plan_scene(renderer, ctx);
    // Computed before drawing: it reads the app element's pre-draw commit
    // counter and advances `last_present`.
    let damage_hint = flip_damage(size, ctx, &plan);

    pass_background(renderer, &mut *framebuffer, size, ctx, &plan)?;
    pass_home(size, ctx, &plan);
    pass_rotated_app(renderer, &mut *framebuffer, size, ctx, &plan)?;
    pass_blurred_app(renderer, &mut *framebuffer, size, ctx, &plan)?;
    pass_app_card(renderer, &mut *framebuffer, size, ctx, &mut plan)?;
    pass_switcher_cards(renderer, &mut *framebuffer, size, ctx)?;
    pass_overlays(renderer, &mut *framebuffer, size, ctx, plan.rotated)?;
    pass_chrome(size, ctx, plan.rotated);

    send_frame_callbacks(ctx);
    Ok(damage_hint)
}

/// Draw the session lock: black, plus the lock client's surface if it has one.
///
/// Deliberately minimal — this is the whole frame. Nothing from the session is
/// drawn, not even the home bar (the gesture it advertises does nothing while
/// locked). The volume OSD and the touch indicators are the only overlays kept:
/// neither shows session content, and volume keys still work while locked.
///
/// Damage is always the full output: the lock's own frames are rare, and the
/// partial-damage fast path assumes a fullscreen *app* element it must not be
/// handed here.
fn draw_locked(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
) -> Result<Vec<Rectangle<i32, Physical>>, SwapBuffersError> {
    let damage = Rectangle::from_size(size);

    // The lock surface renders at `dpi` like any other client, and covers the
    // whole output (it was configured at the output's logical size), so it draws
    // from the origin rather than the usable area.
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
    // Black, not `CLEAR_COLOR`: a lock surface that hasn't painted yet (or a
    // dead lock client) must not leave the shell's tinted backdrop showing as if
    // the session were merely idle.
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

    if let Some(surface) = ctx.lock_surface {
        send_frames_surface_tree(surface, ctx.frame_time);
    }
    Ok(vec![damage])
}

/// Send frame callbacks to all surfaces in the tree.
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
