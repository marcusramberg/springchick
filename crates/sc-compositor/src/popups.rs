//! Popup geometry and dismiss-chain logic.
//!
//! Pure helpers, kept free of smithay/wayland types so they can be unit-tested
//! in isolation. The compositor (`main.rs`, `render.rs`, `touch.rs`) drives them
//! with physical-pixel coordinates it derives from the popup tree.

/// Clamp a popup's physical origin so its rectangle stays fully within the
/// output. If the popup is wider/taller than the output, the top-left edge wins
/// (origin clamped to 0), so at least the anchor-side content is visible.
///
/// `origin`/`size`/`output` are all physical pixels: `(x, y)` / `(w, h)`.
pub fn clamp_origin(origin: (i32, i32), size: (i32, i32), output: (i32, i32)) -> (i32, i32) {
    let clamp = |pos: i32, extent: i32, bound: i32| -> i32 {
        // Push left/up so the far edge fits, then never past the near edge.
        (pos.min(bound - extent)).max(0)
    };
    (
        clamp(origin.0, size.0, output.0),
        clamp(origin.1, size.1, output.1),
    )
}

/// The rectangle a popup's positioner should be unconstrained against, in the
/// **logical** coordinate space its `xdg_positioner` uses (relative to the
/// popup's parent surface).
///
/// Without this the compositor never applies the client's
/// `constraint_adjustment` (flip/slide/resize), so a menu anchored near the
/// bottom of a phone-sized screen is configured half off-screen and then only
/// shoved back on by [`clamp_origin`] — landing on top of the app's own chrome
/// (Firefox's URL-bar menus are the worst case). Feeding this rect to
/// `PositionerState::get_unconstrained_geometry` makes the popup flip above its
/// anchor instead, which is what the client asked for.
///
/// - `area`: physical `(x, y, w, h)` of the on-screen region popups may occupy.
/// - `root_origin`: physical origin of the popup chain's root surface.
/// - `toplevel_coords`: logical offset from that root to the popup's parent.
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

/// Given a popup chain ordered root→leaf and the index of the popup a touch-down
/// landed on (`None` = the tap missed every popup), return the indices to
/// dismiss, leaf-first (so callers can `send_popup_done` deepest-first).
///
/// - Miss (`None`): dismiss the whole chain.
/// - Hit popup `i`: dismiss only its descendants (`i+1..`), keeping `i` and its
///   ancestors — tapping a parent menu closes open submenus but not itself.
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
        // Right edge 900+300=1200 > 1080 → x = 780. Bottom 2300+400=2700 > 2400 → y = 2000.
        assert_eq!(
            clamp_origin((900, 2300), (300, 400), (1080, 2400)),
            (780, 2000)
        );
    }

    #[test]
    fn clamp_pins_oversized_popup_to_top_left() {
        // Popup wider than output: far-edge clamp would go negative → pinned to 0.
        assert_eq!(clamp_origin((50, 50), (2000, 3000), (1080, 2400)), (0, 0));
    }

    #[test]
    fn target_for_toplevel_rooted_popup_is_area_at_origin() {
        // App fills the usable area, popup parented straight to it: the target
        // is the usable area in logical px, anchored at the parent's (0, 0).
        assert_eq!(
            unconstrain_target((0, 0, 1080, 2280), (0, 0), (0, 0), 3.0),
            (0, 0, 360, 760)
        );
    }

    #[test]
    fn target_for_rotated_app_is_the_landscape_area() {
        // A rotated fullscreen app lives in its own turned space: the area is
        // the axis-swapped output at that space's origin, so a menu is
        // unconstrained against the landscape height it can actually use.
        assert_eq!(
            unconstrain_target((0, 0, 2400, 1080), (0, 0), (0, 0), 3.0),
            (0, 0, 800, 360)
        );
    }

    #[test]
    fn clamp_keeps_rotated_popup_inside_landscape_space() {
        // Same clamp, applied in the app's space: the bound is the turned
        // output, not the portrait one.
        assert_eq!(
            clamp_origin((2300, 900), (300, 400), (2400, 1080)),
            (2100, 680)
        );
    }

    #[test]
    fn target_offsets_by_root_origin_and_parent_coords() {
        // Usable area starts 90px down (a top bar), root drawn at that origin,
        // and the popup's parent sits 20 logical px into the root.
        assert_eq!(
            unconstrain_target((0, 90, 1080, 2190), (0, 90), (0, 20), 3.0),
            (0, -20, 360, 730)
        );
    }

    #[test]
    fn target_is_negative_when_root_drawn_below_area_top() {
        // A bottom-docked layer surface: the area's top edge is above the root,
        // so the target extends upward into negative parent-local coords.
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
