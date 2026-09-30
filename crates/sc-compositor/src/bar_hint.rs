//! Home-bar pill visibility. In an app it blinks once on arrival, then fades
//! out; touching the bar zone brings it back briefly, and it stays lit during
//! a drag. Never drawn on Home. Only the drawn alpha changes; the gesture zone
//! is always live.

use std::time::Instant;

const BLINK_HOLD: f32 = 0.30;
const BLINK_DIP: f32 = 0.15;
/// Not to zero: a pill that vanishes reads as a glitch.
const BLINK_DIP_ALPHA: f32 = 0.15;
const BLINK_SETTLE: f32 = 0.45;
const BLINK_FADE: f32 = 0.50;

const REVEAL_IN: f32 = 0.12;
const REVEAL_HOLD: f32 = 1.00;
const REVEAL_OUT: f32 = 0.45;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarMode {
    Off,
    Shown,
    Auto,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Steady,
    Blink(Instant),
    Hidden,
    Reveal(Instant),
}

#[derive(Clone, Copy, Debug)]
pub struct BarHint {
    phase: Phase,
    mode: BarMode,
}

impl Default for BarHint {
    fn default() -> Self {
        BarHint {
            phase: Phase::Hidden,
            mode: BarMode::Off,
        }
    }
}

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

    /// Idempotent: only a change restarts a sequence.
    pub fn set_mode(&mut self, mode: BarMode, now: Instant) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        self.phase = match mode {
            BarMode::Off => Phase::Hidden,
            BarMode::Shown => Phase::Steady,
            BarMode::Auto => Phase::Blink(now),
        };
    }

    /// A no-op on Home.
    pub fn touched(&mut self, now: Instant) {
        if self.mode == BarMode::Auto {
            self.phase = Phase::Reveal(now);
        }
    }

    pub fn alpha(&self, now: Instant) -> f32 {
        match self.phase {
            Phase::Steady => 1.0,
            Phase::Hidden => 0.0,
            Phase::Blink(start) => Self::blink_alpha(secs_since(start, now)),
            Phase::Reveal(start) => Self::reveal_alpha(secs_since(start, now)),
        }
    }

    /// Called once a frame.
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
    fn on_home_the_bar_is_never_drawn() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_mode(BarMode::Off, t0);
        assert_eq!(hint.alpha(t0), 0.0);
        hint.touched(t0);
        assert_eq!(hint.alpha(at(t0, REVEAL_IN)), 0.0);
        assert!(!hint.is_animating(t0));
    }

    #[test]
    fn during_a_drag_the_bar_is_simply_drawn() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_mode(BarMode::Shown, t0);
        assert_eq!(hint.alpha(at(t0, 10.0)), 1.0);
        assert!(!hint.is_animating(t0));
    }

    #[test]
    fn arriving_in_an_app_blinks_then_fades_out() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_mode(BarMode::Auto, t0);

        assert_eq!(hint.alpha(t0), 1.0);
        let dip = hint.alpha(at(t0, BLINK_HOLD + BLINK_DIP));
        assert!(dip < 0.5, "expected a dip, got {dip}");
        assert_eq!(hint.alpha(at(t0, BLINK_HOLD + 2.0 * BLINK_DIP + 0.01)), 1.0);
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
        hint.set_mode(BarMode::Auto, t0);
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
    fn a_drag_ending_back_in_the_app_blinks_again() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_mode(BarMode::Auto, t0);
        let hidden = at(t0, BarHint::BLINK_TOTAL + 1.0);
        hint.advance(hidden);
        assert_eq!(hint.alpha(hidden), 0.0);

        hint.set_mode(BarMode::Shown, hidden);
        assert_eq!(hint.alpha(hidden), 1.0);
        hint.set_mode(BarMode::Auto, hidden);
        assert_eq!(hint.alpha(hidden), 1.0);
        assert!(hint.alpha(at(hidden, BarHint::BLINK_TOTAL)) < 0.001);
    }

    #[test]
    fn the_same_mode_does_not_restart_the_blink() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_mode(BarMode::Auto, t0);
        // A commit every frame must not hold the pill on screen forever.
        for ms in [10, 200, 800, 1600] {
            hint.set_mode(BarMode::Auto, at(t0, ms as f32 / 1000.0));
        }
        let late = at(t0, BarHint::BLINK_TOTAL + 0.5);
        hint.advance(late);
        assert_eq!(hint.alpha(late), 0.0);
    }

    #[test]
    fn a_touch_mid_blink_takes_over_from_it() {
        let mut hint = BarHint::new();
        let t0 = Instant::now();
        hint.set_mode(BarMode::Auto, t0);
        // Grabbing mid-blink must keep it lit.
        let mid = at(t0, BLINK_HOLD + BLINK_DIP);
        hint.touched(mid);
        assert_eq!(hint.alpha(at(mid, REVEAL_IN + 0.1)), 1.0);
    }
}
