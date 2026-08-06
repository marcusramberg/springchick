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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarRelease {
    /// Swiped up: raise the most recent app (a deliberate jump).
    RaiseRecent,
    /// Swiped sideways: walk the MRU cursor. `-1` = more-recent/previous,
    /// `+1` = older/next, matching [`crate::NavTarget::QuickSwitch`].
    QuickSwitch(i32),
    /// Neither threshold reached — the press was a tap or a stray wobble.
    None,
}

/// What a drag on a switcher card is currently doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardDrag {
    /// Dominantly upward: closing this card, tracked live. `0.0..=1.0`.
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
/// Commits to a neighbour only past [`th::PAGE_COMMIT_FRAC`] of the width, and
/// never past either end of the page strip.
pub fn page_after_swipe(dx: f32, width: f32, page: usize, page_count: usize) -> usize {
    let delta = -dx / width; // positive = swiping toward the next page
    if delta > th::PAGE_COMMIT_FRAC && page + 1 < page_count {
        page + 1
    } else if delta < -th::PAGE_COMMIT_FRAC && page > 0 {
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
        BarRelease::RaiseRecent
    } else if dx.abs() > width * th::BAR_SWITCH_FRAC {
        // Swiping left walks toward older apps (+1), matching the carousel.
        BarRelease::QuickSwitch(if dx < 0.0 { 1 } else { -1 })
    } else {
        BarRelease::None
    }
}

/// Classify a live drag on a switcher card. `dx`/`dy` are from the press point,
/// `dy` negative upward; `start_scroll` is the deck position when it began.
pub fn classify_card_drag(
    dx: f32,
    dy: f32,
    width: f32,
    height: f32,
    start_scroll: f32,
) -> CardDrag {
    if dy < 0.0 && dy.abs() > dx.abs() {
        let progress = ((-dy) / (height * th::CARD_CLOSE_FULL_RISE)).clamp(0.0, 1.0);
        CardDrag::Close { progress }
    } else {
        let per_index = width * th::CARD_SCROLL_PER_INDEX_FRAC;
        CardDrag::Scroll {
            position: start_scroll + dx / per_index,
        }
    }
}

/// Whether a released card-close drag has passed the commit threshold.
pub fn card_close_commits(progress: f32) -> bool {
    progress >= th::CARD_CLOSE_COMMIT
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
        // 29% of a screen is not enough; 31% is.
        assert_eq!(page_after_swipe(-290.0, W, 0, 3), 0);
        assert_eq!(page_after_swipe(-310.0, W, 0, 3), 1);
        assert_eq!(page_after_swipe(310.0, W, 1, 3), 0);
    }

    #[test]
    fn page_swipe_never_leaves_the_strip() {
        assert_eq!(
            page_after_swipe(310.0, W, 0, 3),
            0,
            "no page before the first"
        );
        assert_eq!(
            page_after_swipe(-310.0, W, 2, 3),
            2,
            "no page after the last"
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
    fn bar_swipe_up_raises_the_recent_app() {
        assert_eq!(
            classify_bar_release(0.0, 200.0, W, H),
            BarRelease::RaiseRecent
        );
    }

    #[test]
    fn bar_swipe_up_outranks_a_sideways_component() {
        // Both thresholds cleared: up wins.
        assert_eq!(
            classify_bar_release(400.0, 200.0, W, H),
            BarRelease::RaiseRecent
        );
    }

    #[test]
    fn bar_swipe_sideways_quick_switches_with_carousel_handedness() {
        // Left (negative dx) walks toward older apps.
        assert_eq!(
            classify_bar_release(-200.0, 0.0, W, H),
            BarRelease::QuickSwitch(1)
        );
        assert_eq!(
            classify_bar_release(200.0, 0.0, W, H),
            BarRelease::QuickSwitch(-1)
        );
    }

    #[test]
    fn bar_wobble_does_nothing() {
        assert_eq!(classify_bar_release(100.0, 100.0, W, H), BarRelease::None);
    }

    // --- switcher cards ---

    #[test]
    fn dominant_up_drag_closes_a_card() {
        // A quarter of the screen height is full close progress.
        assert_eq!(
            classify_card_drag(0.0, -500.0, W, H, 0.0),
            CardDrag::Close { progress: 1.0 }
        );
        assert_eq!(
            classify_card_drag(0.0, -250.0, W, H, 0.0),
            CardDrag::Close { progress: 0.5 }
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
    fn downward_drag_never_closes() {
        assert!(matches!(
            classify_card_drag(0.0, 300.0, W, H, 0.0),
            CardDrag::Scroll { .. }
        ));
    }

    #[test]
    fn card_close_commits_past_threshold() {
        assert!(!card_close_commits(0.39));
        assert!(card_close_commits(0.4));
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
