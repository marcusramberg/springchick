/// Normalized point, 0..1 of screen size, y=0 at the top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

/// Recent samples, at up to ~160 Hz over [`VELOCITY_WINDOW`]; older ones drop.
const SAMPLES: usize = 16;
const VELOCITY_WINDOW: f32 = 0.1;
/// Touchscreens send nothing while a finger holds still, so a gap this long
/// means it stopped at `current`.
const HOLD_GAP: f32 = 0.03;

/// Tracks one touch. Velocity, in fractions/sec, is a least-squares fit over
/// the last [`VELOCITY_WINDOW`] seconds, recomputed on every update.
#[derive(Clone, Copy, Debug)]
pub struct Tracker {
    pub start: Pt,
    pub current: Pt,
    pub velocity: Pt,
    clock: f32,
    samples: [(f32, Pt); SAMPLES],
    len: usize,
}

impl Tracker {
    pub fn begin(p: Pt) -> Self {
        Self {
            start: p,
            current: p,
            velocity: Pt { x: 0.0, y: 0.0 },
            clock: 0.0,
            samples: [(0.0, p); SAMPLES],
            len: 1,
        }
    }

    pub fn update(&mut self, p: Pt, dt: f32) {
        self.clock += dt.max(0.0);
        if self.len == SAMPLES {
            self.samples.copy_within(1.., 0);
            self.len -= 1;
        }
        self.samples[self.len] = (self.clock, p);
        self.len += 1;
        self.current = p;
        self.refit();
    }

    /// The finger held still for `dt`. Held fingers send no motion, so call
    /// this before reading a release velocity; otherwise drag-and-hold
    /// releases as a flick.
    pub fn decay(&mut self, dt: f32) {
        self.clock += dt.max(0.0);
        self.refit();
    }

    fn refit(&mut self) {
        let recent = &self.samples[..self.len];
        let last_t = recent[recent.len() - 1].0;
        let held = (self.clock - last_t > HOLD_GAP).then_some((self.clock, self.current));
        let pts = recent
            .iter()
            .copied()
            .filter(|(t, _)| self.clock - t <= VELOCITY_WINDOW)
            .chain(held);
        let (mut n, mut st, mut sx, mut sy) = (0.0f32, 0.0, 0.0, 0.0);
        for (t, p) in pts.clone() {
            n += 1.0;
            st += t;
            sx += p.x;
            sy += p.y;
        }
        self.velocity = Pt { x: 0.0, y: 0.0 };
        if n < 2.0 {
            return;
        }
        let (mt, mx, my) = (st / n, sx / n, sy / n);
        let (mut vt, mut cx, mut cy) = (0.0f32, 0.0, 0.0);
        for (t, p) in pts {
            let dt = t - mt;
            vt += dt * dt;
            cx += dt * (p.x - mx);
            cy += dt * (p.y - my);
        }
        if vt > 1e-9 {
            self.velocity = Pt {
                x: cx / vt,
                y: cy / vt,
            };
        }
    }

    pub fn up_progress(&self) -> f32 {
        (self.start.y - self.current.y).max(0.0)
    }
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
        t.update(Pt { x: 0.5, y: 0.5 }, 1.0 / 90.0);
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
    fn measures_a_steady_drag_exactly() {
        let mut t = Tracker::begin(Pt { x: 0.2, y: 0.9 });
        for i in 1..=20 {
            let s = i as f32 / 120.0;
            t.update(
                Pt {
                    x: 0.2 + 0.5 * s,
                    y: 0.9 - 2.0 * s,
                },
                1.0 / 120.0,
            );
        }
        assert!((t.velocity.x - 0.5).abs() < 1e-2, "{:?}", t.velocity);
        assert!((t.velocity.y + 2.0).abs() < 1e-2, "{:?}", t.velocity);
    }

    #[test]
    fn a_short_flick_reads_full_speed() {
        // Three events: the old low-pass read this at about half speed.
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.9 });
        for i in 1..=3 {
            t.update(
                Pt {
                    x: 0.5,
                    y: 0.9 - 0.02 * i as f32,
                },
                0.01,
            );
        }
        assert!((t.velocity.y + 2.0).abs() < 1e-2, "{:?}", t.velocity);
    }

    #[test]
    fn a_brief_pause_before_release_keeps_the_flick() {
        let mut t = Tracker::begin(Pt { x: 0.5, y: 0.9 });
        for i in 1..=5 {
            t.update(
                Pt {
                    x: 0.5,
                    y: 0.9 - 0.02 * i as f32,
                },
                0.01,
            );
        }
        t.decay(0.008);
        assert!((t.velocity.y + 2.0).abs() < 1e-2, "{:?}", t.velocity);
    }
}
