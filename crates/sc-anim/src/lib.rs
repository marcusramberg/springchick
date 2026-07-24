#![forbid(unsafe_code)]

/// A critically-damped-by-default spring driving one scalar.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub stiffness: f32, // higher = snappier
    pub damping: f32,   // critical damping ~= 2*sqrt(stiffness)
}

impl Spring {
    pub fn new(value: f32) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
            stiffness: 220.0,
            damping: 30.0,
        }
    }

    /// Retarget without losing current value/velocity (interruptible).
    pub fn retarget(&mut self, target: f32) {
        self.target = target;
    }

    /// Advance by dt seconds (semi-implicit Euler). Returns true while still moving.
    pub fn step(&mut self, dt: f32) -> bool {
        let force = -self.stiffness * (self.value - self.target) - self.damping * self.velocity;
        self.velocity += force * dt;
        self.value += self.velocity * dt;
        !self.is_settled()
    }

    pub fn is_settled(&self) -> bool {
        (self.value - self.target).abs() < 0.001 && self.velocity.abs() < 0.001
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_to_rest(s: &mut Spring, max_steps: usize) -> usize {
        let dt = 1.0 / 90.0;
        for i in 0..max_steps {
            if !s.step(dt) {
                return i;
            }
        }
        max_steps
    }

    #[test]
    fn converges_to_target() {
        let mut s = Spring::new(0.0);
        s.retarget(100.0);
        let steps = run_to_rest(&mut s, 1000);
        assert!(steps < 1000, "spring should settle");
        assert!((s.value - 100.0).abs() < 0.01, "value={}", s.value);
    }

    #[test]
    fn no_large_overshoot() {
        let mut s = Spring::new(0.0);
        s.retarget(100.0);
        let dt = 1.0 / 90.0;
        let mut peak = 0.0_f32;
        for _ in 0..1000 {
            s.step(dt);
            peak = peak.max(s.value);
            if s.is_settled() {
                break;
            }
        }
        assert!(peak <= 100.0 * 1.05, "overshoot too large: peak={}", peak);
    }

    #[test]
    fn retarget_preserves_velocity() {
        let mut s = Spring::new(0.0);
        s.retarget(100.0);
        let dt = 1.0 / 90.0;
        for _ in 0..5 {
            s.step(dt);
        }
        let v = s.velocity;
        s.retarget(50.0); // interrupt
        assert_eq!(s.velocity, v, "retarget must not zero velocity");
    }
}
