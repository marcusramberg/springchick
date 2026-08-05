//! Frosted-glass backdrop via ext-background-effect-v1.
//!
//! The search UI is translucent, so what shows through is the Home screen
//! behind it. Asking the compositor to blur that backdrop is one request — but
//! eframe/winit owns the `wl_surface` and exposes no Wayland protocol hooks, so
//! the request is made on a *second* `wayland-client` connection wrapped around
//! the same libwayland display (`Backend::from_foreign_display`), with the
//! surface pointer from the raw window handle turned back into a proxy.
//! libwayland is thread-safe and each connection drives its own queue, so this
//! coexists with winit's.
//!
//! Everything here is best-effort: a compositor without the global (anything
//! but springchick) just leaves the UI unblurred.

use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry::WlRegistry,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1;
pub use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;

/// Larger than any phone panel: the compositor clips the blur region to the
/// surface, so an oversized rectangle is how a client says "all of me" without
/// having to track its own size across resizes.
const WHOLE_SURFACE: i32 = 1 << 14;

/// Dispatch sink. Every event this client can receive (registry globals, the
/// manager's `capabilities`) is informational, so nothing is stored.
struct BlurState;

impl Dispatch<WlRegistry, GlobalListContents> for BlurState {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_events {
    ($($ty:ty),+ $(,)?) => {$(
        impl Dispatch<$ty, ()> for BlurState {
            fn event(
                _: &mut Self,
                _: &$ty,
                _: <$ty as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}

ignore_events!(
    WlCompositor,
    WlRegion,
    ExtBackgroundEffectManagerV1,
    ExtBackgroundEffectSurfaceV1,
);

/// Ask the compositor to blur the backdrop of the whole window.
///
/// Returns the effect object, which must be kept alive for as long as the blur
/// should apply — dropping it destroys the protocol object and, on the next
/// commit, the blur. `None` when the compositor doesn't offer the protocol, or
/// when we're not on Wayland at all.
pub fn blur_whole_window(
    handles: &(impl HasDisplayHandle + HasWindowHandle),
) -> Option<ExtBackgroundEffectSurfaceV1> {
    let RawDisplayHandle::Wayland(display) = handles.display_handle().ok()?.as_raw() else {
        return None;
    };
    let RawWindowHandle::Wayland(window) = handles.window_handle().ok()?.as_raw() else {
        return None;
    };

    // SAFETY: the display pointer comes from winit's live connection, which
    // outlives this process's UI, and libwayland allows multiple queues on one
    // display. `from_foreign_display` does not take ownership.
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) = registry_queue_init::<BlurState>(&conn).ok()?;
    let qh = queue.handle();

    let manager: ExtBackgroundEffectManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
    let compositor: WlCompositor = globals.bind(&qh, 1..=6, ()).ok()?;

    // SAFETY: the surface pointer is winit's live wl_surface for this window.
    let surface_id = unsafe {
        ObjectId::from_ptr(WlSurface::interface(), window.surface.as_ptr().cast())
    }
    .ok()?;
    let surface = WlSurface::from_id(&conn, surface_id).ok()?;

    let region = compositor.create_region(&qh, ());
    region.add(0, 0, WHOLE_SURFACE, WHOLE_SURFACE);
    let effect = manager.get_background_effect(&surface, &qh, ());
    effect.set_blur_region(Some(&region));
    region.destroy();
    // Deliberately no wl_surface.commit here: the region is double-buffered and
    // winit commits every frame. Committing from this side would push winit's
    // half-built surface state early.
    queue.roundtrip(&mut BlurState).ok()?;

    Some(effect)
}
