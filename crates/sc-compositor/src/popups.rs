//! Popup geometry and dismiss logic, in physical pixels.

/// Keep a popup inside the output. When it is larger than the output the
/// top-left edge wins.
pub fn clamp_origin(origin: (i32, i32), size: (i32, i32), output: (i32, i32)) -> (i32, i32) {
    let clamp = |pos: i32, extent: i32, bound: i32| -> i32 { (pos.min(bound - extent)).max(0) };
    (
        clamp(origin.0, size.0, output.0),
        clamp(origin.1, size.1, output.1),
    )
}

/// The rect to unconstrain a popup against, in its positioner's logical space
/// (relative to the parent). Without it the client's flip/slide adjustments
/// never apply and bottom-anchored menus land on the app's chrome.
///
/// `area` and `root_origin` are physical; `toplevel_coords` is the logical
/// offset from the root to the popup's parent.
pub fn unconstrain_target(
    area: (i32, i32, i32, i32),
    root_origin: (i32, i32),
    toplevel_coords: (i32, i32),
    dpi: f64,
) -> (i32, i32, i32, i32) {
    let to_logical = |v: i32| (v as f64 / dpi).round() as i32;
    (
        to_logical(area.0 - root_origin.0) - toplevel_coords.0,
        to_logical(area.1 - root_origin.1) - toplevel_coords.1,
        to_logical(area.2),
        to_logical(area.3),
    )
}

/// Indices to dismiss, leaf-first, for a chain ordered root→leaf. A miss
/// dismisses everything; hitting popup `i` dismisses only its descendants.
pub fn popups_to_dismiss(chain_len: usize, hit: Option<usize>) -> Vec<usize> {
    let keep_through = match hit {
        None => return (0..chain_len).rev().collect(),
        Some(i) => i,
    };
    ((keep_through + 1)..chain_len).rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_leaves_on_screen_popup_untouched() {
        assert_eq!(
            clamp_origin((100, 200), (300, 400), (1080, 2400)),
            (100, 200)
        );
    }

    #[test]
    fn clamp_shifts_popup_overflowing_right_and_bottom() {
        assert_eq!(
            clamp_origin((900, 2300), (300, 400), (1080, 2400)),
            (780, 2000)
        );
    }

    #[test]
    fn clamp_pins_oversized_popup_to_top_left() {
        assert_eq!(clamp_origin((50, 50), (2000, 3000), (1080, 2400)), (0, 0));
    }

    #[test]
    fn target_for_toplevel_rooted_popup_is_area_at_origin() {
        assert_eq!(
            unconstrain_target((0, 0, 1080, 2280), (0, 0), (0, 0), 3.0),
            (0, 0, 360, 760)
        );
    }

    #[test]
    fn target_for_rotated_app_is_the_landscape_area() {
        // Rotated fullscreen app: the area is the axis-swapped output.
        assert_eq!(
            unconstrain_target((0, 0, 2400, 1080), (0, 0), (0, 0), 3.0),
            (0, 0, 800, 360)
        );
    }

    #[test]
    fn clamp_keeps_rotated_popup_inside_landscape_space() {
        assert_eq!(
            clamp_origin((2300, 900), (300, 400), (2400, 1080)),
            (2100, 680)
        );
    }

    #[test]
    fn target_offsets_by_root_origin_and_parent_coords() {
        // Top bar: usable area starts 90px down; parent 20 logical px into the root.
        assert_eq!(
            unconstrain_target((0, 90, 1080, 2190), (0, 90), (0, 20), 3.0),
            (0, -20, 360, 730)
        );
    }

    #[test]
    fn target_is_negative_when_root_drawn_below_area_top() {
        // Bottom-docked layer surface: target extends into negative parent coords.
        assert_eq!(
            unconstrain_target((0, 0, 1080, 2400), (0, 1800), (0, 0), 3.0),
            (0, -600, 360, 800)
        );
    }

    #[test]
    fn dismiss_miss_closes_whole_chain_leaf_first() {
        assert_eq!(popups_to_dismiss(3, None), vec![2, 1, 0]);
    }

    #[test]
    fn dismiss_hit_root_closes_only_descendants() {
        assert_eq!(popups_to_dismiss(3, Some(0)), vec![2, 1]);
    }

    #[test]
    fn dismiss_hit_leaf_closes_nothing() {
        assert_eq!(popups_to_dismiss(3, Some(2)), Vec::<usize>::new());
    }

    #[test]
    fn dismiss_empty_chain_is_noop() {
        assert_eq!(popups_to_dismiss(0, None), Vec::<usize>::new());
    }
}
