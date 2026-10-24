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

    /// Solved in closed form (unit mass), so the curve is the same at any frame
    /// rate or with dropped frames. Returns true while still moving.
    pub fn step(&mut self, dt: f32) -> bool {
        let (d, v) = evaluate(
            self.stiffness,
            self.damping,
            self.value - self.target,
            self.velocity,
            dt,
        );
        self.value = self.target + d;
        self.velocity = v;
        !self.is_settled()
    }

    pub fn is_settled(&self) -> bool {
        (self.value - self.target).abs() < 0.001 && self.velocity.abs() < 0.001
    }
}

/// Displacement `d0` and velocity `v0` of `x'' = -k x - c x'` after `t` seconds.
fn evaluate(k: f32, c: f32, d0: f32, v0: f32, t: f32) -> (f32, f32) {
    let w0 = k.max(0.0).sqrt();
    if w0 == 0.0 {
        return (d0, v0);
    }
    let zeta = c / (2.0 * w0);
    if (zeta - 1.0).abs() < 1e-4 {
        let b = v0 + w0 * d0;
        let e = (-w0 * t).exp();
        let d = e * (d0 + b * t);
        (d, e * b - w0 * d)
    } else if zeta < 1.0 {
        let a = zeta * w0;
        let wd = w0 * (1.0 - zeta * zeta).sqrt();
        let b = (v0 + a * d0) / wd;
        let e = (-a * t).exp();
        let (sin, cos) = (wd * t).sin_cos();
        (
            e * (d0 * cos + b * sin),
            e * (v0 * cos - (a * b + d0 * wd) * sin),
        )
    } else {
        let root = (zeta * zeta - 1.0).sqrt();
        let r1 = -w0 * (zeta - root);
        let r2 = -w0 * (zeta + root);
        let c2 = (v0 - r1 * d0) / (r2 - r1);
        let c1 = d0 - c2;
        let (e1, e2) = ((r1 * t).exp(), (r2 * t).exp());
        (c1 * e1 + c2 * e2, r1 * c1 * e1 + r2 * c2 * e2)
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
    fn frame_rate_does_not_change_the_curve() {
        for (k, c) in [(220.0, 30.0), (320.0, 26.0), (2000.0, 90.0), (400.0, 40.0)] {
            let run = |dt: f32, n: usize| {
                let mut s = Spring::new(0.0);
                s.stiffness = k;
                s.damping = c;
                s.retarget(1.0);
                for _ in 0..n {
                    s.step(dt);
                }
                (s.value, s.velocity)
            };
            let (a, b, one) = (run(1.0 / 120.0, 24), run(1.0 / 30.0, 6), run(0.2, 1));
            assert!(
                (a.0 - b.0).abs() < 1e-4 && (a.0 - one.0).abs() < 1e-4,
                "k={k}: {a:?} {b:?} {one:?}"
            );
            assert!((a.1 - b.1).abs() < 1e-3, "k={k}: {a:?} {b:?}");
        }
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
}
