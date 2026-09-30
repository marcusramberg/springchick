//! `wp_presentation`. Feedback is collected from drawn surfaces and answered
//! at scanout: DRM with the vblank's timestamp and sequence, winit after the
//! swap with our own clock. A frame that never reaches scanout must be
//! [`discard`]ed, or a client waiting on feedback hangs.

use std::time::Duration;

use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, Refresh,
};

/// Every callback taken here must end in [`present`] or [`discard`].
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

/// `seq` and `time` are on `CLOCK_MONOTONIC`, the clock advertised at bind.
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

pub fn discard(callbacks: Vec<PresentationFeedbackCallback>) {
    for callback in callbacks {
        callback.discarded();
    }
}

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
