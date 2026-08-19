//! Pure switcher-deck geometry: fanned stack of window cards.
//!
//! `cards[0]` is the most-recent (front). `scroll` is a single continuous scalar:
//! 0 = folded (front on the right, older cards tucked behind to the left); increasing
//! first unfolds the stack into a spread, then pans left so earlier cards scroll into
//! view when the spread is wider than the screen.

use crate::ui_state::ToplevelId;
use sc_anim::Spring;

/// Geometry of one card in the switcher deck.
#[derive(Clone, Copy, Debug)]
pub struct CardRect {
    pub toplevel: ToplevelId,
    pub center_x: f32,
    pub center_y: f32,
    pub scale: f32,
    pub corner_radius: f32,
    pub z: usize,
    /// Opacity 0..1. Used to fade the live grab-preview fan in/out; 1.0 for
    /// settled switcher / quick-switch cards.
    pub alpha: f32,
    /// Darkening scrim over the card, 0..1 (0 = untouched). Grows with depth in
    /// the stack so cards behind the front read as receding rather than as
    /// equally-bright siblings that blend into each other.
    pub dim: f32,
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
/// Extra darkening per step back in the stack. Continuous in the (fractional)
/// depth so scrolling ramps a card's dim smoothly as it moves toward the front.
const DIM_PER_STEP: f32 = 0.16;
/// Cap on the depth scrim: past this the deck would read as a black wall.
const DIM_MAX: f32 = 0.55;

/// Darkening scrim for a card `depth` steps behind the front slot. Cards at or
/// in front of the front slot (`depth <= 0`) are undimmed.
fn depth_dim(depth: f32) -> f32 {
    (depth.max(0.0) * DIM_PER_STEP).min(DIM_MAX)
}

/// Compute card rects, back-to-front. `cards[0]` = most-recent.
///
/// `scroll` is a continuous focus index into the deck (carousel): `0` puts
/// `cards[0]` in the front slot with the rest fanned behind to the left; as it
/// grows the whole deck pans right — the focused card slides off the right edge
/// and the next card scales up into the front slot.
///
/// `close` optionally names a toplevel being dragged along the close axis and
/// its signed progress: positive lifts the card upward by `progress * h` until
/// it leaves the screen, negative pushes it below the stack. The card keeps its
/// full size throughout — the close reads as a slide, not a shrink.
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
    let gap_back = w * FOLDED_PEEK_FRAC; // fanned peek behind the front slot
    let slide_off = front_w * SLIDE_OFF_FRAC; // travel per unit once past the front

    let focus = clamp_focus(scroll, n);

    cards
        .iter()
        .enumerate()
        .map(|(i, &toplevel)| {
            // Rest position relative to the focused (front) slot. Every card is
            // the same size as the front card (full height); only x differs.
            let rel = i as f32 - focus;
            let center_x = if rel >= 0.0 {
                front_cx - rel * gap_back // fanned to the left
            } else {
                front_cx + (-rel) * slide_off // passed: sliding off right
            };
            // A card being closed only slides — its size never changes.
            let close_progress = match close {
                Some((t, p)) if t == toplevel => p,
                _ => 0.0,
            };

            // Draw/hit priority: front slot on top, passed cards above it (they
            // slide over the deck), cards behind lowest. Monotonic in -rel.
            let z = ((-rel + 100.0) * 10.0) as usize;

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

/// Live switcher-preview fan for the grab gesture: neighbor cards fanned to the
/// LEFT of a front card that is tracking the finger. `cards[0]` is the front
/// (current) app and is NOT returned — the scene draws it from the finger
/// transform; only the deck behind it (`cards[1..]`) is laid out here.
///
/// `front_cx`/`front_cy`/`scale`/`corner` are the live front card's geometry
/// (finger-driven). Neighbours sit at their full fanned peek positions (same
/// spread as the settled switcher); `alpha` fades the whole deck in/out. All z
/// below the front card.
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
            // Nearer neighbours draw on top of farther ones; all below the front.
            z: 100usize.saturating_sub(i),
            alpha,
            dim: depth_dim(i as f32),
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

/// The card sitting in the front slot at `scroll`, i.e. the one the deck is
/// focused on. `None` for an empty deck.
///
/// Rounded from the same rubber-banded focus the layout uses, so mid-scroll the
/// answer flips exactly when the nearer card takes the front slot.
pub fn focused_card(cards: &[ToplevelId], scroll: f32) -> Option<ToplevelId> {
    let n = cards.len();
    if n == 0 {
        return None;
    }
    let i = clamp_focus(scroll, n).round().clamp(0.0, (n - 1) as f32) as usize;
    cards.get(i).copied()
}

/// Fades for the per-card chrome drawn around the deck: the app-icon badges and
/// the focused card's window title.
///
/// Two separate fades, because they answer different questions. `visible` is
/// "is the deck up" — it ramps the whole chrome in as the switcher opens and
/// out as it leaves, so badges don't pop into existence over a rising deck.
/// `title_alpha` is "has the focus moved" — switching cards fades the old title
/// out and the new one in, rather than swapping the text under a steady alpha,
/// which would read as a glitch.
///
/// The title text is owned here (not re-read per frame) precisely so the
/// outgoing title survives its fade-out after the focus has already moved on.
pub struct CardChrome {
    visible: Spring,
    title_alpha: Spring,
    shown: Option<ToplevelId>,
    text: String,
}

/// Below this alpha a title counts as gone: the incoming one can take over
/// without a visible cut, and the outgoing one stops being drawn rather than
/// lingering at the fraction of a percent a spring settles to.
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

    /// Advance both fades by `dt`. `focused` is the front card and its title,
    /// `None` whenever the deck isn't on screen.
    pub fn advance(&mut self, dt: f32, focused: Option<(ToplevelId, &str)>) {
        self.visible
            .retarget(if focused.is_some() { 1.0 } else { 0.0 });
        self.visible.step(dt);

        match focused {
            // Same card still focused: hold (or finish fading in) its title.
            Some((id, _)) if self.shown == Some(id) => self.title_alpha.retarget(1.0),
            Some((id, title)) => {
                // Nothing on screen to cross-fade from — adopt at once, so the
                // first title of a freshly-opened deck fades in with the badges
                // instead of waiting out an empty fade-out first.
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

    /// Opacity for the icon badges: the deck-visibility fade alone.
    pub fn icon_alpha(&self) -> f32 {
        self.visible.value.clamp(0.0, 1.0)
    }

    /// The title to draw and its opacity, plus the card it belongs to. `None`
    /// once it has faded out entirely (or was never shown).
    pub fn title(&self) -> Option<(ToplevelId, &str, f32)> {
        let alpha = self.icon_alpha() * self.title_alpha.value.clamp(0.0, 1.0);
        let id = self.shown?;
        (alpha > TITLE_SWAP_ALPHA && !self.text.is_empty()).then_some((
            id,
            self.text.as_str(),
            alpha,
        ))
    }

    /// True while either fade is still moving, so the render loop keeps frames
    /// coming until the chrome has settled.
    pub fn is_animating(&self) -> bool {
        !self.visible.is_settled() || !self.title_alpha.is_settled()
    }
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
        let rects = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
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
        let s0 = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER);
        let s1 = layout(&[0, 1, 2], 1.0, SIZE, None, CORNER);
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
        // A card one step back at scroll 0 is half-way to undimmed at scroll 0.5.
        let a = layout(&[0, 1, 2], 0.0, SIZE, None, CORNER)[1].dim;
        let b = layout(&[0, 1, 2], 0.5, SIZE, None, CORNER)[1].dim;
        assert!(b < a && b > 0.0, "dim {a} -> {b} should ease, not step");
        // Once it reaches the front slot it is fully clear.
        assert_eq!(layout(&[0, 1, 2], 1.0, SIZE, None, CORNER)[1].dim, 0.0);
    }

    #[test]
    fn passed_cards_are_never_dimmed() {
        // cards[0] has slid off to the right; it is in front of the deck.
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
        // Rubber-banded past either end, still a real card.
        assert_eq!(focused_card(&[7, 3, 1], -4.0), Some(7));
        assert_eq!(focused_card(&[7, 3, 1], 40.0), Some(1));
        assert_eq!(focused_card(&[], 0.0), None);
    }

    /// Run `chrome` for `steps` frames at 60 Hz on one focus.
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
        // Deck gone: fade back out rather than cut.
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

        // The old title fades out first: the new one must not appear yet.
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
        // Nothing was on screen, so the incoming title starts rising at once.
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
        // Lifted upward (smaller y = higher) by exactly progress * height.
        assert!(closing.center_y < base1.center_y);
        assert!((base1.center_y - closing.center_y - 0.5 * SIZE.1).abs() < 0.001);
        // Other cards untouched.
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
