//! Pure geometry for wlr-layer-shell surfaces.
//!
//! No wayland types: the compositor translates protocol state into these plain
//! inputs and reads back plain [`Rect`]s. Two jobs — reserve screen space for
//! exclusive zones ([`usable_area`]), and place one layer surface against the
//! output ([`layer_rect`]).

use crate::Rect;

/// Screen edge a layer surface anchors to / reserves against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// The four anchor flags of a layer surface. Anchoring to opposite edges
/// stretches the surface across that axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
}

/// Per-edge margins (logical px).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Margins {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

/// One surface's claim on screen space via its exclusive zone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Reservation {
    pub edge: Edge,
    pub size: f32,
}

/// Shrink the full output by each reservation to get the area apps may use.
///
/// Reservations stack on their edge (two bottom bars reserve the sum). The
/// result never collapses past zero on either axis.
pub fn usable_area(output_w: f32, output_h: f32, reserved: &[Reservation]) -> Rect {
    let mut left = 0.0_f32;
    let mut top = 0.0_f32;
    let mut right = output_w;
    let mut bottom = output_h;

    for r in reserved {
        let size = r.size.max(0.0);
        match r.edge {
            Edge::Top => top += size,
            Edge::Bottom => bottom -= size,
            Edge::Left => left += size,
            Edge::Right => right -= size,
        }
    }

    // Clamp so a too-large reservation yields an empty (not negative) area.
    if right < left {
        right = left;
    }
    if bottom < top {
        bottom = top;
    }
    Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    }
}

/// Place one layer surface against the full output.
///
/// `size` is the client's requested size; a zero dimension means "stretch to
/// the anchored span" (valid only when anchored to both edges on that axis).
/// Margins inset from the anchored edges. A surface anchored to neither edge on
/// an axis is centered on that axis.
pub fn layer_rect(
    output_w: f32,
    output_h: f32,
    anchor: Anchor,
    req_w: f32,
    req_h: f32,
    margins: Margins,
) -> Rect {
    // Horizontal.
    let (x, w) = axis(
        output_w,
        anchor.left,
        anchor.right,
        req_w,
        margins.left,
        margins.right,
    );
    // Vertical.
    let (y, h) = axis(
        output_h,
        anchor.top,
        anchor.bottom,
        req_h,
        margins.top,
        margins.bottom,
    );
    Rect { x, y, w, h }
}

/// Resolve one axis (position, length) from anchors, requested length and
/// margins. `near` is top/left, `far` is bottom/right.
fn axis(
    extent: f32,
    anchor_near: bool,
    anchor_far: bool,
    req: f32,
    margin_near: f32,
    margin_far: f32,
) -> (f32, f32) {
    match (anchor_near, anchor_far) {
        // Both edges: stretch across, inset by both margins. A nonzero request
        // is ignored — stretching wins.
        (true, true) => {
            let pos = margin_near;
            let len = (extent - margin_near - margin_far).max(0.0);
            (pos, len)
        }
        // Near edge only: sit against it, inset by the near margin.
        (true, false) => (margin_near, req),
        // Far edge only: sit against it, inset by the far margin.
        (false, true) => (extent - margin_far - req, req),
        // Neither: center.
        (false, false) => ((extent - req) / 2.0, req),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1000.0;
    const H: f32 = 2000.0;

    fn bottom_anchor() -> Anchor {
        Anchor {
            bottom: true,
            left: true,
            right: true,
            top: false,
        }
    }

    #[test]
    fn no_reservations_is_full_output() {
        let u = usable_area(W, H, &[]);
        assert_eq!(u, Rect { x: 0.0, y: 0.0, w: W, h: H });
    }

    #[test]
    fn bottom_reservation_shrinks_height_from_the_bottom() {
        let u = usable_area(W, H, &[Reservation { edge: Edge::Bottom, size: 300.0 }]);
        assert_eq!(u, Rect { x: 0.0, y: 0.0, w: W, h: H - 300.0 });
    }

    #[test]
    fn each_edge_reserves_correctly() {
        assert_eq!(usable_area(W, H, &[Reservation { edge: Edge::Top, size: 100.0 }]).y, 100.0);
        assert_eq!(usable_area(W, H, &[Reservation { edge: Edge::Left, size: 100.0 }]).x, 100.0);
        let right = usable_area(W, H, &[Reservation { edge: Edge::Right, size: 100.0 }]);
        assert_eq!(right.x, 0.0);
        assert_eq!(right.w, W - 100.0);
    }

    #[test]
    fn two_reservations_on_one_edge_stack() {
        let u = usable_area(
            W,
            H,
            &[
                Reservation { edge: Edge::Bottom, size: 200.0 },
                Reservation { edge: Edge::Bottom, size: 100.0 },
            ],
        );
        assert_eq!(u.h, H - 300.0);
    }

    #[test]
    fn oversized_reservation_clamps_to_empty_not_negative() {
        let u = usable_area(W, H, &[Reservation { edge: Edge::Bottom, size: H + 500.0 }]);
        assert_eq!(u.y, 0.0);
        assert_eq!(u.h, 0.0);
    }

    #[test]
    fn bottom_full_width_keyboard_rect() {
        // wvkbd: anchored bottom+left+right, requests a height, no width.
        let r = layer_rect(W, H, bottom_anchor(), 0.0, 400.0, Margins::default());
        assert_eq!(r.x, 0.0);
        assert_eq!(r.w, W);
        assert_eq!(r.h, 400.0);
        assert_eq!(r.y, H - 400.0);
    }

    #[test]
    fn margins_inset_from_the_anchored_edge() {
        let m = Margins { bottom: 20.0, ..Margins::default() };
        let r = layer_rect(W, H, bottom_anchor(), 0.0, 400.0, m);
        // Bottom edge stays anchored; the bottom margin lifts it.
        assert_eq!(r.y, H - 400.0 - 20.0);
    }

    #[test]
    fn top_anchored_bar_sits_at_the_top() {
        let anchor = Anchor { top: true, left: true, right: true, bottom: false };
        let r = layer_rect(W, H, anchor, 0.0, 60.0, Margins::default());
        assert_eq!(r.y, 0.0);
        assert_eq!(r.w, W);
        assert_eq!(r.h, 60.0);
    }

    #[test]
    fn unanchored_axis_is_centered() {
        // Anchored to neither left nor right: centered horizontally.
        let anchor = Anchor { top: true, bottom: true, left: false, right: false };
        let r = layer_rect(W, H, anchor, 200.0, 0.0, Margins::default());
        assert_eq!(r.x, (W - 200.0) / 2.0);
        assert_eq!(r.w, 200.0);
        // Anchored top+bottom: stretched vertically.
        assert_eq!(r.y, 0.0);
        assert_eq!(r.h, H);
    }
}
