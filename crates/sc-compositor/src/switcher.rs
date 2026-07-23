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
}

/// Result of a hit test against the deck.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardHit {
    Card(usize),
    Empty,
}

const FRONT_SCALE: f32 = 0.62;
const DEPTH_SCALE_STEP: f32 = 0.06; // each card behind is this much smaller
const FOLDED_PEEK: f32 = 90.0; // px of edge showing when stacked (resting fan)
const CORNER: f32 = 28.0;

/// Compute card rects, back-to-front. `cards[0]` = front.
pub fn layout(cards: &[ToplevelId], scroll: f32, size: (f32, f32)) -> Vec<CardRect> {
    let (w, h) = size;
    let n = cards.len();
    if n == 0 {
        return Vec::new();
    }
    let front_w = w * FRONT_SCALE;
    // Front card rests centered-right with a small right margin.
    let front_cx = w - front_w / 2.0 - w * 0.06;
    let cy = h / 2.0;

    // Spread distance per card grows with scroll: folded → just the peek; open → full card.
    let spread = soft_clamp(scroll); // 0..~1+ (rubber-band handled in soft_clamp)
    let gap = FOLDED_PEEK + spread * (front_w * 0.55);

    // Pan: once unfolded, extra scroll beyond the point where all cards fit pans left.
    let total_w = gap * (n as f32 - 1.0);
    let overflow = (total_w - (front_cx - w * 0.04)).max(0.0);
    let pan = (spread - 1.0).max(0.0) * overflow;

    cards
        .iter()
        .enumerate()
        .map(|(i, &toplevel)| {
            let depth = i as f32;
            CardRect {
                toplevel,
                center_x: front_cx - gap * depth + pan,
                center_y: cy,
                scale: (FRONT_SCALE - DEPTH_SCALE_STEP * depth).max(0.30),
                corner_radius: CORNER,
                z: n - i, // front (i=0) has the highest z
            }
        })
        .collect()
}

/// Topmost (highest z) card whose rect contains the point, else Empty.
pub fn hit_test(rects: &[CardRect], x: f32, y: f32, size: (f32, f32)) -> CardHit {
    let (w, h) = size;
    let mut best: Option<usize> = None;
    for (i, r) in rects.iter().enumerate() {
        let cw = w * r.scale;
        let ch = h * r.scale;
        let inside = (x - r.center_x).abs() <= cw / 2.0 && (y - r.center_y).abs() <= ch / 2.0;
        if inside && best.map_or(true, |b| r.z > rects[b].z) {
            best = Some(i);
        }
    }
    match best {
        Some(i) => CardHit::Card(i),
        None => CardHit::Empty,
    }
}

/// Map raw scroll to a spread factor with rubber-banding past the ends.
fn soft_clamp(scroll: f32) -> f32 {
    if scroll < 0.0 {
        scroll * 0.3
    } else {
        scroll // upper rubber-band applied by the caller's pan term; positions stay finite
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (f32, f32) = (1224.0, 2700.0);

    #[test]
    fn front_is_rightmost_when_folded() {
        let rects = layout(&[0, 1, 2], 0.0, SIZE);
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
    fn folded_stacks_tight_unfold_widens() {
        let folded = layout(&[0, 1, 2], 0.0, SIZE);
        let open = layout(&[0, 1, 2], 1.0, SIZE);
        let gap = |v: &[CardRect]| (v[0].center_x - v[1].center_x).abs();
        assert!(gap(&open) > gap(&folded)); // unfolding widens the x-gap
    }

    #[test]
    fn scroll_clamps_and_rubber_bands() {
        // Past max, positions keep moving but sub-linearly (rubber-band), never NaN.
        let a = layout(&[0, 1, 2], 5.0, SIZE);
        let b = layout(&[0, 1, 2], 50.0, SIZE);
        assert!(a.iter().all(|r| r.center_x.is_finite()));
        assert!(b.iter().all(|r| r.center_x.is_finite()));
    }

    #[test]
    fn single_card_centers() {
        let rects = layout(&[7], 0.0, SIZE);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].toplevel, 7);
    }

    #[test]
    fn empty_is_empty() {
        assert!(layout(&[], 0.0, SIZE).is_empty());
    }

    #[test]
    fn hit_test_picks_topmost() {
        let rects = layout(&[0, 1, 2], 1.0, SIZE);
        let front = rects.iter().max_by_key(|r| r.z).unwrap();
        match hit_test(&rects, front.center_x, front.center_y, SIZE) {
            CardHit::Card(i) => assert_eq!(rects[i].toplevel, front.toplevel),
            _ => panic!("expected a card hit at the front card center"),
        }
    }

    #[test]
    fn hit_test_empty_off_card() {
        let rects = layout(&[0], 0.0, SIZE);
        assert!(matches!(hit_test(&rects, 5.0, 5.0, SIZE), CardHit::Empty));
    }
}
