//! wp_content_type_v1. The hint for auto-landscape: Wayland has no protocol
//! for a client to request an orientation, so this is the only standard input.

use smithay::reexports::wayland_protocols::wp::content_type::v1::server::wp_content_type_v1::Type;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::content_type::{ContentTypeState, ContentTypeSurfaceCachedState};

use crate::State;

/// Keeps the global alive; the tag lives in each surface's cached state.
pub struct ContentType {
    #[allow(dead_code)]
    manager: ContentTypeState,
}

impl ContentType {
    pub fn new(dh: &DisplayHandle) -> Self {
        ContentType {
            manager: ContentTypeState::new::<State>(dh),
        }
    }
}

pub fn of(surface: &WlSurface) -> Type {
    smithay::wayland::compositor::with_states(surface, |states| {
        *states
            .cached_state
            .get::<ContentTypeSurfaceCachedState>()
            .current()
            .content_type()
    })
}

/// Only fullscreen video and games rotate. Photos don't: a portrait photo
/// would turn the wrong way.
pub fn wants_landscape(content_type: Type, fullscreen: bool) -> bool {
    fullscreen && matches!(content_type, Type::Video | Type::Game)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_fullscreen_video_or_game_wants_landscape() {
        assert!(wants_landscape(Type::Video, true));
        assert!(wants_landscape(Type::Game, true));
        assert!(!wants_landscape(Type::Video, false));
        assert!(!wants_landscape(Type::Photo, true));
        assert!(!wants_landscape(Type::None, true));
    }
}
