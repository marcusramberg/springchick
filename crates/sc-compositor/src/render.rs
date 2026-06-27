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
    /// Output transform (winit = Flipped180; DRM = connector transform).
    pub transform: Transform,
    /// Mirror the Skia home/bar vertically — the DRM/GBM scanout buffer has the
    /// opposite Y-origin from Skia's BottomLeft surface. winit presents
    /// already-correct, so false.
    pub skia_flip_y: bool,
    /// Time in ms for frame callbacks.
    pub frame_time: u32,
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
    let is_fullscreen = window_transform.is_none_or(|t| t.scale >= 0.99);

    // Collect render elements for the app surface (if any).
    let base_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
        if let Some(wl_surface) = ctx.app_surface {
            render_elements_from_surface_tree(
                renderer,
                wl_surface,
                (0, 0),
                1.0,
                1.0,
                Kind::Unspecified,
            )
        } else {
            Vec::new()
        };

    // Pass 1: clear background; draw the app here if fullscreen (no home behind).
    {
        let mut frame = renderer
            .render(&mut *framebuffer, size, ctx.transform)
            .map_err(SwapBuffersError::from)?;
        frame
            .clear(CLEAR_COLOR, &[damage])
            .map_err(SwapBuffersError::from)?;

        if is_fullscreen && !base_elements.is_empty() {
            if let Err(e) = draw_render_elements(&mut frame, 1.0, &base_elements, &[damage]) {
                warn!(?e, "failed to draw app elements");
            }
        }

        let _sync = frame.finish().map_err(SwapBuffersError::from)?;
    }

    // Skia: draw the home screen behind a shrinking window (during transitions).
    if scene.show_home {
        ctx.skia.draw_home(
            size.w,
            size.h,
            scene.home_page,
            scene.page_offset,
            ctx.model,
            ctx.icon_cache,
            ctx.app_catalog,
            ctx.skia_flip_y,
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
                RescaleRenderElement<RelocateRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
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
            if let Err(e) = draw_render_elements(&mut frame, 1.0, &scaled, &[damage]) {
                warn!(?e, "failed to draw scaled app elements");
            }
            let _sync = frame.finish().map_err(SwapBuffersError::from)?;
        }
    }

    // Switcher cards: draw each card back-to-front (already sorted ascending z).
    if !scene.cards.is_empty() {
        for card in &scene.cards {
            let Some(Some(tl)) = ctx.toplevels.get(card.toplevel) else { continue };
            let card_w = size.w as f32 * card.scale;
            let card_h = size.h as f32 * card.scale;
            let card_x = (card.center_x - card_w / 2.0) as i32;
            let card_y = (card.center_y - card_h / 2.0) as i32;

            let card_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer,
                    tl.surface.wl_surface(),
                    (0, 0),
                    1.0,
                    1.0,
                    Kind::Unspecified,
                );
            if card_elements.is_empty() {
                continue;
            }

            let scaled: Vec<
                RescaleRenderElement<RelocateRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
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

            let mut frame = renderer
                .render(&mut *framebuffer, size, ctx.transform)
                .map_err(SwapBuffersError::from)?;
            if let Err(e) = draw_render_elements(&mut frame, 1.0, &scaled, &[damage]) {
                warn!(?e, "failed to draw switcher card");
            }
            let _sync = frame.finish().map_err(SwapBuffersError::from)?;
        }
    }

    // Always draw the bar on top.
    ctx.skia.draw_bar_overlay(size.w, size.h, ctx.skia_flip_y);

    // Send frame callbacks.
    if let Some(wl_surface) = ctx.app_surface {
        send_frames_surface_tree(wl_surface, ctx.frame_time);
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
