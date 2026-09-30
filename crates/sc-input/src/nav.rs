use crate::gesture::Tracker;
use crate::thresholds as th;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavState {
    Idle,
    Grabbing,        // window detached, no deck yet
    SwitcherPreview, // past reveal: neighbour cards fanning in
    QuickSwitching,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NavTarget {
    BackToApp,
    Home,
    Switcher,
    QuickSwitch(i32), // -1 = more recent (swipe left), +1 = older (swipe right)
}

/// Called each frame during a grab.
pub fn live_state(t: &Tracker) -> NavState {
    let horizontal = t.dx().abs() > t.up_progress();
    if horizontal && t.dx().abs() >= th::QUICK_SWITCH_PROGRESS {
        return NavState::QuickSwitching;
    }
    let up = t.up_progress();
    // Past HOME_MIN the fan collapses back to the single card heading home.
    if (th::SWITCHER_REVEAL_PROGRESS..th::HOME_MIN_PROGRESS).contains(&up) {
        return NavState::SwitcherPreview;
    }
    NavState::Grabbing
}

pub fn classify_release(t: &Tracker) -> NavTarget {
    let horizontal_dominant = t.dx().abs() > t.up_progress();
    if horizontal_dominant
        && (t.dx().abs() >= th::QUICK_SWITCH_PROGRESS
            || t.velocity.x.abs() >= th::QUICK_SWITCH_VELOCITY)
    {
        // Most recent is on the right, matching the carousel.
        return NavTarget::QuickSwitch(if t.dx() < 0.0 { -1 } else { 1 });
    }

    let progress = t.up_progress();
    if progress < th::BACK_TO_APP_MAX_PROGRESS {
        return NavTarget::BackToApp;
    }
    // Home on any quick flick, or on dragging far enough. Only a slow drag
    // settles in the fan.
    if t.velocity.y <= th::HOME_FLICK_VELOCITY {
        return NavTarget::Home;
    }
    if progress >= th::HOME_MIN_PROGRESS {
        return NavTarget::Home;
    }
    NavTarget::Switcher
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::Pt;

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
    fn quick_flick_goes_home_not_to_the_switcher() {
        // Velocity is what the low-pass reports for a 20% flick in ~120ms.
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.75 },
            Pt { x: 0.0, y: -1.0 },
        );
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn a_drag_just_under_flick_speed_still_settles_in_the_fan() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.75 },
            Pt { x: 0.0, y: -0.6 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn slow_mid_drag_settles_in_switcher() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.65 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn slow_drag_past_mid_goes_home() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.35 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn live_state_collapses_fan_past_mid() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.35 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(live_state(&t), NavState::Grabbing);
    }

    #[test]
    fn all_the_way_up_goes_home() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.07 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Home);
    }

    #[test]
    fn moderate_slow_drag_settles_in_fan_stack() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.65 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn short_slow_drag_just_past_backstop_is_switcher() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.80 },
            Pt { x: 0.0, y: -0.4 },
        );
        assert_eq!(classify_release(&t), NavTarget::Switcher);
    }

    #[test]
    fn horizontal_flick_left_quick_switches_to_previous() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.2, y: 0.93 },
            Pt { x: -2.0, y: 0.0 },
        );
        assert_eq!(classify_release(&t), NavTarget::QuickSwitch(-1));
    }

    #[test]
    fn live_state_reveals_switcher_past_threshold() {
        let t = t_with(
            Pt { x: 0.5, y: 0.95 },
            Pt { x: 0.5, y: 0.75 },
            Pt { x: 0.0, y: -0.5 },
        );
        assert_eq!(live_state(&t), NavState::SwitcherPreview);
    }
}
