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

/// Live touch-visualization state. Cheap to keep around even when disabled — it
/// only ever holds anything while `show_touches` routes events into it.
#[derive(Default)]
pub struct TouchViz {
    /// Held contacts keyed by input id (touch slot value, or [`POINTER_ID`] for
    /// the pointer). Value is the current physical position.
    active: HashMap<u64, (f32, f32)>,
    /// Lifted contacts still fading out.
    ripples: Vec<Ripple>,
}

/// Synthetic id for the (single) pointer contact, kept out of the touch-slot
/// range so a mouse press and finger 0 never alias.
pub const POINTER_ID: u64 = u64::MAX;

impl TouchViz {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record (or move) a held contact at physical `(x, y)`.
    pub fn contact(&mut self, id: u64, x: f32, y: f32) {
        self.active.insert(id, (x, y));
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

    /// Drop finished ripples. Call once per frame before [`Self::is_active`].
    pub fn prune(&mut self, now: Instant) {
        self.ripples
            .retain(|r| now.saturating_duration_since(r.released_at) < RIPPLE_DUR);
    }

    /// Whether anything still needs drawing (and the frame loop kept awake).
    pub fn is_active(&self, now: Instant) -> bool {
        !self.active.is_empty()
            || self
                .ripples
                .iter()
                .any(|r| now.saturating_duration_since(r.released_at) < RIPPLE_DUR)
    }

    /// The marks to paint this frame. Held contacts first (solid discs), then
    /// release rings (expanding, fading).
    pub fn marks(&self, now: Instant) -> Vec<TouchMark> {
        let mut marks = Vec::with_capacity(self.active.len() + self.ripples.len());
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
        v.contact(0, 100.0, 200.0);
        let now = Instant::now();
        let marks = v.marks(now);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].x, 100.0);
        assert_eq!(marks[0].y, 200.0);
        assert!(marks[0].filled);
        assert!(v.is_active(now));
    }

    #[test]
    fn contact_moves_in_place_not_appended() {
        let mut v = TouchViz::new();
        v.contact(0, 10.0, 10.0);
        v.contact(0, 50.0, 60.0);
        let marks = v.marks(Instant::now());
        assert_eq!(marks.len(), 1);
        assert_eq!((marks[0].x, marks[0].y), (50.0, 60.0));
    }

    #[test]
    fn release_ripple_expands_and_fades_then_expires() {
        let mut v = TouchViz::new();
        let t0 = Instant::now();
        v.contact(0, 0.0, 0.0);
        v.release(0, t0);
        // Just after release: a fading ring, larger than nothing, alpha < full.
        let early = v.marks(t0 + Duration::from_millis(10));
        assert_eq!(early.len(), 1);
        assert!(!early[0].filled);
        assert!(early[0].radius >= CONTACT_RADIUS);
        // Midway it has grown and dimmed.
        let mid = v.marks(t0 + Duration::from_millis(175));
        assert!(mid[0].radius > early[0].radius);
        assert!(mid[0].alpha < early[0].alpha);
        // Past the duration it is gone and prune clears it.
        let late = t0 + Duration::from_millis(400);
        assert!(v.marks(late).is_empty());
        assert!(!v.is_active(late));
        v.prune(late);
        assert!(!v.is_active(late));
    }

    #[test]
    fn multiple_fingers_each_get_a_mark() {
        let mut v = TouchViz::new();
        v.contact(0, 1.0, 1.0);
        v.contact(1, 2.0, 2.0);
        v.contact(POINTER_ID, 3.0, 3.0);
        assert_eq!(v.marks(Instant::now()).len(), 3);
    }

    #[test]
    fn releasing_unknown_id_is_a_noop() {
        let mut v = TouchViz::new();
        v.release(7, Instant::now());
        assert!(v.marks(Instant::now()).is_empty());
    }
}
