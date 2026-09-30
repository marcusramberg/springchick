//! ext-background-effect-v1: blur behind translucent surfaces. smithay caches
//! the region; this maps it to screen space for
//! [`crate::skia_gl::SkiaGl::blur_backdrop`]. Only advertise `blur` while it
//! is really implemented: clients draw thinner chrome when they see it.

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

/// Physical screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlurRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// `false` for a `Subtract` rectangle.
    pub add: bool,
}

/// Keeps the global alive; regions live in each surface's cached state.
pub struct BackgroundEffect {
    #[allow(dead_code)]
    manager: BackgroundEffectState,
}

impl BackgroundEffect {
    pub fn new(dh: &DisplayHandle) -> Self {
        BackgroundEffect {
            manager: BackgroundEffectState::new::<State>(dh),
        }
    }
}

/// The committed blur region in physical screen coords. Clipped to the
/// surface: clients send oversized rects to mean "all of me".
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

/// A blurred surface depends on what's drawn behind it, so it rules out the
/// partial-damage fast path.
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
