/// Normalized point: x in [0,1] of screen width, y in [0,1] of screen height
/// with y=0 at the top. Keeps the logic resolution-independent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

/// Tracks a single touch and produces a low-passed velocity (units: fraction/sec).
#[derive(Clone, Copy, Debug)]
pub struct Tracker {
    pub start: Pt,
    pub current: Pt,
    pub velocity: Pt,
}

impl Tracker {
    pub fn begin(p: Pt) -> Self {
        Self {
            start: p,
            current: p,
            velocity: Pt { x: 0.0, y: 0.0 },
        }
    }

    pub fn update(&mut self, p: Pt, dt: f32) {
        if dt > 0.0 {
            let inst = Pt {
                x: (p.x - self.current.x) / dt,
                y: (p.y - self.current.y) / dt,
            };
            let a = crate::thresholds::VELOCITY_SMOOTHING;
            self.velocity.x = a * inst.x + (1.0 - a) * self.velocity.x;
            self.velocity.y = a * inst.y + (1.0 - a) * self.velocity.y;
        }
        self.current = p;
    }

    /// Decay velocity toward zero. Motion events stop arriving while a finger is
    /// held still, so the frame loop calls this each tick during a grab; without
    /// it a "drag up and hold" keeps its stale upward velocity and every release
    /// reads as a fast flick. Frame-rate independent (~halves every ~35ms).
    pub fn decay(&mut self, dt: f32) {
        let factor = (-dt / 0.05).exp();
        self.velocity.x *= factor;
        self.velocity.y *= factor;
    }

    /// Upward progress: how far up from the start (0 at start, 1 = full screen up).
    pub fn up_progress(&self) -> f32 {
        (self.start.y - self.current.y).max(0.0)
    }
    /// Signed horizontal travel from start.
    pub fn dx(&self) -> f32 {
        self.current.x - self.start.x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_upward_progress() {
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.95 });
        t.update(Pt { x: 0.5, y: 0.45 }, 1.0 / 90.0);
        assert!((t.up_progress() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn decay_kills_velocity_on_hold() {
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.9 });
        t.update(Pt { x: 0.5, y: 0.5 }, 1.0 / 90.0); // fast upward drag
        let flick = t.velocity.y;
        assert!(flick < 0.0);
        // Hold still for ~150ms worth of frames.
        for _ in 0..14 {
            t.decay(1.0 / 90.0);
        }
        assert!(
            t.velocity.y.abs() < flick.abs() * 0.1,
            "velocity should decay during a hold"
        );
    }

    #[test]
    fn velocity_is_low_passed_not_instantaneous() {
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.9 });
        let dt = 1.0 / 90.0;
        let raw = (0.5 - 0.9) / dt;
        t.update(Pt { x: 0.5, y: 0.5 }, dt);
        assert!(t.velocity.y.abs() < raw.abs(), "should be smoothed");
        assert!(t.velocity.y < 0.0, "upward = negative");
    }
}
