//! ext-background-effect-v1: blur behind translucent surfaces.
//!
//! A client marks a region of its surface (surface-local, double-buffered) and
//! the compositor blurs whatever is already on screen behind that region — the
//! frosted-glass look panels and on-screen keyboards want. The realistic users
//! here are layer surfaces (status/quick-settings panels, the OSK) and popups.
//!
//! smithay dispatches the protocol and caches the region per surface; this
//! module converts a surface's committed region into screen-space rectangles for
//! the render path, which does the actual blur ([`crate::skia_gl::SkiaGl::blur_backdrop`]).
//!
//! The advertised capability is `blur` only, and it is only advertised because
//! the blur is really implemented — a client that sees the capability draws
//! thinner, more translucent chrome on the assumption the blur is there.

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::Rectangle;
use smithay::wayland::background_effect::{
    BackgroundEffectState, BackgroundEffectSurfaceCachedState, Capability,
    ExtBackgroundEffectHandler,
};
use smithay::wayland::compositor::RectangleKind;

use tracing::debug;

use crate::State;

/// One rectangle of a blur region, in physical screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlurRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// `false` for a `Subtract` rectangle: a hole punched in the blur region.
    pub add: bool,
}

/// Compositor-side state. Only holds the global alive — regions live in each
/// surface's cached state.
pub struct BackgroundEffect {
    #[allow(dead_code)]
    manager: BackgroundEffectState,
}

impl BackgroundEffect {
    /// Create the manager global.
    pub fn new(dh: &DisplayHandle) -> Self {
        BackgroundEffect {
            manager: BackgroundEffectState::new::<State>(dh),
        }
    }
}

/// The committed blur region of `surface`, mapped to physical screen coords:
/// clipped to the surface, scaled by `scale` (dpi) and offset to where the
/// surface is drawn.
///
/// Clipping is required by the protocol ("clipped by the compositor to the
/// surface size") and matters in practice: clients set an oversized rectangle
/// to mean "all of me", and an unclipped one would blur the screen around the
/// surface too.
///
/// Empty when the surface set no region, which is the common case — callers use
/// that to skip the whole blur pass.
pub fn blur_rects(surface: &WlSurface, origin: (i32, i32), scale: f64) -> Vec<BlurRect> {
    let surface_size =
        smithay::backend::renderer::utils::with_renderer_surface_state(surface, |state| {
            state.surface_size()
        })
        .flatten();
    smithay::wayland::compositor::with_states(surface, |states| {
        let region = states
            .cached_state
            .get::<BackgroundEffectSurfaceCachedState>()
            .current()
            .blur_region
            .clone();
        let Some(region) = region else {
            return Vec::new();
        };
        region
            .rects
            .iter()
            .filter_map(|(kind, rect)| {
                let mut rect = *rect;
                if let Some(size) = surface_size {
                    rect = rect.intersection(Rectangle::from_size(size))?;
                }
                Some(BlurRect {
                    x: origin.0 as f32 + rect.loc.x as f32 * scale as f32,
                    y: origin.1 as f32 + rect.loc.y as f32 * scale as f32,
                    w: rect.size.w as f32 * scale as f32,
                    h: rect.size.h as f32 * scale as f32,
                    add: matches!(kind, RectangleKind::Add),
                })
            })
            .collect()
    })
}

/// Whether `surface` committed a non-empty blur region. Cheaper than
/// [`blur_rects`] and used to decide whether a frame can take the partial-damage
/// fast path at all (a blurred surface's pixels depend on what is drawn behind
/// it, so any damage below it must be repainted through the blur).
pub fn has_blur_region(surface: &WlSurface) -> bool {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .cached_state
            .get::<BackgroundEffectSurfaceCachedState>()
            .current()
            .blur_region
            .as_ref()
            .is_some_and(|r| !r.rects.is_empty())
    })
}

impl ExtBackgroundEffectHandler for State {
    fn capabilities(&self) -> Capability {
        Capability::Blur
    }

    fn set_blur_region(
        &mut self,
        _surface: WlSurface,
        region: smithay::wayland::compositor::RegionAttributes,
    ) {
        debug!(rects = region.rects.len(), "blur region set");
    }

    fn unset_blur_region(&mut self, _surface: WlSurface) {
        debug!("blur region unset");
    }
}
