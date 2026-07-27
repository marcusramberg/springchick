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
        assert_eq!(clamp_origin((100, 200), (300, 400), (1080, 2400)), (100, 200));
    }

    #[test]
    fn clamp_shifts_popup_overflowing_right_and_bottom() {
        // Right edge 900+300=1200 > 1080 → x = 780. Bottom 2300+400=2700 > 2400 → y = 2000.
        assert_eq!(clamp_origin((900, 2300), (300, 400), (1080, 2400)), (780, 2000));
    }

    #[test]
    fn clamp_pins_oversized_popup_to_top_left() {
        // Popup wider than output: far-edge clamp would go negative → pinned to 0.
        assert_eq!(clamp_origin((50, 50), (2000, 3000), (1080, 2400)), (0, 0));
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
