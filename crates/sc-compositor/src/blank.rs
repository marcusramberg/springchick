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

#[cfg(test)]
mod tests {
    use super::*;

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
}
