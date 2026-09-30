//! Switcher deck geometry. `cards[0]` is the most recent. `scroll` is a
//! continuous focus index: 0 has the front card on the right with older cards
//! fanned behind to the left.

use crate::ui_state::ToplevelId;
use sc_anim::Spring;

#[derive(Clone, Copy, Debug)]
pub struct CardRect {
    pub toplevel: ToplevelId,
    pub center_x: f32,
    pub center_y: f32,
    pub scale: f32,
    pub corner_radius: f32,
    pub z: usize,
    /// Fades the live grab-preview fan; 1.0 once settled.
    pub alpha: f32,
    /// Scrim that grows with depth so back cards read as receding.
    pub dim: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CardHit {
    Card(usize),
    Empty,
}

const FRONT_SCALE: f32 = 0.62;
/// Peek between stacked cards, fraction of output width.
const FOLDED_PEEK_FRAC: f32 = 0.17;
/// Per unit of scroll, once a card has passed the front slot.
const SLIDE_OFF_FRAC: f32 = 1.15;
/// The newest passed card parks this much of its width at the right edge;
/// older passed cards slide off.
const PASSED_PEEK_FRAC: f32 = 0.28;
const DIM_PER_STEP: f32 = 0.16;
const DIM_MAX: f32 = 0.55;

fn depth_dim(depth: f32) -> f32 {
    (depth.max(0.0) * DIM_PER_STEP).min(DIM_MAX)
}

/// Card rects, back-to-front. As `scroll` grows the focused card slides off
/// right, parks as the right-hand sliver, and pushes the previous sliver off.
///
/// `close` is a toplevel being dragged on the close axis and its signed
/// progress: positive lifts it by `progress * h`, negative pushes it down.
/// Close is a slide, never a shrink.
pub fn layout(
    cards: &[ToplevelId],
    scroll: f32,
    size: (f32, f32),
    close: Option<(ToplevelId, f32)>,
    corner_radius: f32,
) -> Vec<CardRect> {
    let (w, h) = size;
    let n = cards.len();
    if n == 0 {
        return Vec::new();
    }
    let (front_cx, cy, front_scale) = front_slot(size);
    let front_w = w * FRONT_SCALE;
    let gap_back = w * FOLDED_PEEK_FRAC;
    let slide_off = front_w * SLIDE_OFF_FRAC;
    let passed_cap = w + front_w * (0.5 - PASSED_PEEK_FRAC); // right-edge park

    let focus = clamp_focus(scroll, n);

    cards
        .iter()
        .enumerate()
        .map(|(i, &toplevel)| {
            let rel = i as f32 - focus;
            let center_x = if rel >= 0.0 {
                front_cx - rel * gap_back
            } else {
                // The newest passed card parks at the right edge; each older one continues
                // a card-width further right, off screen.
                let p = -rel;
                if p <= 1.0 {
                    (front_cx + p * slide_off).min(passed_cap)
                } else {
                    passed_cap + (p - 1.0) * front_w
                }
            };
            let close_progress = match close {
                Some((t, p)) if t == toplevel => p,
                _ => 0.0,
            };

            // The parked card nearest the focus is topmost, so the sliver is what you
            // came from, not the MRU front.
            let z = if rel < 0.0 {
                (2000.0 + rel * 10.0) as usize
            } else {
                (1000.0 - rel * 10.0) as usize
            };

            CardRect {
                toplevel,
                center_x,
                center_y: cy - close_progress * h,
                scale: front_scale,
                corner_radius,
                z,
                alpha: 1.0,
                dim: depth_dim(rel),
            }
        })
        .collect()
}

/// The grab gesture's live fan: neighbours to the left of a finger-driven
/// front card. `cards[0]` is not returned; the scene draws it.
pub fn fan_around(
    front_cx: f32,
    front_cy: f32,
    scale: f32,
    cards: &[ToplevelId],
    alpha: f32,
    corner: f32,
    size: (f32, f32),
) -> Vec<CardRect> {
    let (w, _h) = size;
    let gap = w * FOLDED_PEEK_FRAC;
    cards
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, &toplevel)| CardRect {
            toplevel,
            center_x: front_cx - i as f32 * gap,
            center_y: front_cy,
            scale,
            corner_radius: corner,
            z: 100usize.saturating_sub(i),
            alpha,
            dim: depth_dim(i as f32),
        })
        .collect()
}

/// `(center_x, center_y, scale)`. A release into the switcher settles here.
pub fn front_slot(size: (f32, f32)) -> (f32, f32, f32) {
    let (w, h) = size;
    let front_w = w * FRONT_SCALE;
    (w - front_w / 2.0 - w * 0.06, h / 2.0, FRONT_SCALE)
}

/// Uses the same rubber-banded focus as the layout.
pub fn focused_card(cards: &[ToplevelId], scroll: f32) -> Option<ToplevelId> {
    let n = cards.len();
    if n == 0 {
        return None;
    }
    let i = clamp_focus(scroll, n).round().clamp(0.0, (n - 1) as f32) as usize;
    cards.get(i).copied()
}

/// Fades for the deck's icon badges and the focused card's title. `visible`
/// follows the deck; `title_alpha` cross-fades on focus change. The title is
/// owned here so the outgoing one survives its fade-out.
pub struct CardChrome {
    visible: Spring,
    title_alpha: Spring,
    shown: Option<ToplevelId>,
    text: String,
}

/// Below this a title counts as gone.
const TITLE_SWAP_ALPHA: f32 = 0.02;

impl CardChrome {
    pub fn new() -> Self {
        Self {
            visible: Spring::new(0.0),
            title_alpha: Spring::new(0.0),
            shown: None,
            text: String::new(),
        }
    }

    /// `focused` is `None` when the deck isn't on screen.
    pub fn advance(&mut self, dt: f32, focused: Option<(ToplevelId, &str)>) {
        self.visible
            .retarget(if focused.is_some() { 1.0 } else { 0.0 });
        self.visible.step(dt);

        match focused {
            Some((id, _)) if self.shown == Some(id) => self.title_alpha.retarget(1.0),
            Some((id, title)) => {
                // Nothing to cross-fade from: adopt at once.
                if self.shown.is_none() || self.title_alpha.value <= TITLE_SWAP_ALPHA {
                    self.shown = Some(id);
                    self.text.clear();
                    self.text.push_str(title);
                    self.title_alpha.value = 0.0;
                    self.title_alpha.velocity = 0.0;
                    self.title_alpha.retarget(1.0);
                } else {
                    self.title_alpha.retarget(0.0);
                }
            }
            None => self.title_alpha.retarget(0.0),
        }
        self.title_alpha.step(dt);
    }

    pub fn icon_alpha(&self) -> f32 {
        self.visible.value.clamp(0.0, 1.0)
    }

    /// `None` once fully faded.
    pub fn title(&self) -> Option<(ToplevelId, &str, f32)> {
        let alpha = self.icon_alpha() * self.title_alpha.value.clamp(0.0, 1.0);
        let id = self.shown?;
        (alpha > TITLE_SWAP_ALPHA && !self.text.is_empty()).then_some((
            id,
            self.text.as_str(),
            alpha,
        ))
    }

    pub fn is_animating(&self) -> bool {
        !self.visible.is_settled() || !self.title_alpha.is_settled()
    }
}

/// Soft rubber-banding past the ends.
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
        let rects = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
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
        let s0 = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
        let s1 = layout(&[0, 1, 2], 1.0, SIZE, None, CORNER);
        let front_x = s0.iter().find(|r| r.toplevel == 0).unwrap().center_x;
        let c1 = s1.iter().find(|r| r.toplevel == 1).unwrap();
        assert!((c1.center_x - front_x).abs() < 1.0);
        let c0 = s1.iter().find(|r| r.toplevel == 0).unwrap();
        assert!(c0.center_x > front_x);
        assert!(c0.z > c1.z);
    }

    #[test]
    fn newest_passed_card_parks_and_the_older_one_leaves() {
        let (w, _) = SIZE;
        let front_w = w * FRONT_SCALE;
        for focus in [1.0_f32, 2.0, 3.0] {
            let rects = layout(&[0, 1, 2, 3], focus, SIZE, None, CORNER);
            let f = focus as usize;
            let sliver = &rects[f - 1];
            let left = sliver.center_x - front_w / 2.0;
            assert!(left < w, "card vanished off the right at focus {focus}");
            assert!(
                (w - left) >= front_w * PASSED_PEEK_FRAC - 0.01,
                "peek too thin at focus {focus}: {}",
                w - left
            );
            for r in &rects[..f - 1] {
                assert!(
                    r.center_x - front_w / 2.0 >= w,
                    "superseded card still on screen at focus {focus}"
                );
            }
        }
    }

    #[test]
    fn right_sliver_is_the_card_above_the_active_one() {
        // The parked pile shows cards[1] (just above focus), not cards[0].
        let rects = layout(&[0, 1, 2, 3], 2.0, SIZE, None, CORNER);
        let top = rects.iter().max_by_key(|r| r.z).unwrap();
        assert_eq!(top.toplevel, 1, "wrong card shows in the right sliver");
        assert!(rects[1].z > rects[0].z, "nearer parked card must win");
    }

    #[test]
    fn scroll_clamps_and_rubber_bands() {
        let a = layout(&[0, 1, 2], 5.0, SIZE, None, CORNER);
        let b = layout(&[0, 1, 2], 50.0, SIZE, None, CORNER);
        assert!(a.iter().all(|r| r.center_x.is_finite()));
        assert!(b.iter().all(|r| r.center_x.is_finite()));
    }

    #[test]
    fn single_card_centers() {
        let rects = layout(&[7], 0.0, SIZE, None, CORNER);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].toplevel, 7);
    }

    #[test]
    fn empty_is_empty() {
        assert!(layout(&[], 0.0, SIZE, None, CORNER).is_empty());
    }

    #[test]
    fn dim_grows_with_depth_and_front_is_clear() {
        let rects = layout(&[0, 1, 2, 3], 0.0, SIZE, None, CORNER);
        let front = rects.iter().find(|r| r.toplevel == 0).unwrap();
        assert_eq!(front.dim, 0.0, "front card is undimmed");
        for w in rects.windows(2) {
            assert!(w[1].dim > w[0].dim, "deeper card must be dimmer");
            assert!(w[1].dim <= DIM_MAX);
        }
    }

    #[test]
    fn dim_ramps_continuously_with_scroll() {
        let a = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER)[1].dim;
        let b = layout(&[0, 1, 2], 0.5, SIZE, None, CORNER)[1].dim;
        assert!(b < a && b > 0.0, "dim {a} -> {b} should ease, not step");
        assert_eq!(layout(&[0, 1, 2], 1.0, SIZE, None, CORNER)[1].dim, 0.0);
    }

    #[test]
    fn passed_cards_are_never_dimmed() {
        let rects = layout(&[0, 1, 2], 1.5, SIZE, None, CORNER);
        assert_eq!(rects[0].dim, 0.0);
    }

    #[test]
    fn fan_neighbours_dim_by_depth() {
        let fan = fan_around(900.0, 1350.0, FRONT_SCALE, &[0, 1, 2], 1.0, CORNER, SIZE);
        assert_eq!(fan.len(), 2, "front card is not returned");
        assert!(fan[0].dim > 0.0 && fan[1].dim > fan[0].dim);
    }

    #[test]
    fn focused_card_follows_the_scroll() {
        assert_eq!(focused_card(&[7, 3, 1], 0.0), Some(7));
        assert_eq!(focused_card(&[7, 3, 1], 0.9), Some(3));
        assert_eq!(focused_card(&[7, 3, 1], 2.0), Some(1));
        assert_eq!(focused_card(&[7, 3, 1], -4.0), Some(7));
        assert_eq!(focused_card(&[7, 3, 1], 40.0), Some(1));
        assert_eq!(focused_card(&[], 0.0), None);
    }

    fn run(chrome: &mut CardChrome, focused: Option<(ToplevelId, &str)>, steps: usize) {
        for _ in 0..steps {
            chrome.advance(1.0 / 60.0, focused);
        }
    }

    #[test]
    fn chrome_fades_in_with_the_deck_and_out_when_it_leaves() {
        let mut c = CardChrome::new();
        assert_eq!(c.icon_alpha(), 0.0, "nothing drawn before the deck is up");
        c.advance(1.0 / 60.0, Some((1, "Terminal")));
        let first = c.icon_alpha();
        assert!(first > 0.0 && first < 1.0, "ramps in, doesn't pop: {first}");
        run(&mut c, Some((1, "Terminal")), 120);
        assert!(c.icon_alpha() > 0.99);
        c.advance(1.0 / 60.0, None);
        assert!(c.icon_alpha() < 1.0);
        run(&mut c, None, 120);
        assert!(c.icon_alpha() < 0.01);
        assert!(c.title().is_none());
    }

    #[test]
    fn title_cross_fades_when_the_focus_moves() {
        let mut c = CardChrome::new();
        run(&mut c, Some((1, "Terminal")), 120);
        let (id, text, alpha) = c.title().unwrap();
        assert_eq!((id, text), (1, "Terminal"));
        assert!(alpha > 0.99);

        c.advance(1.0 / 60.0, Some((2, "Browser")));
        let (id, text, alpha) = c.title().unwrap();
        assert_eq!((id, text), (1, "Terminal"), "old title fades out first");
        assert!(alpha < 1.0);

        run(&mut c, Some((2, "Browser")), 120);
        let (id, text, alpha) = c.title().unwrap();
        assert_eq!((id, text), (2, "Browser"));
        assert!(alpha > 0.99, "new title fades in to full: {alpha}");
    }

    #[test]
    fn first_title_needs_no_fade_out_first() {
        let mut c = CardChrome::new();
        run(&mut c, Some((1, "Terminal")), 12);
        let (id, _, alpha) = c.title().unwrap();
        assert_eq!(id, 1);
        assert!(alpha > 0.0);
    }

    #[test]
    fn untitled_window_draws_no_title() {
        let mut c = CardChrome::new();
        run(&mut c, Some((1, "")), 60);
        assert!(c.title().is_none());
        assert!(c.icon_alpha() > 0.0, "the badge still shows");
    }

    #[test]
    fn chrome_animates_only_while_a_fade_is_moving() {
        let mut c = CardChrome::new();
        assert!(!c.is_animating(), "idle at rest");
        c.advance(1.0 / 60.0, Some((1, "Terminal")));
        assert!(c.is_animating());
        run(&mut c, Some((1, "Terminal")), 300);
        assert!(!c.is_animating(), "settles so the render loop can idle");
    }

    #[test]
    fn hit_test_picks_topmost() {
        let rects = layout(&[0, 1, 2], 1.0, SIZE, None, CORNER);
        let front = rects.iter().max_by_key(|r| r.z).unwrap();
        match hit_test(&rects, front.center_x, front.center_y, SIZE) {
            CardHit::Card(i) => assert_eq!(rects[i].toplevel, front.toplevel),
            _ => panic!("expected a card hit at the front card center"),
        }
    }

    #[test]
    fn hit_test_empty_off_card() {
        let rects = layout(&[0], 0.0, SIZE, None, CORNER);
        assert!(matches!(hit_test(&rects, 5.0, 5.0, SIZE), CardHit::Empty));
    }

    #[test]
    fn close_lifts_only_that_card() {
        let base = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
        let rects = layout(&[0, 1, 2], 0.0, SIZE, Some((1, 0.5)), CORNER);
        let closing = rects.iter().find(|r| r.toplevel == 1).unwrap();
        let base1 = base.iter().find(|r| r.toplevel == 1).unwrap();
        assert!(closing.center_y < base1.center_y);
        assert!((base1.center_y - closing.center_y - 0.5 * SIZE.1).abs() < 0.001);
        let other = rects.iter().find(|r| r.toplevel == 0).unwrap();
        assert_eq!(other.center_y, SIZE.1 / 2.0);
    }

    #[test]
    fn close_never_scales_the_card() {
        let base = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
        let base1 = base.iter().find(|r| r.toplevel == 1).unwrap();
        for p in [0.25_f32, 0.5, 1.0, -0.08] {
            let rects = layout(&[0, 1, 2], 0.0, SIZE, Some((1, p)), CORNER);
            let c = rects.iter().find(|r| r.toplevel == 1).unwrap();
            assert_eq!(c.scale, base1.scale, "scale changed at progress {p}");
        }
    }

    #[test]
    fn negative_close_pushes_the_card_below_rest() {
        let base = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
        let rects = layout(&[0, 1, 2], 0.0, SIZE, Some((1, -0.08)), CORNER);
        let base1 = base.iter().find(|r| r.toplevel == 1).unwrap();
        let pushed = rects.iter().find(|r| r.toplevel == 1).unwrap();
        assert!(pushed.center_y > base1.center_y);
        assert!((pushed.center_y - base1.center_y - 0.08 * SIZE.1).abs() < 0.001);
    }
}
