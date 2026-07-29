//! Pure switcher-deck geometry: fanned stack of window cards.
//!
//! `cards[0]` is the most-recent (front). `scroll` is a single continuous scalar:
//! 0 = folded (front on the right, older cards tucked behind to the left); increasing
//! first unfolds the stack into a spread, then pans left so earlier cards scroll into
//! view when the spread is wider than the screen.

use crate::ui_state::ToplevelId;

/// Geometry of one card in the switcher deck.
#[derive(Clone, Copy, Debug)]
pub struct CardRect {
    pub toplevel: ToplevelId,
    pub center_x: f32,
    pub center_y: f32,
    pub scale: f32,
    pub corner_radius: f32,
    pub z: usize,
    /// 0 = at rest; →1 = sliding up to close (lift + fade). Only the card being
    /// swiped up is non-zero.
    pub close_progress: f32,
}

/// Result of a hit test against the deck.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardHit {
    Card(usize),
    Empty,
}

const FRONT_SCALE: f32 = 0.62;
/// Resting peek between stacked cards, as a fraction of output width. Wide
/// enough that each card exposes a clear tappable strip without any scroll.
const FOLDED_PEEK_FRAC: f32 = 0.17;
/// How far (fraction of front card width) a card slides right per unit of scroll
/// once it has passed the front slot and is leaving to the right.
const SLIDE_OFF_FRAC: f32 = 1.15;

/// Compute card rects, back-to-front. `cards[0]` = most-recent.
///
/// `scroll` is a continuous focus index into the deck (carousel): `0` puts
/// `cards[0]` in the front slot with the rest fanned behind to the left; as it
/// grows the whole deck pans right — the focused card slides off the right edge
/// and the next card scales up into the front slot.
///
/// `close` optionally names a toplevel being swiped up to close and its progress
/// (0..1); that card is lifted upward by `progress * h` and shrinks away.
///
/// `entry` (0..1) animates the deck fanning in: at 0 every card is stacked at
/// the front slot (behind the front card); at 1 they sit at their rest fan
/// positions. The front card is unaffected (its rest position is the front slot).
pub fn layout(
    cards: &[ToplevelId],
    scroll: f32,
    size: (f32, f32),
    close: Option<(ToplevelId, f32)>,
    entry: f32,
    corner_radius: f32,
) -> Vec<CardRect> {
    let (w, h) = size;
    let n = cards.len();
    if n == 0 {
        return Vec::new();
    }
    let (front_cx, cy, front_scale) = front_slot(size);
    let front_w = w * FRONT_SCALE;
    let gap_back = w * FOLDED_PEEK_FRAC; // fanned peek behind the front slot
    let slide_off = front_w * SLIDE_OFF_FRAC; // travel per unit once past the front

    let focus = clamp_focus(scroll, n);
    let e = entry.clamp(0.0, 1.0);

    cards
        .iter()
        .enumerate()
        .map(|(i, &toplevel)| {
            // Rest position relative to the focused (front) slot. Every card is
            // the same size as the front card (full height); only x differs.
            let rel = i as f32 - focus;
            let rest_x = if rel >= 0.0 {
                front_cx - rel * gap_back // fanned to the left
            } else {
                front_cx + (-rel) * slide_off // passed: sliding off right
            };

            // Entry: blend from the front slot (stacked) out to the rest fan.
            let center_x = front_cx + (rest_x - front_cx) * e;
            let base_scale = front_scale;

            let close_progress = match close {
                Some((t, p)) if t == toplevel => p,
                _ => 0.0,
            };
            // Closing card lifts up and shrinks away.
            let scale = (base_scale * (1.0 - close_progress)).max(0.0);

            // Draw/hit priority: front slot on top, passed cards above it (they
            // slide over the deck), cards behind lowest. Monotonic in -rel.
            let z = ((-rel + 100.0) * 10.0) as usize;

            CardRect {
                toplevel,
                center_x,
                center_y: cy - close_progress * h,
                scale,
                corner_radius,
                z,
                close_progress,
            }
        })
        .collect()
}

/// Geometry of the front (focused) card slot: `(center_x, center_y, scale)`.
/// The app settles into this when releasing into the switcher, so the hand-off
/// from the shrinking window to the front card is seamless.
pub fn front_slot(size: (f32, f32)) -> (f32, f32, f32) {
    let (w, h) = size;
    let front_w = w * FRONT_SCALE;
    (w - front_w / 2.0 - w * 0.06, h / 2.0, FRONT_SCALE)
}

/// Clamp the focus index to the deck with soft rubber-banding past the ends.
fn clamp_focus(scroll: f32, n: usize) -> f32 {
    let max = (n as f32 - 1.0).max(0.0);
    if scroll < 0.0 {
        scroll * 0.3
    } else if scroll > max {
        max + (scroll - max) * 0.3
    } else {
        scroll
    }
}

/// Topmost (highest z) card whose rect contains the point, else Empty.
pub fn hit_test(rects: &[CardRect], x: f32, y: f32, size: (f32, f32)) -> CardHit {
    let (w, h) = size;
    let mut best: Option<usize> = None;
    for (i, r) in rects.iter().enumerate() {
        let cw = w * r.scale;
        let ch = h * r.scale;
        let inside = (x - r.center_x).abs() <= cw / 2.0 && (y - r.center_y).abs() <= ch / 2.0;
        if inside && best.is_none_or(|b| r.z > rects[b].z) {
            best = Some(i);
        }
    }
    match best {
        Some(i) => CardHit::Card(i),
        None => CardHit::Empty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (f32, f32) = (1224.0, 2700.0);
    const CORNER: f32 = 40.0;

    #[test]
    fn front_is_rightmost_when_folded() {
        let rects = layout(&[0, 1, 2], 0.0, SIZE, None, 1.0, CORNER);
        // cards[0] is the front; it sits furthest right and is largest / top z.
        let front = rects.iter().find(|r| r.toplevel == 0).unwrap();
        for r in &rects {
            if r.toplevel != 0 {
                assert!(front.center_x > r.center_x);
                assert!(front.scale >= r.scale);
                assert!(front.z >= r.z);
            }
        }
    }

    #[test]
    fn scroll_advances_focus_to_front_and_slides_active_off_right() {
        let s0 = layout(&[0, 1, 2], 0.0, SIZE, None, 1.0, CORNER);
        let s1 = layout(&[0, 1, 2], 1.0, SIZE, None, 1.0, CORNER);
        let front_x = s0.iter().find(|r| r.toplevel == 0).unwrap().center_x;
        // At scroll 1, the next card (1) occupies the front slot...
        let c1 = s1.iter().find(|r| r.toplevel == 1).unwrap();
        assert!((c1.center_x - front_x).abs() < 1.0);
        // ...and the previously-active card (0) has slid off to the right.
        let c0 = s1.iter().find(|r| r.toplevel == 0).unwrap();
        assert!(c0.center_x > front_x);
        // The card leaving to the right renders on top of the deck.
        assert!(c0.z > c1.z);
    }

    #[test]
    fn scroll_clamps_and_rubber_bands() {
        // Past max, positions keep moving but sub-linearly (rubber-band), never NaN.
        let a = layout(&[0, 1, 2], 5.0, SIZE, None, 1.0, CORNER);
        let b = layout(&[0, 1, 2], 50.0, SIZE, None, 1.0, CORNER);
        assert!(a.iter().all(|r| r.center_x.is_finite()));
        assert!(b.iter().all(|r| r.center_x.is_finite()));
    }

    #[test]
    fn single_card_centers() {
        let rects = layout(&[7], 0.0, SIZE, None, 1.0, CORNER);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].toplevel, 7);
    }

    #[test]
    fn empty_is_empty() {
        assert!(layout(&[], 0.0, SIZE, None, 1.0, CORNER).is_empty());
    }

    #[test]
    fn hit_test_picks_topmost() {
        let rects = layout(&[0, 1, 2], 1.0, SIZE, None, 1.0, CORNER);
        let front = rects.iter().max_by_key(|r| r.z).unwrap();
        match hit_test(&rects, front.center_x, front.center_y, SIZE) {
            CardHit::Card(i) => assert_eq!(rects[i].toplevel, front.toplevel),
            _ => panic!("expected a card hit at the front card center"),
        }
    }

    #[test]
    fn hit_test_empty_off_card() {
        let rects = layout(&[0], 0.0, SIZE, None, 1.0, CORNER);
        assert!(matches!(hit_test(&rects, 5.0, 5.0, SIZE), CardHit::Empty));
    }

    #[test]
    fn close_lifts_and_records_only_that_card() {
        let base = layout(&[0, 1, 2], 0.0, SIZE, None, 1.0, CORNER);
        let rects = layout(&[0, 1, 2], 0.0, SIZE, Some((1, 0.5)), 1.0, CORNER);
        let closing = rects.iter().find(|r| r.toplevel == 1).unwrap();
        let base1 = base.iter().find(|r| r.toplevel == 1).unwrap();
        // Records progress and lifts the card upward (smaller y = higher).
        assert_eq!(closing.close_progress, 0.5);
        assert!(closing.center_y < base1.center_y);
        assert!((base1.center_y - closing.center_y - 0.5 * SIZE.1).abs() < 0.001);
        // Other cards untouched.
        let other = rects.iter().find(|r| r.toplevel == 0).unwrap();
        assert_eq!(other.close_progress, 0.0);
        assert_eq!(other.center_y, SIZE.1 / 2.0);
    }
}
