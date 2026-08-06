//! All navigation feel constants. Tuned on-harness in Milestone 3.
//! Distances are fractions of screen height/width (resolution-independent).

/// Release below this upward progress → return to the app. Kept small so it's
/// easy to clear and land in the fan stack.
pub const BACK_TO_APP_MAX_PROGRESS: f32 = 0.10;
/// Switcher card deck begins fading in at/above this progress (live preview).
/// Low — just past the bottom dock — so neighbours appear as soon as the drag
/// clears the dock, not several icon rows up.
pub const SWITCHER_REVEAL_PROGRESS: f32 = 0.12;
/// Band B/C boundary: at/above this progress the live fan has faded out and a
/// release goes home. Below it (and above SWITCHER_REVEAL) the fan is live and a
/// release settles in the switcher stack.
pub const HOME_MIN_PROGRESS: f32 = 0.35;
/// Upward velocity (fraction of screen height per second) above which a flick
/// always flings home regardless of distance. Negative = upward. Only decisive
/// flicks should go home, so the fan stack stays easy to reach.
pub const HOME_FLICK_VELOCITY: f32 = -2.6;
/// Horizontal travel fraction (of screen width) that commits a quick-switch.
pub const QUICK_SWITCH_PROGRESS: f32 = 0.15;
/// Horizontal velocity (fraction of screen width/s) that commits a quick-switch.
pub const QUICK_SWITCH_VELOCITY: f32 = 1.5;
/// Velocity low-pass smoothing factor (0..1, higher = snappier/noisier).
pub const VELOCITY_SMOOTHING: f32 = 0.6;

// --- Home screen / switcher deck (see `crate::home`) ---

/// Pixels a finger may travel from an icon press and still count as a tap
/// (launch). Past this the press is cancelled and the gesture becomes a swipe.
/// In pixels, not a fraction: it models finger jitter, which does not scale
/// with screen size.
pub const ICON_TAP_SLOP_PX: f32 = 12.0;
/// Same idea for a press on the switcher deck: below this drift the release is
/// a tap (open the card / dismiss), above it a scroll.
pub const SWITCHER_TAP_SLOP_PX: f32 = 15.0;

/// Fraction of a page's width the finger must travel for a released page drag
/// to commit to the neighbouring page.
pub const PAGE_COMMIT_FRAC: f32 = 0.3;
/// How much of the finger's travel is still followed once a drag is past an end
/// stop — the rubber-band feel. Shared by the ends of the page strip and the
/// ends of the quick-switch app stack, deliberately: they should give way by the
/// same amount, or the two gestures feel like different surfaces.
pub const RUBBER_BAND_FOLLOW: f32 = 0.3;
/// Extra gate on an arrange-mode empty-area release: below this fraction of the
/// width the gesture is a still tap (exit arrange) rather than a page swipe.
pub const ARRANGE_PAGE_SWIPE_FRAC: f32 = 0.15;

/// Downward travel (fraction of screen height) on empty Home space that opens
/// the pull-down search. The drag must also be more vertical than horizontal.
pub const PULL_DOWN_SEARCH_FRAC: f32 = 0.08;

/// Upward travel (fraction of height) on the Home bar that opens the switcher.
pub const BAR_RAISE_FRAC: f32 = 0.08;
/// Rightward travel (fraction of width) on the Home bar that slides onto the top
/// card of the stack.
pub const BAR_SWITCH_FRAC: f32 = 0.15;
/// Fraction of screen width a live quick-switch slide must reach at release to
/// commit to the neighbour app; short of it the swipe springs back. Kept low so
/// a short, deliberate swipe commits instead of bouncing.
pub const QUICK_SWITCH_COMMIT_FRAC: f32 = 0.2;

/// Upward travel (fraction of screen height) that drives a switcher card's
/// close progress from 0 to 1.
pub const CARD_CLOSE_FULL_RISE: f32 = 0.25;
/// Close progress at/above which a released card is actually closed.
pub const CARD_CLOSE_COMMIT: f32 = 0.4;
/// Finger travel that advances the switcher carousel one card, as a fraction of
/// output width (≈ front card width × slide distance).
pub const CARD_SCROLL_PER_INDEX_FRAC: f32 = 0.42;
