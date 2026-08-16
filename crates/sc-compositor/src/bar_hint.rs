//! Home-bar visibility while an app is fullscreen.
//!
//! Outside fullscreen the pill is simply always there. Fullscreen is the case
//! where a permanent bar is wrong — it sits over video, over a game, over
//! whatever the user went immersive for — but hiding it outright leaves no
//! clue that the gesture exists at all. So on entering fullscreen the pill
//! blinks once to say "here", then fades away; touching the bar zone brings it
//! back for a moment.
//!
//! Only the *drawn* alpha changes. The gesture zone is untouched, so a hidden
//! pill still swipes exactly like a visible one — which is what makes fading it
//! out safe.
//!
//! Clock-injected and pure: the caller supplies `Instant`s, so every timing rule
//! here is unit-testable without sleeping.

use std::time::Instant;

/// Entering fullscreen: hold, blink down and back, hold, then fade away.
const BLINK_HOLD: f32 = 0.30;
const BLINK_DIP: f32 = 0.15;
/// How dark the blink's dip goes. Not to zero: a pill that vanishes completely
/// reads as a glitch rather than as a wink.
const BLINK_DIP_ALPHA: f32 = 0.15;
const BLINK_SETTLE: f32 = 0.45;
const BLINK_FADE: f32 = 0.50;

/// Touched while hidden: appear quickly, stay for a beat, fade back out.
const REVEAL_IN: f32 = 0.12;
const REVEAL_HOLD: f32 = 1.00;
const REVEAL_OUT: f32 = 0.45;

/// What the bar is doing right now.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Not fullscreen: always drawn.
    Steady,
    /// Just went fullscreen: the blink-then-fade sequence, started at `Instant`.
    Blink(Instant),
    /// Fullscreen and out of the way.
    Hidden,
    /// Fullscreen, but the bar zone was touched: shown again from `Instant`.
    Reveal(Instant),
}

/// Drawn-alpha policy for the home pill.
#[derive(Clone, Copy, Debug)]
pub struct BarHint {
    phase: Phase,
    fullscreen: bool,
}

impl Default for BarHint {
    fn default() -> Self {
        BarHint {
            phase: Phase::Steady,
            fullscreen: false,
        }
    }
}

/// Linear ramp from `from` to `to` across `dur` seconds of `t`.
fn ramp(t: f32, dur: f32, from: f32, to: f32) -> f32 {
    if dur <= 0.0 {
        return to;
    }
    from + (to - from) * (t / dur).clamp(0.0, 1.0)
}

impl BarHint {
    pub fn new() -> Self {
        BarHint::default()
    }

    /// The foreground app's fullscreen state, as committed by the client.
    /// Idempotent: only a change starts (or ends) the sequence, so the
    /// per-commit caller can hand it the same value every frame.
    pub fn set_fullscreen(&mut self, fullscreen: bool, now: Instant) {
        if fullscreen == self.fullscreen {
            return;
        }
        self.fullscreen = fullscreen;
        self.phase = if fullscreen {
            Phase::Blink(now)
        } else {
            Phase::Steady
        };
    }

    /// A finger landed in the bar's gesture zone. Brings a hidden pill back so
    /// the user can see what they are dragging; a no-op outside fullscreen,
    /// where it never went away.
    pub fn touched(&mut self, now: Instant) {
        if self.fullscreen {
            self.phase = Phase::Reveal(now);
        }
    }

    /// Drawn alpha for the pill, 0..1.
    pub fn alpha(&self, now: Instant) -> f32 {
        match self.phase {
            Phase::Steady => 1.0,
            Phase::Hidden => 0.0,
            Phase::Blink(start) => Self::blink_alpha(secs_since(start, now)),
            Phase::Reveal(start) => Self::reveal_alpha(secs_since(start, now)),
        }
    }

    /// Retire a finished sequence so `alpha` stops doing arithmetic and the
    /// render loop stops being told there is an animation. Called once a frame.
    pub fn advance(&mut self, now: Instant) {
        let done = match self.phase {
            Phase::Blink(start) => secs_since(start, now) >= Self::BLINK_TOTAL,
            Phase::Reveal(start) => secs_since(start, now) >= Self::REVEAL_TOTAL,
            _ => false,
        };
        if done {
            self.phase = Phase::Hidden;
        }
    }

    /// Whether the alpha is still changing, so the frame loop keeps drawing.
    pub fn is_animating(&self, now: Instant) -> bool {
        match self.phase {
            Phase::Steady | Phase::Hidden => false,
            Phase::Blink(start) => secs_since(start, now) < Self::BLINK_TOTAL,
            Phase::Reveal(start) => secs_since(start, now) < Self::REVEAL_TOTAL,
        }
    }

    const BLINK_TOTAL: f32 = BLINK_HOLD + BLINK_DIP + BLINK_DIP + BLINK_SETTLE + BLINK_FADE;
    const REVEAL_TOTAL: f32 = REVEAL_IN + REVEAL_HOLD + REVEAL_OUT;

    /// on → dip → back on → hold → out.
    fn blink_alpha(t: f32) -> f32 {
        let mut edge = BLINK_HOLD;
        if t < edge {
            return 1.0;
        }
        if t < edge + BLINK_DIP {
            return ramp(t - edge, BLINK_DIP, 1.0, BLINK_DIP_ALPHA);
        }
        edge += BLINK_DIP;
        if t < edge + BLINK_DIP {
            return ramp(t - edge, BLINK_DIP, BLINK_DIP_ALPHA, 1.0);
        }
        edge += BLINK_DIP;
        if t < edge + BLINK_SETTLE {
            return 1.0;
        }
        edge += BLINK_SETTLE;
        ramp(t - edge, BLINK_FADE, 1.0, 0.0)
    }

    /// in → hold → out.
    fn reveal_alpha(t: f32) -> f32 {
        if t < REVEAL_IN {
            return ramp(t, REVEAL_IN, 0.0, 1.0);
        }
        if t < REVEAL_IN + REVEAL_HOLD {
            return 1.0;
        }
        ramp(t - REVEAL_IN - REVEAL_HOLD, REVEAL_OUT, 1.0, 0.0)
    }
}

fn secs_since(start: Instant, now: Instant) -> f32 {
    now.saturating_duration_since(start).as_secs_f32()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, s: f32) -> Instant {
        base + std::time::Duration::from_secs_f32(s)
    }

    #[test]
    fn outside_fullscreen_the_bar_is_simply_drawn() {
        let hint = BarHint::new();
        let t0 = Instant::now();
        assert_eq!(hint.alpha(t0), 1.0);
        assert!(!hint.is_animating(t0));
    }

    #[test]
    fn entering_fullscreen_blinks_then_fades_out() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_fullscreen(true, t0);

        // Visible at first, so the eye catches it where it already was.
        assert_eq!(hint.alpha(t0), 1.0);
        // Dips mid-blink...
        let dip = hint.alpha(at(t0, BLINK_HOLD + BLINK_DIP));
        assert!(dip < 0.5, "expected a dip, got {dip}");
        // ...comes back...
        assert_eq!(hint.alpha(at(t0, BLINK_HOLD + 2.0 * BLINK_DIP + 0.01)), 1.0);
        // ...then fades away and stays away.
        assert!(hint.alpha(at(t0, BarHint::BLINK_TOTAL)) < 0.001);
        let late = at(t0, BarHint::BLINK_TOTAL + 5.0);
        hint.advance(late);
        assert_eq!(hint.alpha(late), 0.0);
        assert!(!hint.is_animating(late));
    }

    #[test]
    fn touching_the_bar_brings_it_back_then_hides_it_again() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_fullscreen(true, t0);
        let settled = at(t0, BarHint::BLINK_TOTAL + 1.0);
        hint.advance(settled);
        assert_eq!(hint.alpha(settled), 0.0);

        hint.touched(settled);
        assert!(hint.alpha(at(settled, REVEAL_IN)) > 0.9, "fades in");
        assert_eq!(hint.alpha(at(settled, REVEAL_IN + REVEAL_HOLD / 2.0)), 1.0);
        assert!(hint.alpha(at(settled, BarHint::REVEAL_TOTAL)) < 0.001);

        let done = at(settled, BarHint::REVEAL_TOTAL + 0.1);
        hint.advance(done);
        assert!(!hint.is_animating(done));
    }

    #[test]
    fn leaving_fullscreen_restores_a_permanent_bar() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_fullscreen(true, t0);
        let hidden = at(t0, BarHint::BLINK_TOTAL + 1.0);
        hint.advance(hidden);
        assert_eq!(hint.alpha(hidden), 0.0);

        hint.set_fullscreen(false, hidden);
        assert_eq!(hint.alpha(hidden), 1.0);
        assert!(!hint.is_animating(hidden));
    }

    #[test]
    fn the_same_fullscreen_value_does_not_restart_the_blink() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_fullscreen(true, t0);
        // A commit every frame must not hold the pill on screen forever.
        for ms in [10, 200, 800, 1600] {
            hint.set_fullscreen(true, at(t0, ms as f32 / 1000.0));
        }
        let late = at(t0, BarHint::BLINK_TOTAL + 0.5);
        hint.advance(late);
        assert_eq!(hint.alpha(late), 0.0);
    }

    #[test]
    fn touching_outside_fullscreen_changes_nothing() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.touched(t0);
        assert_eq!(hint.alpha(t0), 1.0);
        assert!(!hint.is_animating(t0));
    }

    #[test]
    fn a_touch_mid_blink_takes_over_from_it() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_fullscreen(true, t0);
        // Grabbing the bar while it is still blinking must leave it lit, not
        // let the blink's own fade run out from under the finger.
        let mid = at(t0, BLINK_HOLD + BLINK_DIP);
        hint.touched(mid);
        assert_eq!(hint.alpha(at(mid, REVEAL_IN + 0.1)), 1.0);
    }
}
