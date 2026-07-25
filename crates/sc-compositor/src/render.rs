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
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{Color32F, Frame, Renderer, RendererSuper};
use smithay::backend::SwapBuffersError;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{
    with_surface_tree_downward, SurfaceAttributes, TraversalAction,
};

use tracing::warn;

/// Background clear color.
pub const CLEAR_COLOR: Color32F = Color32F::new(0.06, 0.10, 0.14, 1.0);

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
    /// Home-bar opacity (faded out when the OSK covers it).
    pub bar_alpha: f32,
    /// App id of the icon currently pressed on Home (draws a press highlight).
    pub pressed_app: Option<&'a str>,
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
) -> Result<(), SwapBuffersError> {
    let elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        render_elements_from_surface_tree(renderer, surface, origin, 1.0, 1.0, Kind::Unspecified);
    if elements.is_empty() {
        return Ok(());
    }
    let damage = Rectangle::from_size(size);
    let mut frame = renderer
        .render(framebuffer, size, transform)
        .map_err(SwapBuffersError::from)?;
    if let Err(e) = draw_render_elements(&mut frame, 1.0, &elements, &[damage]) {
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
) -> Result<(), SwapBuffersError> {
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
            (0, 0),
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
                1.0,
                1.0,
                Kind::Unspecified,
            )
        })
        .filter(|e| !e.is_empty())
        .collect();

    let app_fills_screen = is_fullscreen && !base_elements.is_empty();

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
            if let Err(e) = draw_render_elements(&mut frame, 1.0, elements, &[damage]) {
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

    // Top/overlay layer surfaces (e.g. the on-screen keyboard) sit above the
    // app but below springchick's own chrome.
    for (surface, origin) in ctx.layers_above {
        draw_layer(renderer, framebuffer, size, ctx.transform, surface, *origin)?;
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

    Ok(())
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
