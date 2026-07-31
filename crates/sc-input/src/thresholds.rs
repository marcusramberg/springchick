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
