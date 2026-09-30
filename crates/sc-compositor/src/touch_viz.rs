//! Touch indicator overlay for demo recordings (`show_touches`). Drawing is in
//! `skia_gl`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Physical pixels, sized for the FP5 at dpi 3.
const CONTACT_RADIUS: f32 = 55.0;

const RIPPLE_DUR: Duration = Duration::from_millis(350);

/// Multiple of [`CONTACT_RADIUS`].
const RIPPLE_GROWTH: f32 = 2.2;

const TRAIL_DUR: Duration = Duration::from_millis(450);

/// Minimum move between breadcrumbs, so a still press doesn't stack dots.
const TRAIL_MIN_STEP: f32 = 14.0;

/// Fraction of [`CONTACT_RADIUS`].
const TRAIL_RADIUS_FRAC: f32 = 0.5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchMark {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    pub alpha: f32,
    /// Solid disc (held) vs. stroked ring (released).
    pub filled: bool,
}

#[derive(Clone, Copy, Debug)]
struct Ripple {
    x: f32,
    y: f32,
    released_at: Instant,
}

#[derive(Clone, Copy, Debug)]
struct TrailPoint {
    x: f32,
    y: f32,
    at: Instant,
}

#[derive(Default)]
pub struct TouchViz {
    /// Keyed by touch slot or [`POINTER_ID`]; physical position.
    active: HashMap<u64, (f32, f32)>,
    ripples: Vec<Ripple>,
    trail: Vec<TrailPoint>,
}

/// Outside the touch-slot range so a mouse press and finger 0 never alias.
pub const POINTER_ID: u64 = u64::MAX;

impl TouchViz {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contact(&mut self, id: u64, x: f32, y: f32, now: Instant) {
        let moved = self
            .active
            .get(&id)
            .is_none_or(|&(px, py)| (px - x).hypot(py - y) >= TRAIL_MIN_STEP);
        self.active.insert(id, (x, y));
        if moved {
            self.trail.push(TrailPoint { x, y, at: now });
        }
    }

    pub fn release(&mut self, id: u64, now: Instant) {
        if let Some((x, y)) = self.active.remove(&id) {
            self.ripples.push(Ripple {
                x,
                y,
                released_at: now,
            });
        }
    }

    /// Call once per frame before [`Self::is_active`].
    pub fn prune(&mut self, now: Instant) {
        self.ripples
            .retain(|r| now.saturating_duration_since(r.released_at) < RIPPLE_DUR);
        self.trail
            .retain(|p| now.saturating_duration_since(p.at) < TRAIL_DUR);
    }

    pub fn is_active(&self, now: Instant) -> bool {
        !self.active.is_empty()
            || self
                .ripples
                .iter()
                .any(|r| now.saturating_duration_since(r.released_at) < RIPPLE_DUR)
            || self
                .trail
                .iter()
                .any(|p| now.saturating_duration_since(p.at) < TRAIL_DUR)
    }

    /// Trail first (underneath), then held discs, then release rings.
    pub fn marks(&self, now: Instant) -> Vec<TouchMark> {
        let mut marks =
            Vec::with_capacity(self.trail.len() + self.active.len() + self.ripples.len());
        for p in &self.trail {
            let t = now.saturating_duration_since(p.at).as_secs_f32() / TRAIL_DUR.as_secs_f32();
            if t >= 1.0 {
                continue;
            }
            marks.push(TouchMark {
                x: p.x,
                y: p.y,
                radius: CONTACT_RADIUS * TRAIL_RADIUS_FRAC,
                alpha: 0.30 * (1.0 - t),
                filled: true,
            });
        }
        for &(x, y) in self.active.values() {
            marks.push(TouchMark {
                x,
                y,
                radius: CONTACT_RADIUS,
                alpha: 0.35,
                filled: true,
            });
        }
        for r in &self.ripples {
            let t = now.saturating_duration_since(r.released_at).as_secs_f32()
                / RIPPLE_DUR.as_secs_f32();
            if t >= 1.0 {
                continue;
            }
            marks.push(TouchMark {
                x: r.x,
                y: r.y,
                radius: CONTACT_RADIUS * (1.0 + (RIPPLE_GROWTH - 1.0) * t),
                alpha: 0.5 * (1.0 - t),
                filled: false,
            });
        }
        marks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_contact_is_a_solid_disc() {
        let mut v = TouchViz::new();
        let now = Instant::now();
        v.contact(0, 100.0, 200.0, now);
        let marks = v.marks(now);
        assert_eq!(marks.len(), 2);
        let head = marks.last().unwrap();
        assert_eq!((head.x, head.y), (100.0, 200.0));
        assert_eq!(head.radius, CONTACT_RADIUS);
        assert!(head.filled);
        assert!(v.is_active(now));
    }

    #[test]
    fn stationary_contact_lays_a_single_breadcrumb() {
        let mut v = TouchViz::new();
        let now = Instant::now();
        v.contact(0, 10.0, 10.0, now);
        v.contact(0, 10.0 + TRAIL_MIN_STEP / 2.0, 10.0, now);
        let marks = v.marks(now);
        assert_eq!(marks.len(), 2);
        let head = marks.last().unwrap();
        assert_eq!((head.x, head.y), (10.0 + TRAIL_MIN_STEP / 2.0, 10.0));
    }

    #[test]
    fn a_swipe_lays_a_fading_trail() {
        let mut v = TouchViz::new();
        let t0 = Instant::now();
        for i in 0..5 {
            let x = i as f32 * (TRAIL_MIN_STEP + 5.0);
            v.contact(0, x, 0.0, t0);
        }
        let marks = v.marks(t0);
        assert_eq!(marks.len(), 6);
        assert_eq!(marks[0].radius, CONTACT_RADIUS * TRAIL_RADIUS_FRAC);
        assert_eq!(marks.last().unwrap().radius, CONTACT_RADIUS);

        v.release(0, t0);
        assert!(v.is_active(t0 + Duration::from_millis(100)));
        let gone = t0 + TRAIL_DUR + Duration::from_millis(10);
        assert!(v.marks(gone).is_empty());
        v.prune(gone);
        assert!(!v.is_active(gone));
    }

    #[test]
    fn release_ripple_expands_and_fades_then_expires() {
        let mut v = TouchViz::new();
        let t0 = Instant::now();
        v.contact(0, 0.0, 0.0, t0);
        v.release(0, t0);
        let ring = |v: &TouchViz, at: Instant| v.marks(at).into_iter().find(|m| !m.filled);
        let early = ring(&v, t0 + Duration::from_millis(10)).expect("ring present");
        assert!(early.radius >= CONTACT_RADIUS);
        let mid = ring(&v, t0 + Duration::from_millis(175)).expect("ring present");
        assert!(mid.radius > early.radius);
        assert!(mid.alpha < early.alpha);
        let after_ripple = t0 + Duration::from_millis(400);
        assert!(ring(&v, after_ripple).is_none());
        let late = t0 + TRAIL_DUR + Duration::from_millis(10);
        assert!(v.marks(late).is_empty());
        v.prune(late);
        assert!(!v.is_active(late));
    }

    #[test]
    fn multiple_fingers_each_get_a_mark() {
        let mut v = TouchViz::new();
        let now = Instant::now();
        v.contact(0, 1.0, 1.0, now);
        v.contact(1, 2.0, 2.0, now);
        v.contact(POINTER_ID, 3.0, 3.0, now);
        let live = v
            .marks(now)
            .into_iter()
            .filter(|m| m.radius == CONTACT_RADIUS)
            .count();
        assert_eq!(live, 3);
    }

    #[test]
    fn releasing_unknown_id_is_a_noop() {
        let mut v = TouchViz::new();
        v.release(7, Instant::now());
        assert!(v.marks(Instant::now()).is_empty());
    }
}
