//! Home-screen and switcher-deck gesture decisions.
//!
//! [`nav`](crate::nav) classifies the in-app grab gesture; this module covers
//! everything the *shell* interprets — page drags, the pull-down search, the
//! Home bar, the switcher deck, and the live quick-switch slide.
//!
//! Every function here is a pure decision: screen-space numbers in, a verdict
//! out. The compositor's job is to feed them measurements and then apply what
//! they return, so that "what does this gesture mean" is testable without a
//! Wayland display, a GPU, or a compositor `State`.
//!
//! Distances arrive in output pixels and are compared against the fractions in
//! [`thresholds`](crate::thresholds), so behaviour is resolution-independent
//! except where a threshold is deliberately in pixels (finger jitter).

use crate::thresholds as th;

/// What a released drag on the Home bar means.
///
/// The Home bar is the inverse of the in-app grab: from Home the deck is what
/// you reach for, not an individual app. Swiping up opens the switcher, swiping
/// right slides straight onto the app at the front of the stack. A leftward
/// swipe has nothing behind it (the stack only extends one way from Home), so it
/// is deliberately inert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarRelease {
    /// Swiped up: animate into the app switcher deck (or bounce if nothing is
    /// running).
    OpenSwitcher,
    /// Swiped right: slide Home leftwards onto the top card of the stack.
    SlideToTop,
    /// Neither threshold reached — the press was a tap or a stray wobble.
    None,
}

/// What a drag on a switcher card is currently doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardDrag {
    /// Dominantly vertical: the close axis, tracked live. Positive up to `1.0`
    /// (toward closing), negative down to a small rubber-banded push below the
    /// stack that never commits.
    Close { progress: f32 },
    /// Otherwise: panning the carousel to this scroll position, in cards.
    Scroll { position: f32 },
}

/// What a released live quick-switch slide means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QuickSwitchRelease {
    /// Past the threshold with an app in that direction: commit to it. `dir`
    /// walks the MRU cursor; `target` is where the offset spring settles.
    Commit { dir: i32, target: f32 },
    /// Short of the threshold, or nothing in that direction: spring back.
    Reject,
}

/// Live page-spring value for a page drag of `dx` output pixels.
///
/// Tracks the finger directly (no spring physics while dragging) and
/// rubber-bands past the first and last page. Returned in pages, so `1.5` means
/// halfway between pages 1 and 2.
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

/// The page a released page drag of `dx` output pixels settles on.
///
/// Two ways to commit to a neighbour, either alone enough:
/// - distance: past [`th::PAGE_COMMIT_FRAC`] of the width, at any speed;
/// - flick: moving faster than [`th::PAGE_FLICK_VELOCITY`] in that direction,
///   having covered at least [`th::PAGE_FLICK_MIN_FRAC`].
///
/// Without the flick case a quick swipe that lets go early — the natural way to
/// page — dies short of 30% and springs back. `vx` is the release velocity in
/// fractions of output width per second, positive rightward (toward the
/// *previous* page). Never settles past either end of the page strip.
pub fn page_after_swipe(dx: f32, vx: f32, width: f32, page: usize, page_count: usize) -> usize {
    let delta = -dx / width; // positive = swiping toward the next page
    let flick = -vx; // positive = flicking toward the next page
                     // Asked once per direction, so a flick only counts when it agrees with the
                     // travel: a drag out and a snap back must not page the wrong way.
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

/// Whether an arrange-mode empty-area release travelled far enough to be a page
/// swipe rather than a still tap (which exits arrange mode).
pub fn is_arrange_page_swipe(dx: f32, width: f32) -> bool {
    dx.abs() > width * th::ARRANGE_PAGE_SWIPE_FRAC
}

/// Whether a drag from an armed pull-down has become a search gesture: far
/// enough down, and more vertical than horizontal.
///
/// `dy_down` is positive downward; `dx` is signed and compared by magnitude.
pub fn is_pull_down_search(dx: f32, dy_down: f32, height: f32) -> bool {
    dy_down > height * th::PULL_DOWN_SEARCH_FRAC && dy_down > dx.abs()
}

/// Whether movement from an icon press has exceeded the tap slop, making the
/// gesture a swipe and cancelling the pending launch.
pub fn exceeds_icon_tap_slop(dx: f32, dy: f32) -> bool {
    (dx * dx + dy * dy).sqrt() > th::ICON_TAP_SLOP_PX
}

/// Whether a press on the switcher deck stayed still enough to be a tap.
pub fn is_switcher_tap(dx: f32, dy: f32) -> bool {
    dx.abs() < th::SWITCHER_TAP_SLOP_PX && dy.abs() < th::SWITCHER_TAP_SLOP_PX
}

/// Classify a released drag on the Home bar. `dy_up` is positive upward.
pub fn classify_bar_release(dx: f32, dy_up: f32, width: f32, height: f32) -> BarRelease {
    if dy_up > height * th::BAR_RAISE_FRAC {
        BarRelease::OpenSwitcher
    } else if dx > width * th::BAR_SWITCH_FRAC {
        // Only rightward: the stack sits to the right of Home (carousel
        // handedness), so a leftward swipe would be walking off the near end.
        BarRelease::SlideToTop
    } else {
        BarRelease::None
    }
}

/// Classify a live drag on a switcher card. `dx`/`dy` are from the press point,
/// `dy` negative upward; `start_scroll` is the deck position when it began.
///
/// A vertically-dominant drag is a close drag either way: upward gives positive
/// progress toward the commit threshold, downward a small rubber-banded
/// negative progress that always springs back.
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
            // Upward: the card rides the finger exactly (progress is in screen
            // heights, and the deck lifts a card by `progress * height`).
            (-travel).min(1.0)
        } else {
            // Downward: nothing to commit to below the stack, so the card only
            // rubber-bands a short way and springs back on release.
            -(travel * th::CARD_PUSH_DOWN_RUBBER).min(th::CARD_PUSH_DOWN_MAX)
        };
        CardDrag::Close { progress }
    } else {
        let per_index = width * th::CARD_SCROLL_PER_INDEX_FRAC;
        CardDrag::Scroll {
            position: start_scroll + dx / per_index,
        }
    }
}

/// Whether a released drag commits, by either of the two routes every drag in
/// this shell uses: carried far enough (`commit`) at any speed, or flicked —
/// faster than `flick_velocity` *and* past a token `flick_min`, so a fast
/// jitter can't trigger it.
///
/// `travel` and `velocity` are both positive in the committing direction and in
/// the same units (fractions of the relevant screen axis, per second for
/// velocity), so a gesture that commits *upward* passes the negation of its
/// screen-space y values.
pub fn commits_by_distance_or_flick(
    travel: f32,
    velocity: f32,
    commit: f32,
    flick_velocity: f32,
    flick_min: f32,
) -> bool {
    travel >= commit || (velocity >= flick_velocity && travel >= flick_min)
}

/// Whether a released card-close drag actually closes the card. `vy` is the
/// release velocity in fractions of screen height per second, negative upward —
/// the same figure (and the same flick divide) the in-app grab classifies a
/// fling home with. A downward push (negative progress) never commits.
pub fn card_close_commits(progress: f32, vy: f32) -> bool {
    commits_by_distance_or_flick(
        progress,
        -vy,
        th::CARD_CLOSE_COMMIT,
        th::CARD_CLOSE_FLICK_VELOCITY,
        th::CARD_CLOSE_FLICK_MIN_FRAC,
    )
}

/// Live offset (in screens, `-1.0..=1.0`) for a quick-switch slide of `dx`
/// output pixels, rubber-banding when there is no app in that direction.
///
/// Positive offset slides right, revealing the older/`prev` app.
pub fn quick_switch_offset(dx: f32, width: f32, has_prev: bool, has_next: bool) -> f32 {
    let mut f = dx / width;
    let at_end = (f > 0.0 && !has_prev) || (f < 0.0 && !has_next);
    if at_end {
        f *= th::RUBBER_BAND_FOLLOW;
    }
    f.clamp(-1.0, 1.0)
}

/// Classify a released quick-switch slide sitting at `offset`.
pub fn classify_quick_switch_release(
    offset: f32,
    has_prev: bool,
    has_next: bool,
) -> QuickSwitchRelease {
    // The `prev` slot (rightward slide) holds the older/next app, so committing
    // it walks the cursor forward (+1); the `next` slot (leftward) holds the
    // more-recent app (-1). `target` follows the slide direction, not the app.
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

    // --- page drag ---

    #[test]
    fn page_drag_tracks_the_finger() {
        // Half a screen dragged left sits halfway to the next page.
        assert_eq!(page_drag_value(-500.0, W, 0, 3), 0.5);
        // Dragging right from page 1 walks back toward page 0.
        assert_eq!(page_drag_value(500.0, W, 1, 3), 0.5);
    }

    #[test]
    fn page_drag_rubber_bands_at_both_ends() {
        // Past the first page: only 30% of the travel is followed.
        assert_eq!(page_drag_value(500.0, W, 0, 3), -0.15);
        // Past the last page, likewise.
        assert_eq!(page_drag_value(-500.0, W, 2, 3), 2.0 + 0.15);
    }

    #[test]
    fn single_page_rubber_bands_in_both_directions() {
        assert!(page_drag_value(-500.0, W, 0, 1) > 0.0);
        assert!(page_drag_value(500.0, W, 0, 1) < 0.0);
    }

    #[test]
    fn page_commits_only_past_the_threshold() {
        // 29% of a screen is not enough at a standstill; 31% is.
        assert_eq!(page_after_swipe(-290.0, 0.0, W, 0, 3), 0);
        assert_eq!(page_after_swipe(-310.0, 0.0, W, 0, 3), 1);
        assert_eq!(page_after_swipe(310.0, 0.0, W, 1, 3), 0);
    }

    #[test]
    fn quick_flick_pages_short_of_the_distance_threshold() {
        // 10% of the width, but still moving fast leftward: pages forward.
        assert_eq!(page_after_swipe(-100.0, -1.2, W, 0, 3), 1);
        // Same travel rightward from page 1: pages back.
        assert_eq!(page_after_swipe(100.0, 1.2, W, 1, 3), 0);
        // Slow drag over the same distance still springs back.
        assert_eq!(page_after_swipe(-100.0, -0.2, W, 0, 3), 0);
    }

    #[test]
    fn flick_needs_travel_and_a_matching_direction() {
        // Fast but barely moved — a jittery tap, not a swipe.
        assert_eq!(page_after_swipe(-20.0, -2.0, W, 0, 3), 0);
        // Dragged out then snapped back: velocity points away from the travel,
        // so it must not page in either direction.
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

    // --- pull-down search ---

    #[test]
    fn pull_down_opens_search_only_when_dominantly_downward() {
        assert!(is_pull_down_search(0.0, 200.0, H));
        // Far enough down, but more sideways than down → still a page swipe.
        assert!(!is_pull_down_search(300.0, 200.0, H));
        // Dominantly downward but too short.
        assert!(!is_pull_down_search(0.0, 100.0, H));
        // Upward never opens search.
        assert!(!is_pull_down_search(0.0, -300.0, H));
    }

    // --- tap slop ---

    #[test]
    fn icon_tap_slop_is_radial() {
        assert!(!exceeds_icon_tap_slop(0.0, 0.0));
        assert!(!exceeds_icon_tap_slop(8.0, 8.0), "11.3px is inside 12px");
        assert!(exceeds_icon_tap_slop(9.0, 9.0), "12.7px is outside");
        assert!(exceeds_icon_tap_slop(-13.0, 0.0), "sign does not matter");
    }

    #[test]
    fn switcher_tap_slop_is_per_axis() {
        assert!(is_switcher_tap(14.0, 14.0));
        assert!(!is_switcher_tap(16.0, 0.0));
        assert!(!is_switcher_tap(0.0, -16.0));
    }

    // --- home bar ---

    #[test]
    fn bar_swipe_up_opens_the_switcher() {
        assert_eq!(
            classify_bar_release(0.0, 200.0, W, H),
            BarRelease::OpenSwitcher
        );
    }

    #[test]
    fn bar_swipe_up_outranks_a_sideways_component() {
        // Both thresholds cleared: up wins.
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
        // Short of the threshold is still nothing.
        assert_eq!(classify_bar_release(100.0, 0.0, W, H), BarRelease::None);
    }

    #[test]
    fn bar_swipe_left_does_nothing() {
        // Home is the near end of the stack — there is nothing to the left.
        assert_eq!(classify_bar_release(-400.0, 0.0, W, H), BarRelease::None);
    }

    #[test]
    fn bar_wobble_does_nothing() {
        assert_eq!(classify_bar_release(100.0, 100.0, W, H), BarRelease::None);
    }

    // --- switcher cards ---

    #[test]
    fn dominant_up_drag_closes_a_card() {
        // Progress is the finger's own travel, in screen heights, capped at a
        // full screen.
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
        // 42% of the width advances exactly one card.
        assert_eq!(
            classify_card_drag(420.0, 0.0, W, H, 0.0),
            CardDrag::Scroll { position: 1.0 }
        );
        // Scrolling is relative to where the drag began.
        assert_eq!(
            classify_card_drag(420.0, 0.0, W, H, 2.0),
            CardDrag::Scroll { position: 3.0 }
        );
    }

    #[test]
    fn a_diagonal_drag_needs_up_to_dominate_to_close() {
        // More sideways than up → scroll, not close.
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
        // Negative (below rest), and well short of the finger's own travel:
        // 300px of a 2000px-high screen would be -0.15 unbanded.
        assert!(progress < 0.0);
        assert!(progress > -0.1, "progress={progress}");
        // Never commits, at any release speed.
        assert!(!card_close_commits(progress, 0.0));
        assert!(!card_close_commits(progress, -5.0));
    }

    #[test]
    fn downward_drag_caps_and_never_commits() {
        let CardDrag::Close { progress } = classify_card_drag(0.0, 5000.0, W, H, 0.0) else {
            panic!("downward drag should be a close drag");
        };
        assert!(
            (progress + th::CARD_PUSH_DOWN_MAX).abs() < 1e-6,
            "{progress}"
        );
        assert!(!card_close_commits(progress, 0.0));
    }

    #[test]
    fn upward_drag_tracks_the_finger_one_to_one() {
        // The card rides the finger exactly: progress is in screen heights.
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
        // A slow drag past the distance still commits.
        assert!(card_close_commits(th::CARD_CLOSE_COMMIT, -0.05));
    }

    #[test]
    fn card_close_commits_on_a_short_upward_flick() {
        let short = th::CARD_CLOSE_COMMIT / 2.0;
        // Fast enough up (negative vy) and past the token distance.
        assert!(card_close_commits(short, -th::CARD_CLOSE_FLICK_VELOCITY));
        // Same speed downward never closes.
        assert!(!card_close_commits(short, th::CARD_CLOSE_FLICK_VELOCITY));
        // A fast flick that barely moved is jitter, not a close.
        assert!(!card_close_commits(0.001, -5.0));
    }

    // --- quick switch ---

    #[test]
    fn quick_switch_offset_tracks_and_clamps() {
        assert_eq!(quick_switch_offset(500.0, W, true, true), 0.5);
        assert_eq!(quick_switch_offset(-500.0, W, true, true), -0.5);
        // Never past a whole screen.
        assert_eq!(quick_switch_offset(3000.0, W, true, true), 1.0);
    }

    #[test]
    fn quick_switch_offset_rubber_bands_at_the_stack_ends() {
        // Sliding right with no older app behind: only 30% is followed.
        assert_eq!(quick_switch_offset(500.0, W, false, true), 0.15);
        // Sliding left with nothing more recent.
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
