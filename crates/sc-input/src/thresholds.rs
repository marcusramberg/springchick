//! Navigation feel constants. Distances are fractions of screen height/width.

/// Release below this upward progress returns to the app.
pub const BACK_TO_APP_MAX_PROGRESS: f32 = 0.10;
/// Switcher fan starts fading in at this progress, just past the dock.
pub const SWITCHER_REVEAL_PROGRESS: f32 = 0.12;
/// At/above this progress a release goes home; between SWITCHER_REVEAL and
/// here it settles in the switcher.
pub const HOME_MIN_PROGRESS: f32 = 0.35;
/// Upward velocity (screens/s, negative = up) above which a release goes home
/// regardless of distance.
///
/// Compared against the low-passed velocity, which under-reports short flicks
/// badly (a real 1.7/s flick reads ~1.0), so this sits near the reported figure.
pub const HOME_FLICK_VELOCITY: f32 = -0.9;
pub const QUICK_SWITCH_PROGRESS: f32 = 0.15;
pub const QUICK_SWITCH_VELOCITY: f32 = 1.5;
/// Velocity low-pass factor (0..1, higher = snappier/noisier).
pub const VELOCITY_SMOOTHING: f32 = 0.6;

// Home screen / switcher deck

/// Travel from an icon press that still counts as a tap. Pixels, not a
/// fraction: finger jitter doesn't scale with screen size.
pub const ICON_TAP_SLOP_PX: f32 = 12.0;

/// Travel allowed while waiting out a long press.
pub const ICON_HOLD_SLOP_PX: f32 = 32.0;
pub const SWITCHER_TAP_SLOP_PX: f32 = 15.0;

/// Travel (fraction of width) for a slow page drag to commit.
pub const PAGE_COMMIT_FRAC: f32 = 0.3;
/// Page-drag speed that commits regardless of distance. Low-passed velocity,
/// see [`HOME_FLICK_VELOCITY`].
pub const PAGE_FLICK_VELOCITY: f32 = 0.7;
/// Minimum travel before speed alone can commit a page flick.
pub const PAGE_FLICK_MIN_FRAC: f32 = 0.04;
/// Fraction of finger travel followed past an end stop. Shared by the page
/// strip and quick-switch stack so both feel the same.
pub const RUBBER_BAND_FOLLOW: f32 = 0.3;
/// Below this width fraction an arrange-mode empty-area release is a tap
/// (exit arrange), not a page swipe.
pub const ARRANGE_PAGE_SWIPE_FRAC: f32 = 0.15;

/// Downward travel on empty Home that opens search; must be mostly vertical.
pub const PULL_DOWN_SEARCH_FRAC: f32 = 0.08;

/// Upward travel on the Home bar that opens the switcher.
pub const BAR_RAISE_FRAC: f32 = 0.08;
/// Rightward travel on the Home bar that slides onto the top card.
pub const BAR_SWITCH_FRAC: f32 = 0.15;
/// Quick-switch slide fraction needed at release to commit.
pub const QUICK_SWITCH_COMMIT_FRAC: f32 = 0.2;

/// Upward travel for a slow card drag to close the card. Tracks the finger 1:1.
pub const CARD_CLOSE_COMMIT: f32 = 0.18;
/// Card-close flick speed. Same as [`HOME_FLICK_VELOCITY`] on purpose: it is
/// the same hand motion.
pub const CARD_CLOSE_FLICK_VELOCITY: f32 = -HOME_FLICK_VELOCITY;
pub const CARD_CLOSE_FLICK_MIN_FRAC: f32 = 0.02;
/// Scale of a downward card drag into negative close progress (resistance).
pub const CARD_PUSH_DOWN_RUBBER: f32 = 0.35;
pub const CARD_PUSH_DOWN_MAX: f32 = 0.08;
/// Finger travel (fraction of width) that scrolls the switcher one card.
pub const CARD_SCROLL_PER_INDEX_FRAC: f32 = 0.42;
