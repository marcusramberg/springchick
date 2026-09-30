//! Pure gesture decisions for the shell: page drags, pull-down search, the
//! Home bar, the switcher deck, and quick-switch. Distances are output pixels,
//! compared against [`thresholds`](crate::thresholds).

use crate::thresholds as th;

/// From Home the bar reaches for the deck: up opens the switcher, right slides
/// onto the front app. Left is inert; the stack only extends one way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarRelease {
    /// Bounces if nothing is running.
    OpenSwitcher,
    SlideToTop,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardDrag {
    /// Mostly vertical. Positive up to `1.0` toward closing; negative is a small
    /// rubber-banded push that never commits.
    Close { progress: f32 },
    /// Carousel scroll position, in cards.
    Scroll { position: f32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuickSwitchRelease {
    /// `dir` walks the MRU cursor; `target` is where the offset spring settles.
    Commit {
        dir: i32,
        target: f32,
    },
    Reject,
}

/// In pages. Tracks the finger directly, rubber-banding past the ends.
pub fn page_drag_value(dx: f32, width: f32, page: usize, page_count: usize) -> f32 {
    let raw = page as f32 - dx / width;
    let max_page = page_count.saturating_sub(1) as f32;
    if raw < 0.0 {
        raw * th::RUBBER_BAND_FOLLOW
    } else if raw > max_page {
        max_page + (raw - max_page) * th::RUBBER_BAND_FOLLOW
    } else {
        raw
    }
}

/// Commits to a neighbour by distance ([`th::PAGE_COMMIT_FRAC`]) or by flick
/// ([`th::PAGE_FLICK_VELOCITY`] past [`th::PAGE_FLICK_MIN_FRAC`]). `vx` is in
/// widths/s, positive rightward (toward the previous page).
pub fn page_after_swipe(dx: f32, vx: f32, width: f32, page: usize, page_count: usize) -> usize {
    let delta = -dx / width;
    let flick = -vx;
    // A flick only counts in the direction of travel, so a drag-out-and-back
    // doesn't page the wrong way.
    let toward = |sign: f32| {
        commits_by_distance_or_flick(
            sign * delta,
            sign * flick,
            th::PAGE_COMMIT_FRAC,
            th::PAGE_FLICK_VELOCITY,
            th::PAGE_FLICK_MIN_FRAC,
        )
    };
    let next = toward(1.0);
    let prev = toward(-1.0);
    if next && page + 1 < page_count {
        page + 1
    } else if prev && page > 0 {
        page - 1
    } else {
        page
    }
}

pub fn is_arrange_page_swipe(dx: f32, width: f32) -> bool {
    dx.abs() > width * th::ARRANGE_PAGE_SWIPE_FRAC
}

/// `dy_down` is positive downward.
pub fn is_pull_down_search(dx: f32, dy_down: f32, height: f32) -> bool {
    dy_down > height * th::PULL_DOWN_SEARCH_FRAC && dy_down > dx.abs()
}

pub fn exceeds_icon_tap_slop(dx: f32, dy: f32) -> bool {
    (dx * dx + dy * dy).sqrt() > th::ICON_TAP_SLOP_PX
}

pub fn exceeds_icon_hold_slop(dx: f32, dy: f32) -> bool {
    (dx * dx + dy * dy).sqrt() > th::ICON_HOLD_SLOP_PX
}

pub fn is_switcher_tap(dx: f32, dy: f32) -> bool {
    dx.abs() < th::SWITCHER_TAP_SLOP_PX && dy.abs() < th::SWITCHER_TAP_SLOP_PX
}

/// `dy_up` is positive upward.
pub fn classify_bar_release(dx: f32, dy_up: f32, width: f32, height: f32) -> BarRelease {
    if dy_up > height * th::BAR_RAISE_FRAC {
        BarRelease::OpenSwitcher
    } else if dx > width * th::BAR_SWITCH_FRAC {
        BarRelease::SlideToTop
    } else {
        BarRelease::None
    }
}

/// `dy` is negative upward; `start_scroll` is the deck position at press.
pub fn classify_card_drag(
    dx: f32,
    dy: f32,
    width: f32,
    height: f32,
    start_scroll: f32,
) -> CardDrag {
    if dy.abs() > dx.abs() {
        let travel = dy / height;
        let progress = if dy < 0.0 {
            // The card rides the finger exactly.
            (-travel).min(1.0)
        } else {
            // Asymptotic, not clamped: a hard cap reads as the card snapping off the
            // finger.
            let max = th::CARD_PUSH_DOWN_MAX;
            -max * (1.0 - (-travel * th::CARD_PUSH_DOWN_RUBBER / max).exp())
        };
        CardDrag::Close { progress }
    } else {
        let per_index = width * th::CARD_SCROLL_PER_INDEX_FRAC;
        CardDrag::Scroll {
            position: start_scroll + dx / per_index,
        }
    }
}

/// Distance at any speed, or a flick past a token distance. `travel` and
/// `velocity` are positive in the committing direction, same units.
pub fn commits_by_distance_or_flick(
    travel: f32,
    velocity: f32,
    commit: f32,
    flick_velocity: f32,
    flick_min: f32,
) -> bool {
    travel >= commit || (velocity >= flick_velocity && travel >= flick_min)
}

/// `vy` is negative upward, the same divide as the in-app fling home.
pub fn card_close_commits(progress: f32, vy: f32) -> bool {
    commits_by_distance_or_flick(
        progress,
        -vy,
        th::CARD_CLOSE_COMMIT,
        th::CARD_CLOSE_FLICK_VELOCITY,
        th::CARD_CLOSE_FLICK_MIN_FRAC,
    )
}

/// In screens, `-1.0..=1.0`. Positive slides right, revealing the older app.
pub fn quick_switch_offset(dx: f32, width: f32, has_prev: bool, has_next: bool) -> f32 {
    let mut f = dx / width;
    let at_end = (f > 0.0 && !has_prev) || (f < 0.0 && !has_next);
    if at_end {
        f *= th::RUBBER_BAND_FOLLOW;
    }
    f.clamp(-1.0, 1.0)
}

pub fn classify_quick_switch_release(
    offset: f32,
    has_prev: bool,
    has_next: bool,
) -> QuickSwitchRelease {
    // Rightward (`prev` slot) holds the older app, so it walks +1; leftward -1.
    // `target` follows the slide direction.
    if offset >= th::QUICK_SWITCH_COMMIT_FRAC && has_prev {
        QuickSwitchRelease::Commit {
            dir: 1,
            target: 1.0,
        }
    } else if offset <= -th::QUICK_SWITCH_COMMIT_FRAC && has_next {
        QuickSwitchRelease::Commit {
            dir: -1,
            target: -1.0,
        }
    } else {
        QuickSwitchRelease::Reject
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1000.0;
    const H: f32 = 2000.0;

    #[test]
    fn page_drag_tracks_the_finger() {
        assert_eq!(page_drag_value(-500.0, W, 0, 3), 0.5);
        assert_eq!(page_drag_value(500.0, W, 1, 3), 0.5);
    }

    #[test]
    fn page_drag_rubber_bands_at_both_ends() {
        assert_eq!(page_drag_value(500.0, W, 0, 3), -0.15);
        assert_eq!(page_drag_value(-500.0, W, 2, 3), 2.0 + 0.15);
    }

    #[test]
    fn single_page_rubber_bands_in_both_directions() {
        assert!(page_drag_value(-500.0, W, 0, 1) > 0.0);
        assert!(page_drag_value(500.0, W, 0, 1) < 0.0);
    }

    #[test]
    fn page_commits_only_past_the_threshold() {
        assert_eq!(page_after_swipe(-290.0, 0.0, W, 0, 3), 0);
        assert_eq!(page_after_swipe(-310.0, 0.0, W, 0, 3), 1);
        assert_eq!(page_after_swipe(310.0, 0.0, W, 1, 3), 0);
    }

    #[test]
    fn quick_flick_pages_short_of_the_distance_threshold() {
        assert_eq!(page_after_swipe(-100.0, -1.2, W, 0, 3), 1);
        assert_eq!(page_after_swipe(100.0, 1.2, W, 1, 3), 0);
        assert_eq!(page_after_swipe(-100.0, -0.2, W, 0, 3), 0);
    }

    #[test]
    fn flick_needs_travel_and_a_matching_direction() {
        assert_eq!(page_after_swipe(-20.0, -2.0, W, 0, 3), 0);
        // Dragged out then snapped back: must not page either way.
        assert_eq!(page_after_swipe(-100.0, 1.5, W, 1, 3), 1);
    }

    #[test]
    fn page_swipe_never_leaves_the_strip() {
        assert_eq!(
            page_after_swipe(310.0, 0.0, W, 0, 3),
            0,
            "no page before the first"
        );
        assert_eq!(
            page_after_swipe(-310.0, 0.0, W, 2, 3),
            2,
            "no page after the last"
        );
        assert_eq!(
            page_after_swipe(-200.0, -2.0, W, 2, 3),
            2,
            "a flick cannot leave the strip either"
        );
    }

    #[test]
    fn arrange_page_swipe_needs_more_travel_than_a_tap() {
        assert!(!is_arrange_page_swipe(140.0, W));
        assert!(is_arrange_page_swipe(160.0, W));
        assert!(
            is_arrange_page_swipe(-160.0, W),
            "direction does not matter"
        );
    }

    #[test]
    fn pull_down_opens_search_only_when_dominantly_downward() {
        assert!(is_pull_down_search(0.0, 200.0, H));
        assert!(!is_pull_down_search(300.0, 200.0, H));
        assert!(!is_pull_down_search(0.0, 100.0, H));
        assert!(!is_pull_down_search(0.0, -300.0, H));
    }

    #[test]
    fn icon_tap_slop_is_radial() {
        assert!(!exceeds_icon_tap_slop(0.0, 0.0));
        assert!(!exceeds_icon_tap_slop(8.0, 8.0), "11.3px is inside 12px");
        assert!(exceeds_icon_tap_slop(9.0, 9.0), "12.7px is outside");
        assert!(exceeds_icon_tap_slop(-13.0, 0.0), "sign does not matter");
        assert!(!exceeds_icon_hold_slop(-13.0, 0.0));
        assert!(exceeds_icon_hold_slop(33.0, 0.0));
    }

    #[test]
    fn switcher_tap_slop_is_per_axis() {
        assert!(is_switcher_tap(14.0, 14.0));
        assert!(!is_switcher_tap(16.0, 0.0));
        assert!(!is_switcher_tap(0.0, -16.0));
    }

    #[test]
    fn bar_swipe_up_opens_the_switcher() {
        assert_eq!(
            classify_bar_release(0.0, 200.0, W, H),
            BarRelease::OpenSwitcher
        );
    }

    #[test]
    fn bar_swipe_up_outranks_a_sideways_component() {
        assert_eq!(
            classify_bar_release(400.0, 200.0, W, H),
            BarRelease::OpenSwitcher
        );
    }

    #[test]
    fn bar_swipe_right_slides_to_the_top_card() {
        assert_eq!(
            classify_bar_release(200.0, 0.0, W, H),
            BarRelease::SlideToTop
        );
        assert_eq!(classify_bar_release(100.0, 0.0, W, H), BarRelease::None);
    }

    #[test]
    fn bar_swipe_left_does_nothing() {
        assert_eq!(classify_bar_release(-400.0, 0.0, W, H), BarRelease::None);
    }

    #[test]
    fn bar_wobble_does_nothing() {
        assert_eq!(classify_bar_release(100.0, 100.0, W, H), BarRelease::None);
    }

    #[test]
    fn dominant_up_drag_closes_a_card() {
        assert_eq!(
            classify_card_drag(0.0, -H, W, H, 0.0),
            CardDrag::Close { progress: 1.0 }
        );
        assert_eq!(
            classify_card_drag(0.0, -H * 2.0, W, H, 0.0),
            CardDrag::Close { progress: 1.0 }
        );
        assert_eq!(
            classify_card_drag(0.0, -500.0, W, H, 0.0),
            CardDrag::Close { progress: 0.25 }
        );
    }

    #[test]
    fn sideways_drag_scrolls_the_carousel() {
        assert_eq!(
            classify_card_drag(420.0, 0.0, W, H, 0.0),
            CardDrag::Scroll { position: 1.0 }
        );
        assert_eq!(
            classify_card_drag(420.0, 0.0, W, H, 2.0),
            CardDrag::Scroll { position: 3.0 }
        );
    }

    #[test]
    fn a_diagonal_drag_needs_up_to_dominate_to_close() {
        assert!(matches!(
            classify_card_drag(300.0, -200.0, W, H, 0.0),
            CardDrag::Scroll { .. }
        ));
        assert!(matches!(
            classify_card_drag(200.0, -300.0, W, H, 0.0),
            CardDrag::Close { .. }
        ));
    }

    #[test]
    fn downward_drag_rubber_bands_below_the_stack() {
        let CardDrag::Close { progress } = classify_card_drag(0.0, 300.0, W, H, 0.0) else {
            panic!("downward drag should be a close drag");
        };
        assert!(progress < 0.0);
        assert!(progress > -0.1, "progress={progress}");
        assert!(!card_close_commits(progress, 0.0));
        assert!(!card_close_commits(progress, -5.0));
    }

    #[test]
    fn downward_drag_caps_and_never_commits() {
        let CardDrag::Close { progress } = classify_card_drag(0.0, 5000.0, W, H, 0.0) else {
            panic!("downward drag should be a close drag");
        };
        assert!(
            (progress + th::CARD_PUSH_DOWN_MAX).abs() < 1e-3,
            "{progress}"
        );
        assert!(!card_close_commits(progress, 0.0));
        let near = |dy: f32| match classify_card_drag(0.0, dy, W, H, 0.0) {
            CardDrag::Close { progress } => progress,
            _ => panic!("downward drag should be a close drag"),
        };
        let step = near(600.0) - near(560.0);
        assert!(step < 0.0 && step > -0.01, "step={step}");
    }

    #[test]
    fn upward_drag_tracks_the_finger_one_to_one() {
        let CardDrag::Close { progress } = classify_card_drag(0.0, -H * 0.3, W, H, 0.0) else {
            panic!("upward drag should be a close drag");
        };
        assert!((progress - 0.3).abs() < 1e-6, "progress={progress}");
    }

    #[test]
    fn card_close_commits_on_distance_at_any_speed() {
        let just_under = th::CARD_CLOSE_COMMIT - 0.01;
        assert!(!card_close_commits(just_under, 0.0));
        assert!(card_close_commits(th::CARD_CLOSE_COMMIT, 0.0));
        assert!(card_close_commits(th::CARD_CLOSE_COMMIT, -0.05));
    }

    #[test]
    fn card_close_commits_on_a_short_upward_flick() {
        let short = th::CARD_CLOSE_COMMIT / 2.0;
        assert!(card_close_commits(short, -th::CARD_CLOSE_FLICK_VELOCITY));
        assert!(!card_close_commits(short, th::CARD_CLOSE_FLICK_VELOCITY));
        assert!(!card_close_commits(0.001, -5.0));
    }

    #[test]
    fn quick_switch_offset_tracks_and_clamps() {
        assert_eq!(quick_switch_offset(500.0, W, true, true), 0.5);
        assert_eq!(quick_switch_offset(-500.0, W, true, true), -0.5);
        assert_eq!(quick_switch_offset(3000.0, W, true, true), 1.0);
    }

    #[test]
    fn quick_switch_offset_rubber_bands_at_the_stack_ends() {
        assert_eq!(quick_switch_offset(500.0, W, false, true), 0.15);
        assert_eq!(quick_switch_offset(-500.0, W, true, false), -0.15);
    }

    #[test]
    fn quick_switch_commits_past_threshold_when_an_app_is_there() {
        assert_eq!(
            classify_quick_switch_release(0.25, true, true),
            QuickSwitchRelease::Commit {
                dir: 1,
                target: 1.0
            }
        );
        assert_eq!(
            classify_quick_switch_release(-0.25, true, true),
            QuickSwitchRelease::Commit {
                dir: -1,
                target: -1.0
            }
        );
    }

    #[test]
    fn quick_switch_rejects_a_short_slide() {
        assert_eq!(
            classify_quick_switch_release(0.15, true, true),
            QuickSwitchRelease::Reject
        );
    }

    #[test]
    fn quick_switch_rejects_when_there_is_no_app_that_way() {
        assert_eq!(
            classify_quick_switch_release(0.5, false, true),
            QuickSwitchRelease::Reject
        );
        assert_eq!(
            classify_quick_switch_release(-0.5, true, false),
            QuickSwitchRelease::Reject
        );
    }
}
