//! Display blanking policy. The CRTC side lives in `drm_backend.rs`; winit
//! ignores it.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyWhileBlanked {
    /// The press woke the screen and must not fire its binding.
    Woke,
    /// Nobody is watching: a pocket press must not fire a binding.
    Swallow,
    /// Screen on, or an external display shows the session.
    Normal,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Blank {
    blanked: bool,
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

    /// Idempotent.
    pub fn set(&mut self, blanked: bool) {
        if self.blanked == blanked {
            return;
        }
        self.blanked = blanked;
        self.dirty = true;
    }

    pub fn take_change(&mut self) -> Option<bool> {
        self.dirty.then(|| {
            self.dirty = false;
            self.blanked
        })
    }

    /// Only `wake_key` (the `toggle-display` key) wakes the panel; typing at an
    /// external display must not. Other keys are swallowed unless an external
    /// display is attached.
    pub fn on_key_press(&mut self, wake_key: bool, external_display: bool) -> KeyWhileBlanked {
        if !self.blanked {
            return KeyWhileBlanked::Normal;
        }
        if wake_key {
            self.blanked = false;
            self.dirty = true;
            return KeyWhileBlanked::Woke;
        }
        if external_display {
            KeyWhileBlanked::Normal
        } else {
            KeyWhileBlanked::Swallow
        }
    }
}

/// Idle-blank timer. Acting on it is the backend's job.
#[derive(Clone, Copy, Debug)]
pub struct Idle {
    /// `None` disables idle blanking.
    timeout: Option<std::time::Duration>,
    last_activity: std::time::Instant,
}

impl Idle {
    /// `secs == 0` disables. `now` seeds the clock so it waits a full timeout.
    pub fn new(secs: u64, now: std::time::Instant) -> Self {
        Idle {
            timeout: (secs > 0).then(|| std::time::Duration::from_secs(secs)),
            last_activity: now,
        }
    }

    pub fn activity(&mut self, now: std::time::Instant) {
        self.last_activity = now;
    }

    /// The caller must still check the panel isn't already blanked.
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
    fn the_power_key_wakes_the_screen_instead_of_firing() {
        let mut b = Blank::new();
        assert!(!b.is_blanked());
        b.toggle();
        assert!(b.is_blanked());
        assert_eq!(b.on_key_press(true, false), KeyWhileBlanked::Woke);
        assert!(!b.is_blanked());
        assert_eq!(b.on_key_press(true, false), KeyWhileBlanked::Normal);
    }

    #[test]
    fn other_keys_never_wake_the_panel() {
        let mut b = Blank::new();
        b.set(true);
        assert_eq!(b.take_change(), Some(true));
        assert_eq!(b.on_key_press(false, false), KeyWhileBlanked::Swallow);
        assert_eq!(b.on_key_press(false, true), KeyWhileBlanked::Normal);
        assert!(b.is_blanked());
        assert_eq!(b.take_change(), None);
    }

    #[test]
    fn setting_a_state_is_idempotent() {
        let mut b = Blank::new();
        b.set(true);
        assert!(b.is_blanked());
        assert_eq!(b.take_change(), Some(true));

        b.set(true);
        assert!(b.is_blanked());
        assert_eq!(b.take_change(), None);

        assert_eq!(b.on_key_press(true, false), KeyWhileBlanked::Woke);
        assert!(!b.is_blanked());
    }

    #[test]
    fn changes_are_reported_once() {
        let mut b = Blank::new();
        assert_eq!(b.take_change(), None);
        b.toggle();
        assert_eq!(b.take_change(), Some(true));
        assert_eq!(b.take_change(), None);
        b.on_key_press(true, false);
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
        assert!(!idle.should_blank(t0 + Duration::from_secs(590)));
        assert!(idle.should_blank(t0 + Duration::from_secs(1100)));
    }

    #[test]
    fn zero_timeout_disables_idle_blanking() {
        let t0 = Instant::now();
        let idle = Idle::new(0, t0);
        assert!(!idle.should_blank(t0 + Duration::from_secs(100_000)));
    }
}
