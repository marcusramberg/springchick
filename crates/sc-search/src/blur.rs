//! Backdrop blur via ext-background-effect-v1. eframe/winit owns the
//! `wl_surface` and exposes no protocol hooks, so the request goes over a
//! second `wayland-client` connection on the same libwayland display. Best
//! effort: without the global the UI is just unblurred.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry::WlRegistry,
    wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1;
pub use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;

/// Oversized on purpose: the compositor clips the region to the surface.
const WHOLE_SURFACE: i32 = 1 << 14;

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

/// The returned object must stay alive; dropping it removes the blur.
pub fn blur_whole_window(
    handles: &(impl HasDisplayHandle + HasWindowHandle),
) -> Option<ExtBackgroundEffectSurfaceV1> {
    let RawDisplayHandle::Wayland(display) = handles.display_handle().ok()?.as_raw() else {
        return None;
    };
    let RawWindowHandle::Wayland(window) = handles.window_handle().ok()?.as_raw() else {
        return None;
    };

    // SAFETY: winit's display outlives the UI; `from_foreign_display` does not
    // take ownership.
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let conn = Connection::from_backend(backend);
    let (globals, mut queue) = registry_queue_init::<BlurState>(&conn).ok()?;
    let qh = queue.handle();

    let manager: ExtBackgroundEffectManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
    let compositor: WlCompositor = globals.bind(&qh, 1..=6, ()).ok()?;

    // SAFETY: winit's live wl_surface for this window.
    let surface_id =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), window.surface.as_ptr().cast()) }
            .ok()?;
    let surface = WlSurface::from_id(&conn, surface_id).ok()?;

    let region = compositor.create_region(&qh, ());
    region.add(0, 0, WHOLE_SURFACE, WHOLE_SURFACE);
    let effect = manager.get_background_effect(&surface, &qh, ());
    effect.set_blur_region(Some(&region));
    region.destroy();
    // No commit here: winit commits every frame, and committing from this side
    // would push its half-built surface state.
    queue.roundtrip(&mut BlurState).ok()?;

    Some(effect)
}
