//! wp_content_type_v1: what a surface says it is showing.
//!
//! A client tags its surface `photo`, `video` or `game`; the tag is
//! double-buffered per surface, so it arrives with the commit that starts
//! playback and goes away with the one that stops it. mpv, GTK's video sink and
//! friends set it.
//!
//! springchick uses it as the *hint* half of auto-landscape: a fullscreen
//! surface showing video or a game is content the user almost certainly wants
//! rotated, whereas a fullscreen text app is not. Wayland has no protocol for a
//! client to request an orientation — no stable, staging or unstable protocol
//! carries one — so the rotation decision is compositor policy, and this is the
//! only standards-based input to it.
//!
//! This module wires the protocol and derives the hint; nothing rotates yet
//! (the render/input transform is separate work). [`State::landscape_hint`] is
//! where that lands.

use smithay::reexports::wayland_protocols::wp::content_type::v1::server::wp_content_type_v1::Type;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::content_type::{ContentTypeState, ContentTypeSurfaceCachedState};

use crate::State;

/// Compositor-side state for the content-type protocol. Only holds the global
/// alive — the tag itself lives in each surface's cached state.
pub struct ContentType {
    #[allow(dead_code)]
    manager: ContentTypeState,
}

impl ContentType {
    /// Create the manager global.
    pub fn new(dh: &DisplayHandle) -> Self {
        ContentType {
            manager: ContentTypeState::new::<State>(dh),
        }
    }
}

/// The content type a surface committed, `None` if it never tagged itself.
pub fn of(surface: &WlSurface) -> Type {
    smithay::wayland::compositor::with_states(surface, |states| {
        *states
            .cached_state
            .get::<ContentTypeSurfaceCachedState>()
            .current()
            .content_type()
    })
}

/// Whether a surface's content wants a landscape display, given its content
/// type and whether it is fullscreen.
///
/// Video and games are landscape-shaped content; photos are not (a portrait
/// photo would be rotated the wrong way), and untagged content stays as-is.
/// Non-fullscreen never rotates: a video in a windowed player is not the shell's
/// business, and rotating the whole shell around it would be worse than leaving
/// it alone.
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
        // Windowed video is the client's problem, not the shell's.
        assert!(!wants_landscape(Type::Video, false));
        // A portrait photo rotated to landscape is worse than leaving it.
        assert!(!wants_landscape(Type::Photo, true));
        // Untagged clients (the common case) never trigger rotation.
        assert!(!wants_landscape(Type::None, true));
    }
}
