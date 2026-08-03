use crate::gesture::Tracker;
use crate::thresholds as th;

/// Live navigation phase (drives what the shell renders during the drag).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavState {
    Idle,
    Grabbing,        // window detached, tracking finger, no deck yet
    SwitcherPreview, // dragged past reveal: neighbor cards fanning in
    QuickSwitching,  // horizontal drag swapping adjacent app
}

/// Where the gesture lands on release.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavTarget {
    BackToApp,
    Home,
    Switcher,
    QuickSwitch(i32), // -1 = previous app (swipe right), +1 = next (swipe left)
}

/// Live phase from the current tracker (called each frame during a grab).
pub fn live_state(t: &Tracker) -> NavState {
    let horizontal = t.dx().abs() > t.up_progress();
    if horizontal && t.dx().abs() >= th::QUICK_SWITCH_PROGRESS {
        return NavState::QuickSwitching;
    }
    let up = t.up_progress();
    // Band B (reveal..mid): live fan. Band C (>= mid, past mid-screen): fan
    // collapses back to the single card heading home, so report plain Grabbing.
    if (th::SWITCHER_REVEAL_PROGRESS..th::HOME_MIN_PROGRESS).contains(&up) {
        return NavState::SwitcherPreview;
    }
    NavState::Grabbing
}

/// Classify the release target (spec: release-targets table).
pub fn classify_release(t: &Tracker) -> NavTarget {
    // Horizontal quick-switch wins if it dominates by travel or velocity.
    let horizontal_dominant = t.dx().abs() > t.up_progress();
    if horizontal_dominant
        && (t.dx().abs() >= th::QUICK_SWITCH_PROGRESS
            || t.velocity.x.abs() >= th::QUICK_SWITCH_VELOCITY)
    {
        return NavTarget::QuickSwitch(if t.dx() < 0.0 { 1 } else { -1 });
    }

    let progress = t.up_progress();
    // Barely moved → fall back into the app.
    if progress < th::BACK_TO_APP_MAX_PROGRESS {
        return NavTarget::BackToApp;
    }
    // Home only two ways: a decisive upward flick, or dragging all the way up.
    if t.velocity.y <= th::HOME_FLICK_VELOCITY {
        return NavTarget::Home;
    }
    if progress >= th::HOME_MIN_PROGRESS {
        return NavTarget::Home;
    }
    // Everything else — the whole slow middle band — settles in the fan stack.
    NavTarget::Switcher
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::Pt;

    // Build a tracker with an explicit end position and velocity.
    fn t_with(start: Pt, end: Pt, vel: Pt) -> Tracker {
        let mut t = Tracker::begin(start);
        t.current = end;
        t.velocity = vel;
        t
    }

    #[test]
    fn tiny_rise_returns_to_app() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.90 },
            Pt { x: 0.0, y: -0.2 },
        );
        assert_eq!(classify_release(&t), NavTarget::BackToApp);
    }

    #[test]
    fn fast_upward_flick_goes_home_even_if_short() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.75 },
            Pt { x: 0.0, y: -3.0 },
        );
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn slow_mid_drag_settles_in_switcher() {
        // A slow drag into band B (below mid-screen) lands in the switcher.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.65 }, // up_progress 0.30 (band B, below 0.35 mid)
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn slow_drag_past_mid_goes_home() {
        // Past mid-screen (band C), a slow release goes home, not switcher.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.35 }, // up_progress 0.60 >= 0.50
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn live_state_collapses_fan_past_mid() {
        // Above mid-screen the fan collapses: live_state reports plain Grabbing.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.35 }, // up_progress 0.60 >= 0.50
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(live_state(&t), NavState::Grabbing);
    }

    #[test]
    fn all_the_way_up_goes_home() {
        // Slow drag almost to the top (progress 0.88) → home, not switcher.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.07 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn moderate_slow_drag_settles_in_fan_stack() {
        // A slow drag into the middle band should land in the switcher, not
        // home — the whole slow middle is forgiving now.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.65 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn short_slow_drag_just_past_backstop_is_switcher() {
        // Just past the back-to-app deadzone, a slow drag already reveals the fan.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.80 },
            Pt { x: 0.0, y: -0.4 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn horizontal_flick_quick_switches_next() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.2, y: 0.93 },
            Pt { x: -2.0, y: 0.0 },
        );
        assert_eq!(classify_release(&t), NavTarget::QuickSwitch(1));
    }

    #[test]
    fn live_state_reveals_switcher_past_threshold() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.75 }, // up_progress 0.20 (band B: reveal..mid)
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(live_state(&t), NavState::SwitcherPreview);
    }
}
