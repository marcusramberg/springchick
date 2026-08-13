//! Icon context-menu geometry: the panel a long press on a home/dock icon opens,
//! and the rows inside it.
//!
//! Pure like the rest of this crate — the compositor decides *which* rows an app
//! gets, this decides where they land. The panel is anchored to the icon that
//! was held, flipping above it when there is no room below (dock icons always
//! flip) and clamping to the screen margins so a menu on an edge column stays
//! fully visible.

use crate::Rect;

/// Panel width as a fraction of output width.
const PANEL_W_FRAC: f32 = 0.52;
/// Row height as a fraction of output height. Sized as a comfortable touch
/// target rather than to the text.
const ITEM_H_FRAC: f32 = 0.045;
/// Padding inside the panel, above the first row and below the last.
const PAD_FRAC: f32 = 0.008;
/// Gap between the anchor point and the panel edge.
const ANCHOR_GAP_FRAC: f32 = 0.035;
/// Screen margin the panel keeps clear on every side. Matches the home grid's
/// horizontal margin.
const MARGIN_FRAC: f32 = 0.04;

/// A laid-out icon menu.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuLayout {
    /// The panel background.
    pub panel: Rect,
    /// One rect per row, top to bottom, in the order the items were given.
    pub items: Vec<Rect>,
}

/// Lay out a menu of `item_count` rows anchored at `anchor` (normally the center
/// of the icon that was held) on an output of `width` x `height`.
pub fn compute(anchor: (f32, f32), item_count: usize, width: f32, height: f32) -> MenuLayout {
    let margin = width.min(height) * MARGIN_FRAC;
    let pad = height * PAD_FRAC;
    let item_h = height * ITEM_H_FRAC;
    let panel_w = width * PANEL_W_FRAC;
    let panel_h = 2.0 * pad + item_count as f32 * item_h;

    // Centered on the icon, pulled back inside the margins on the edge columns.
    // `max` after `min` so a panel wider than the screen still starts at the
    // left margin rather than off-screen to the right.
    let x = (anchor.0 - panel_w / 2.0)
        .min(width - margin - panel_w)
        .max(margin);

    // Below the icon by default; above it when that would run off the bottom
    // (every dock icon, and the last grid row).
    let gap = height * ANCHOR_GAP_FRAC;
    let below = anchor.1 + gap;
    let y = if below + panel_h <= height - margin {
        below
    } else {
        (anchor.1 - gap - panel_h).max(margin)
    };

    let panel = Rect {
        x,
        y,
        w: panel_w,
        h: panel_h,
    };
    let items = (0..item_count)
        .map(|i| Rect {
            x,
            y: y + pad + i as f32 * item_h,
            w: panel_w,
            h: item_h,
        })
        .collect();
    MenuLayout { panel, items }
}

/// Index of the row at `(x, y)`, or `None` for a point outside the panel (which
/// the caller treats as "dismiss").
pub fn hit_test(menu: &MenuLayout, x: f32, y: f32) -> Option<usize> {
    if !menu.panel.contains(x, y) {
        return None;
    }
    menu.items.iter().position(|r| r.contains(x, y))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: &[(f32, f32)] = &[
        (1224.0, 2700.0), // Fairphone 5, portrait
        (1901.0, 2088.0), // nested winit window
        (2700.0, 1224.0), // rotated
        (720.0, 1440.0),
        (400.0, 800.0),
    ];

    /// Wherever the icon is, the panel stays on screen — that is the whole job
    /// of the clamping, and an edge column is the easy case to get wrong.
    #[test]
    fn panel_stays_on_screen_from_any_anchor() {
        for &(w, h) in SIZES {
            for &(ax, ay) in &[
                (0.0, 0.0),
                (w, 0.0),
                (0.0, h),
                (w, h),
                (w / 2.0, h / 2.0),
                (w * 0.05, h * 0.9),
            ] {
                for count in 1..=5 {
                    let m = compute((ax, ay), count, w, h);
                    assert!(
                        m.panel.x >= 0.0 && m.panel.x + m.panel.w <= w,
                        "{w}x{h} anchor ({ax},{ay}) x{count}: panel {:?} off screen horizontally",
                        m.panel
                    );
                    assert!(
                        m.panel.y >= 0.0 && m.panel.y + m.panel.h <= h,
                        "{w}x{h} anchor ({ax},{ay}) x{count}: panel {:?} off screen vertically",
                        m.panel
                    );
                }
            }
        }
    }

    #[test]
    fn opens_below_a_top_icon_and_above_a_bottom_one() {
        let (w, h) = (1224.0, 2700.0);
        let top = compute((w / 2.0, h * 0.15), 4, w, h);
        assert!(
            top.panel.y > h * 0.15,
            "a top icon's menu should hang below"
        );

        let bottom = compute((w / 2.0, h * 0.9), 4, w, h);
        assert!(
            bottom.panel.y + bottom.panel.h < h * 0.9,
            "a dock icon's menu should flip above"
        );
    }

    #[test]
    fn rows_are_stacked_inside_the_panel_without_gaps() {
        let (w, h) = (1224.0, 2700.0);
        let m = compute((w / 2.0, h * 0.3), 4, w, h);
        assert_eq!(m.items.len(), 4);
        for pair in m.items.windows(2) {
            assert!(
                (pair[1].y - (pair[0].y + pair[0].h)).abs() < 0.01,
                "rows should be flush: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        let first = m.items.first().unwrap();
        let last = m.items.last().unwrap();
        assert!(first.y > m.panel.y, "top padding missing");
        assert!(
            last.y + last.h < m.panel.y + m.panel.h,
            "bottom padding missing"
        );
    }

    #[test]
    fn hit_test_finds_rows_and_rejects_outside() {
        let (w, h) = (1224.0, 2700.0);
        let m = compute((w / 2.0, h * 0.3), 3, w, h);
        for (i, r) in m.items.iter().enumerate() {
            assert_eq!(hit_test(&m, r.center_x(), r.center_y()), Some(i));
        }
        // Outside the panel entirely: the caller dismisses.
        assert_eq!(hit_test(&m, m.panel.x - 1.0, m.panel.y + 1.0), None);
        // Inside the panel but in the padding: no row, and also no dismiss —
        // the caller distinguishes those by testing the panel itself.
        assert_eq!(hit_test(&m, m.panel.center_x(), m.panel.y + 0.5), None);
    }
}
