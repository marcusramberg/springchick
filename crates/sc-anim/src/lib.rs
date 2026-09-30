#![forbid(unsafe_code)]

#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub stiffness: f32,
    pub damping: f32,
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

    /// Shared tuning for icon-zoom and card open/close.
    pub fn zoom(from: f32, to: f32) -> Self {
        let mut s = Spring::new(from);
        s.stiffness = 300.0;
        s.damping = 35.0;
        s.retarget(to);
        s
    }

    pub fn retarget(&mut self, target: f32) {
        self.target = target;
    }

    /// Semi-implicit Euler. Returns true while still moving.
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

pub fn lerp_point(from: (f32, f32), to: (f32, f32), t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
}

pub fn ease_out_cubic(p: f32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    1.0 - (1.0 - p).powi(3)
}

/// Remaining offset of a surface sliding in from an edge at progress `p`.
pub fn slide_in_offset(p: f32, distance: f32) -> f32 {
    (1.0 - ease_out_cubic(p)) * distance
}

/// `sin(elapsed*rate)*0.5 + 0.5`, for the launching-icon halo.
pub fn pulse(elapsed: f32, rate: f32) -> f32 {
    (elapsed * rate).sin() * 0.5 + 0.5
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
        s.retarget(50.0);
        assert_eq!(s.velocity, v, "retarget must not zero velocity");
    }

    #[test]
    fn lerp_point_endpoints_midpoint_and_clamp() {
        assert_eq!(lerp_point((0.0, 0.0), (10.0, 20.0), 0.0), (0.0, 0.0));
        assert_eq!(lerp_point((0.0, 0.0), (10.0, 20.0), 1.0), (10.0, 20.0));
        assert_eq!(lerp_point((0.0, 0.0), (10.0, 20.0), 0.5), (5.0, 10.0));
        assert_eq!(lerp_point((0.0, 0.0), (10.0, 0.0), -1.0), (0.0, 0.0));
        assert_eq!(lerp_point((0.0, 0.0), (10.0, 0.0), 2.0), (10.0, 0.0));
    }

    #[test]
    fn ease_out_cubic_endpoints_and_clamp() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.5);
    }

    #[test]
    fn slide_in_offset_runs_full_distance_to_zero() {
        assert_eq!(slide_in_offset(0.0, 800.0), 800.0);
        assert_eq!(slide_in_offset(1.0, 800.0), 0.0);
        assert!(slide_in_offset(0.5, 800.0) < 400.0);
        let mut prev = f32::INFINITY;
        for i in 0..=20 {
            let v = slide_in_offset(i as f32 / 20.0, 800.0);
            assert!(v < prev);
            prev = v;
        }
    }

    #[test]
    fn pulse_stays_in_unit_range_and_starts_mid() {
        assert!((pulse(0.0, 4.4) - 0.5).abs() < 1e-6);
        for i in 0..100 {
            let v = pulse(i as f32 * 0.1, 4.4);
            assert!((0.0..=1.0).contains(&v), "pulse out of range: {v}");
        }
    }
}
