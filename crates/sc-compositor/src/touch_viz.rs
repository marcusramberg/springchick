//! Visual touch indicator overlay (for demo recordings).
//!
//! Opt-in via `[main].show_touches` in `config.toml`. When enabled, the input
//! path feeds every contact (touch slot or pointer press) into a [`TouchViz`],
//! which the render path turns into on-screen marks: a soft filled disc under
//! each held finger plus an expanding ring that fades out when it lifts. The
//! geometry/fade math is pure and tested here; the actual Skia drawing lives in
//! `skia_gl`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Physical-pixel radius of the solid disc drawn under a held finger. Sized for
/// the FP5 panel (≈1224×2700 at dpi 3); it reads as a fingertip, not a dot.
const CONTACT_RADIUS: f32 = 55.0;

/// How long a release ring takes to expand and fade to nothing.
const RIPPLE_DUR: Duration = Duration::from_millis(350);

/// How far the release ring grows, as a multiple of [`CONTACT_RADIUS`].
const RIPPLE_GROWTH: f32 = 2.2;

/// How long a trail breadcrumb lingers before fading out. Long enough that a
/// swipe leaves a visible path on a recording, short enough not to smear.
const TRAIL_DUR: Duration = Duration::from_millis(450);

/// Minimum physical-pixel move between breadcrumbs, so a stationary press does
/// not stack dozens of dots on one spot (only motion lays a trail).
const TRAIL_MIN_STEP: f32 = 14.0;

/// Trail-dot radius as a fraction of [`CONTACT_RADIUS`] — smaller than the live
/// fingertip so the current contact still reads as the head of the trail.
const TRAIL_RADIUS_FRAC: f32 = 0.5;

/// A single primitive the overlay should paint. `filled` discs are held
/// contacts; unfilled ones are the expanding release ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchMark {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    /// 0.0..=1.0 opacity.
    pub alpha: f32,
    /// Solid disc (held) vs. stroked ring (releasing).
    pub filled: bool,
}

/// A finger that lifted, animating its ring out from where it left.
#[derive(Clone, Copy, Debug)]
struct Ripple {
    x: f32,
    y: f32,
    released_at: Instant,
}

/// A breadcrumb dropped along a moving contact's path, fading over [`TRAIL_DUR`].
#[derive(Clone, Copy, Debug)]
struct TrailPoint {
    x: f32,
    y: f32,
    at: Instant,
}

/// Live touch-visualization state. Cheap to keep around even when disabled — it
/// only ever holds anything while `show_touches` routes events into it.
#[derive(Default)]
pub struct TouchViz {
    /// Held contacts keyed by input id (touch slot value, or [`POINTER_ID`] for
    /// the pointer). Value is the current physical position.
    active: HashMap<u64, (f32, f32)>,
    /// Lifted contacts still fading out.
    ripples: Vec<Ripple>,
    /// Fading breadcrumbs marking where moving contacts have been, so a swipe or
    /// drag draws a visible path instead of a lone dot.
    trail: Vec<TrailPoint>,
}

/// Synthetic id for the (single) pointer contact, kept out of the touch-slot
/// range so a mouse press and finger 0 never alias.
pub const POINTER_ID: u64 = u64::MAX;

impl TouchViz {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or move) a held contact at physical `(x, y)`. Lays a fading
    /// breadcrumb when the contact has moved far enough since its last position,
    /// so motion draws a trail while a stationary press stays a single dot.
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

    /// A contact lifted: convert its last position into a fading ring.
    pub fn release(&mut self, id: u64, now: Instant) {
        if let Some((x, y)) = self.active.remove(&id) {
            self.ripples.push(Ripple {
                x,
                y,
                released_at: now,
            });
        }
    }

    /// Drop finished ripples and expired trail breadcrumbs. Call once per frame
    /// before [`Self::is_active`].
    pub fn prune(&mut self, now: Instant) {
        self.ripples
            .retain(|r| now.saturating_duration_since(r.released_at) < RIPPLE_DUR);
        self.trail
            .retain(|p| now.saturating_duration_since(p.at) < TRAIL_DUR);
    }

    /// Whether anything still needs drawing (and the frame loop kept awake).
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

    /// The marks to paint this frame. Trail breadcrumbs first (drawn underneath),
    /// then held contacts (solid discs), then release rings (expanding, fading).
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
        // The down lays one breadcrumb (trail) plus the live fingertip disc.
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
        // First contact drops one breadcrumb; a tiny sub-threshold nudge adds none.
        v.contact(0, 10.0, 10.0, now);
        v.contact(0, 10.0 + TRAIL_MIN_STEP / 2.0, 10.0, now);
        // One trail dot + one live disc, and the disc sits at the latest position.
        let marks = v.marks(now);
        assert_eq!(marks.len(), 2);
        let head = marks.last().unwrap();
        assert_eq!((head.x, head.y), (10.0 + TRAIL_MIN_STEP / 2.0, 10.0));
    }

    #[test]
    fn a_swipe_lays_a_fading_trail() {
        let mut v = TouchViz::new();
        let t0 = Instant::now();
        // A drag across the panel: each step is well past the breadcrumb
        // threshold, so every move leaves a dot.
        for i in 0..5 {
            let x = i as f32 * (TRAIL_MIN_STEP + 5.0);
            v.contact(0, x, 0.0, t0);
        }
        let marks = v.marks(t0);
        // 5 breadcrumbs (unfilled-radius trail) + 1 live fingertip disc.
        assert_eq!(marks.len(), 6);
        // Trail dots come first and are smaller than the live head.
        assert_eq!(marks[0].radius, CONTACT_RADIUS * TRAIL_RADIUS_FRAC);
        assert_eq!(marks.last().unwrap().radius, CONTACT_RADIUS);

        // Trail persists after the finger lifts, then fades and prunes away.
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
        // Isolate the release ring (the only unfilled mark) from any trail dot.
        let ring = |v: &TouchViz, at: Instant| v.marks(at).into_iter().find(|m| !m.filled);
        // Just after release: a fading ring, larger than nothing, alpha < full.
        let early = ring(&v, t0 + Duration::from_millis(10)).expect("ring present");
        assert!(early.radius >= CONTACT_RADIUS);
        // Midway it has grown and dimmed.
        let mid = ring(&v, t0 + Duration::from_millis(175)).expect("ring present");
        assert!(mid.radius > early.radius);
        assert!(mid.alpha < early.alpha);
        // Past the ripple duration the ring is gone (trail may still linger).
        let after_ripple = t0 + Duration::from_millis(400);
        assert!(ring(&v, after_ripple).is_none());
        // Past every duration, nothing remains and prune clears it.
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
        // Three live fingertip discs (plus each finger's initial breadcrumb).
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
