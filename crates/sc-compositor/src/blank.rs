//! Display blanking policy.
//!
//! The flag itself is backend-agnostic and testable; turning the CRTC off lives
//! in `drm_backend.rs`, and the winit backend simply ignores it.

/// What a key press means while the panel is blanked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWhileBlanked {
    /// The press woke the screen and must not fire its binding.
    Woke,
    /// Screen was already on; handle the key normally.
    Normal,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Blank {
    blanked: bool,
    /// Set when the state changed, so the backend can act on it once.
    dirty: bool,
}

impl Blank {
    pub fn new() -> Self {
        Blank::default()
    }

    pub fn is_blanked(&self) -> bool {
        self.blanked
    }

    pub fn toggle(&mut self) {
        self.blanked = !self.blanked;
        self.dirty = true;
    }

    /// Consume the "state changed" flag. Returns `Some(blanked)` once per change.
    pub fn take_change(&mut self) -> Option<bool> {
        self.dirty.then(|| {
            self.dirty = false;
            self.blanked
        })
    }

    /// A key press arrived. While blanked, the first press only wakes the panel.
    pub fn on_key_press(&mut self) -> KeyWhileBlanked {
        if self.blanked {
            self.blanked = false;
            self.dirty = true;
            KeyWhileBlanked::Woke
        } else {
            KeyWhileBlanked::Normal
        }
    }
}

/// Idle-blank policy: how long since the last input, and whether that has
/// crossed the configured timeout. Pure and clock-free — the caller passes the
/// current `Instant`, so tests drive it with synthetic time. Acting on the
/// result (flipping [`Blank`]) is the backend's job.
#[derive(Clone, Copy, Debug)]
pub struct Idle {
    /// `None` disables idle blanking (config `idle_blank_secs = 0`).
    timeout: Option<std::time::Duration>,
    last_activity: std::time::Instant,
}

impl Idle {
    /// `secs == 0` disables idle blanking. `now` seeds the activity clock so a
    /// freshly built `Idle` does not fire until a full timeout has elapsed.
    pub fn new(secs: u64, now: std::time::Instant) -> Self {
        Idle {
            timeout: (secs > 0).then(|| std::time::Duration::from_secs(secs)),
            last_activity: now,
        }
    }

    /// Record that input arrived, resetting the idle countdown.
    pub fn activity(&mut self, now: std::time::Instant) {
        self.last_activity = now;
    }

    /// Whether the idle timeout has elapsed since the last activity. Always
    /// `false` when disabled. The caller must still check the panel isn't
    /// already blanked before acting.
    pub fn should_blank(&self, now: std::time::Instant) -> bool {
        match self.timeout {
            Some(timeout) => now.duration_since(self.last_activity) >= timeout,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn a_bound_key_wakes_the_screen_instead_of_firing() {
        let mut b = Blank::new();
        assert!(!b.is_blanked());
        b.toggle();
        assert!(b.is_blanked());
        assert_eq!(b.on_key_press(), KeyWhileBlanked::Woke);
        assert!(!b.is_blanked());
        assert_eq!(b.on_key_press(), KeyWhileBlanked::Normal);
    }

    #[test]
    fn changes_are_reported_once() {
        let mut b = Blank::new();
        assert_eq!(b.take_change(), None);
        b.toggle();
        assert_eq!(b.take_change(), Some(true));
        assert_eq!(b.take_change(), None);
        b.on_key_press();
        assert_eq!(b.take_change(), Some(false));
        assert_eq!(b.take_change(), None);
    }

    #[test]
    fn idle_fires_only_after_the_timeout_elapses() {
        let t0 = Instant::now();
        let idle = Idle::new(600, t0);
        assert!(!idle.should_blank(t0 + Duration::from_secs(599)));
        assert!(idle.should_blank(t0 + Duration::from_secs(600)));
        assert!(idle.should_blank(t0 + Duration::from_secs(601)));
    }

    #[test]
    fn activity_resets_the_idle_countdown() {
        let t0 = Instant::now();
        let mut idle = Idle::new(600, t0);
        idle.activity(t0 + Duration::from_secs(500));
        // 590s since t0, but only 90s since the activity: not yet.
        assert!(!idle.should_blank(t0 + Duration::from_secs(590)));
        // 500 + 600 = 1100s: timeout since the activity has now elapsed.
        assert!(idle.should_blank(t0 + Duration::from_secs(1100)));
    }

    #[test]
    fn zero_timeout_disables_idle_blanking() {
        let t0 = Instant::now();
        let idle = Idle::new(0, t0);
        assert!(!idle.should_blank(t0 + Duration::from_secs(100_000)));
    }
}
