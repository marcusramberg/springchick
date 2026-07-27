//! Shared render path for the winit and DRM backends.
//!
//! Both backends own a `GlesRenderer` and differ only in how they acquire the
//! framebuffer (`bind`) and present (`submit`/page-flip). Everything between —
//! clearing, the transformed two-pass app composite, and the Skia home/bar
//! overlay — is identical and lives here.

use std::collections::HashMap;

use crate::scene::Scene;
use crate::skia_gl::SkiaGl;

use sc_config::AppEntry;
use sc_icons::IconPixels;
use sc_shell_model::ShellModel;

use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::element::{Element, Kind};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::{draw_render_elements, CommitCounter};
use smithay::backend::renderer::{Color32F, Frame, Renderer, RendererSuper};
use smithay::backend::SwapBuffersError;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Point, Rectangle, Scale, Size, Transform};
use smithay::wayland::compositor::{
    with_surface_tree_downward, SurfaceAttributes, TraversalAction,
};

use tracing::warn;

/// Background clear color.
pub const CLEAR_COLOR: Color32F = Color32F::new(0.06, 0.10, 0.14, 1.0);

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
    /// Mirror the Skia home/bar vertically — the DRM/GBM scanout buffer has the
    /// opposite Y-origin from Skia's BottomLeft surface. winit presents
    /// already-correct, so false.
    pub skia_flip_y: bool,
    /// Time in ms for frame callbacks.
    pub frame_time: u32,
    /// Volume OSD to overlay: `(level, muted, alpha)`. `None` when inactive.
    pub osd: Option<(f32, bool, f32)>,
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
    /// Arrange-mode view (badges/Done/drag ghost). `None` when arrange mode
    /// is inactive.
    pub arrange: Option<ArrangeView<'a>>,
    /// When true, the frame is a "quiet fullscreen app" (nothing but the app
    /// surface can have changed) and `draw_scene` may return a narrowed KMS
    /// page-flip damage hint instead of the full rect. The backend computes
    /// this; when false the hint is always the full output.
    pub report_partial_damage: bool,
    /// Per-app commit cursor for `report_partial_damage`. Holds the app surface
    /// and the `CommitCounter` last presented for it, so `damage_since` returns
    /// only what changed since. Reset (→ full damage) when the surface differs.
    pub last_present: &'a mut Option<(WlSurface, CommitCounter)>,
}

/// Render a layer surface's tree at `origin` in its own pass. Used for both the
/// below-app and above-app layers.
fn draw_layer(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    transform: Transform,
    surface: &WlSurface,
    origin: (i32, i32),
    scale: f64,
) -> Result<(), SwapBuffersError> {
    // `origin` is physical; `scale` (= output `dpi`) scales the surface's logical
    // geometry to physical. Layer clients render at fractional scale `dpi`, so
    // their buffer is physical-sized and lands 1:1 — same model as app surfaces.
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(renderer, surface, origin, scale, 1.0, Kind::Unspecified);
    if elements.is_empty() {
        return Ok(());
    }
    let damage = Rectangle::from_size(size);
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, scale, &elements, &[damage]) {
        warn!(?e, "failed to draw layer surface");
    }
    let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    Ok(())
}

/// Execute the full two-pass scene draw against an already-bound framebuffer.
/// Presentation (`submit`/page-flip) is the caller's job.
pub fn draw_scene(
    renderer: &mut GlesRenderer,
    framebuffer: &mut <GlesRenderer as RendererSuper>::Framebuffer<'_>,
    size: Size<i32, Physical>,
    ctx: &mut DrawCtx<'_>,
) -> Result<Vec<Rectangle<i32, Physical>>, SwapBuffersError> {
    let damage = Rectangle::from_size(size);
    let scene = ctx.scene;

    let window_transform = scene.window.as_ref().map(|(_, t)| *t);
    let is_fullscreen = scene.window_covers_screen();

    // Collect render elements for the app surface (if any).
    let base_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = if let Some(wl_surface) =
        ctx.app_surface
    {
        render_elements_from_surface_tree(
            renderer,
            wl_surface,
            if is_fullscreen { ctx.app_origin } else { (0, 0) },
            ctx.app_scale,
            1.0,
            Kind::Unspecified,
        )
    } else {
        Vec::new()
    };

    // Layer-shell surface elements, pre-collected before any render pass (they
    // borrow the renderer). Each is positioned at its computed rect origin.
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

    let app_fills_screen = is_fullscreen && !base_elements.is_empty();

    // KMS page-flip damage hint. Default: the whole output (always correct —
    // drivers without FB_DAMAGE_CLIPS ignore it anyway). Narrow it only when the
    // backend says this is a quiet fullscreen app AND the app is a single render
    // element at the origin, so element-space damage equals output-space damage.
    // Anything else (subsurfaces, animation, chrome) stays full to avoid leaving
    // stale pixels the driver would skip.
    let full_damage = Rectangle::from_size(size);
    let flip_damage: Vec<Rectangle<i32, Physical>> = if ctx.report_partial_damage
        && app_fills_screen
        && base_elements.len() == 1
    {
        let app_wl = ctx.app_surface.expect("app_fills_screen implies app_surface");
        let elem = &base_elements[0];
        let same_surface = ctx
            .last_present
            .as_ref()
            .is_some_and(|(s, _)| s == app_wl);
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
    } else {
        vec![full_damage]
    };

    // Pass 1: clear background; draw the app here if fullscreen (no home behind).
    {
        let mut frame = renderer
            .render(&mut *framebuffer, size, ctx.transform)
            .map_err(SwapBuffersError::from)?;
        frame
            .clear(CLEAR_COLOR, &[damage])
            .map_err(SwapBuffersError::from)?;

        // Background/bottom layer surfaces sit behind the app.
        for elements in &below_elements {
            if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, elements, &[damage]) {
                warn!(?e, "failed to draw background layer surface");
            }
        }

        if app_fills_screen {
            if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, &base_elements, &[damage])
            {
                warn!(?e, "failed to draw app elements");
            }
        }

        let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    }

    // Skia: draw the home screen behind a shrinking window (during
    // transitions). Skip only once the app has actually been drawn covering
    // the screen above — painting home on top of that would flash home over
    // the finished window for the last few frames before the state machine
    // formally settles into UiState::App. Gating on scale alone (without
    // requiring content) blanks home instead of the app during the window
    // where the animation has reached fullscreen scale but the client hasn't
    // painted its first frame yet.
    if scene.show_home && !app_fills_screen {
        ctx.skia.draw_home(
            size.w,
            size.h,
            scene.home_page,
            scene.page_offset,
            ctx.model,
            ctx.icon_cache,
            ctx.app_catalog,
            ctx.skia_flip_y,
            ctx.pressed_app,
            ctx.arrange.as_ref(),
        );
    }

    // Pass 2: draw the scaled app ON TOP of home (no clear).
    if !is_fullscreen && !base_elements.is_empty() {
        if let Some(t) = window_transform {
            let scale_f = t.scale as f64;
            let card_w = size.w as f32 * t.scale;
            let card_h = size.h as f32 * t.scale;
            let card_x = (t.center_x - card_w / 2.0) as i32;
            let card_y = (t.center_y - card_h / 2.0) as i32;

            let scaled: Vec<
                RescaleRenderElement<
                    RelocateRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
                >,
            > = base_elements
                .into_iter()
                .map(|e| {
                    let relocated = RelocateRenderElement::from_element(
                        e,
                        Point::<i32, Physical>::from((card_x, card_y)),
                        Relocate::Relative,
                    );
                    RescaleRenderElement::from_element(
                        relocated,
                        Point::<i32, Physical>::from((card_x, card_y)),
                        smithay::utils::Scale::from(scale_f),
                    )
                })
                .collect();

            let mut frame = renderer
                .render(&mut *framebuffer, size, ctx.transform)
                .map_err(SwapBuffersError::from)?;
            if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, &scaled, &[damage]) {
                warn!(?e, "failed to draw scaled app elements");
            }
            let _sync = frame.finish().map_err(SwapBuffersError::from)?;
        }
    }

    // Switcher cards: draw each card back-to-front (already sorted ascending z).
    if !scene.cards.is_empty() {
        for card in &scene.cards {
            let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) else {
                continue;
            };
            let card_w = size.w as f32 * card.scale;
            let card_h = size.h as f32 * card.scale;
            let card_x = (card.center_x - card_w / 2.0) as i32;
            let card_y = (card.center_y - card_h / 2.0) as i32;

            let card_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    tl.surface.wl_surface(),
                    (0, 0),
                    ctx.app_scale,
                    1.0,
                    Kind::Unspecified,
                );
            if card_elements.is_empty() {
                continue;
            }

            let scaled: Vec<
                RescaleRenderElement<
                    RelocateRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
                >,
            > = card_elements
                .into_iter()
                .map(|e| {
                    let relocated = RelocateRenderElement::from_element(
                        e,
                        Point::<i32, Physical>::from((card_x, card_y)),
                        Relocate::Relative,
                    );
                    RescaleRenderElement::from_element(
                        relocated,
                        Point::<i32, Physical>::from((card_x, card_y)),
                        smithay::utils::Scale::from(card.scale as f64),
                    )
                })
                .collect();

            // close_progress lifts the card upward (via layout) as it slides
            // off-screen to close; the deck itself needs no extra scaling here.
            let mut frame = renderer
                .render(&mut *framebuffer, size, ctx.transform)
                .map_err(SwapBuffersError::from)?;
            if let Err(e) = draw_render_elements(&mut frame, ctx.app_scale, &scaled, &[damage]) {
                warn!(?e, "failed to draw switcher card");
            }
            let _sync = frame.finish().map_err(SwapBuffersError::from)?;
        }
    }

    // App-parented popups (menus, dropdowns) sit above the app but below the
    // top/overlay layers. Each popup surface renders like a layer surface: its
    // tree at a physical origin, scaled by dpi. Ordered root→leaf so submenus
    // draw over their parents.
    for (surface, origin) in ctx.app_popups {
        draw_layer(
            renderer,
            framebuffer,
            size,
            ctx.transform,
            surface,
            *origin,
            ctx.app_scale,
        )?;
    }

    // Top/overlay layer surfaces (e.g. the on-screen keyboard) sit above the
    // app but below springchick's own chrome.
    for (surface, origin) in ctx.layers_above {
        draw_layer(
            renderer,
            framebuffer,
            size,
            ctx.transform,
            surface,
            *origin,
            ctx.app_scale,
        )?;
    }

    // Popups parented to a top/overlay layer surface sit just above it.
    for (surface, origin) in ctx.layer_popups {
        draw_layer(
            renderer,
            framebuffer,
            size,
            ctx.transform,
            surface,
            *origin,
            ctx.app_scale,
        )?;
    }

    // Always draw the bar on top.
    ctx.skia
        .draw_bar_overlay(size.w, size.h, ctx.bar_alpha, ctx.skia_flip_y);

    // Volume OSD sits above everything, including a fullscreen app.
    if let Some((level, muted, alpha)) = ctx.osd {
        ctx.skia
            .draw_osd_overlay(size.w, size.h, level, muted, alpha, ctx.skia_flip_y);
    }

    // Send frame callbacks. The foreground app surface always gets one; in the
    // switcher, every card surface must also be driven, otherwise backgrounded
    // clients (which throttle drawing to frame callbacks) stop presenting and
    // their card renders blank after the entry animation settles.
    if let Some(wl_surface) = ctx.app_surface {
        send_frames_surface_tree(wl_surface, ctx.frame_time);
    }
    for card in &scene.cards {
        if let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) {
            send_frames_surface_tree(tl.surface.wl_surface(), ctx.frame_time);
        }
    }
    // Layer surfaces (e.g. wvkbd) throttle to frame callbacks too.
    for (surface, _) in ctx.layers_below.iter().chain(ctx.layers_above) {
        send_frames_surface_tree(surface, ctx.frame_time);
    }
    // Popups throttle to frame callbacks like any client surface.
    for (surface, _) in ctx.app_popups.iter().chain(ctx.layer_popups) {
        send_frames_surface_tree(surface, ctx.frame_time);
    }

    Ok(flip_damage)
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
