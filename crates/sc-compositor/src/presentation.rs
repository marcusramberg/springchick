//! `wp_presentation`: tell clients exactly when their frame hit the screen.
//!
//! A frame callback only says "draw again now". Presentation feedback says
//! *when the last one landed*, with the vblank's own timestamp, the refresh
//! interval and the CRTC sequence number. Clients that pace themselves — video
//! players picking a frame to show, browsers scheduling animations — use it to
//! line their output up with the panel instead of guessing from wall-clock
//! time. Without it a 24fps video on a 90Hz panel judders no matter how
//! carefully the player works.
//!
//! Feedback is per commit, so the callbacks are collected from the surfaces we
//! actually drew ([`take_feedback`], called from the same walk that sends frame
//! callbacks) and answered once the frame reaches scanout:
//!
//! - DRM: held until the page-flip's vblank, then answered with the kernel's
//!   own timestamp and sequence — real hardware numbers, flagged as such.
//! - winit: answered right after the swap with our own clock, flagged as a
//!   software present, since a nested compositor owns neither the vblank nor
//!   the sequence.
//!
//! A frame that never reaches scanout (draw failed, page-flip refused, the
//! panel is blanked and only a mirror is watching) must answer `discarded`
//! instead — [`discard`]. Dropping the callbacks silently would leave a client
//! that waits for feedback before its next commit hung forever.

use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, Refresh,
};

/// Collect the presentation feedback committed against a surface tree.
///
/// Taking the callbacks hands us the obligation to answer them: every one that
/// comes out of here must end up in either [`present`] or [`discard`].
pub fn take_feedback(surface: &WlSurface, out: &mut Vec<PresentationFeedbackCallback>) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            let mut cached = states.cached_state.get::<PresentationFeedbackCachedState>();
            out.append(&mut cached.current().callbacks);
        },
        |_, _, &()| true,
    );
}

/// Answer every collected callback with the moment the frame was presented.
///
/// `seq` is the CRTC sequence number and `time` the presentation timestamp, both
/// on the clock we advertised at bind (`CLOCK_MONOTONIC`).
pub fn present(
    callbacks: Vec<PresentationFeedbackCallback>,
    output: &Output,
    time: Duration,
    refresh: Refresh,
    seq: u64,
    flags: wp_presentation_feedback::Kind,
) {
    for callback in callbacks {
        callback.presented(output, time, refresh, seq, flags);
    }
}

/// Tell every collected callback its frame never made it to the screen.
pub fn discard(callbacks: Vec<PresentationFeedbackCallback>) {
    for callback in callbacks {
        callback.discarded();
    }
}

/// Refresh interval of a mode given in mHz, for [`Refresh::Fixed`]. A mode with
/// no rate (or a nonsensical one) reports [`Refresh::Unknown`] rather than
/// dividing by zero.
pub fn refresh_from_mhz(refresh_mhz: i32) -> Refresh {
    if refresh_mhz <= 0 {
        return Refresh::Unknown;
    }
    Refresh::Fixed(Duration::from_nanos(
        1_000_000_000_000u64 / refresh_mhz as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_from_a_90hz_mode() {
        // 90Hz in mHz -> 11.11ms.
        assert_eq!(
            refresh_from_mhz(90_000),
            Refresh::Fixed(Duration::from_nanos(11_111_111))
        );
    }

    #[test]
    fn refresh_from_a_60hz_mode() {
        assert_eq!(
            refresh_from_mhz(60_000),
            Refresh::Fixed(Duration::from_nanos(16_666_666))
        );
    }

    #[test]
    fn refresh_without_a_rate_is_unknown() {
        assert_eq!(refresh_from_mhz(0), Refresh::Unknown);
        assert_eq!(refresh_from_mhz(-1), Refresh::Unknown);
    }
}
