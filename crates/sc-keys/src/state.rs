//! Short/long press state machine.
//!
//! Clock-injected: the caller supplies `Instant`s, so the timing rules are
//! testable without sleeping. A long press fires the moment the threshold is
//! crossed, while the key is still held; the matching short binding is then
//! suppressed on release.

use crate::config::{Action, ModMask, PressKind};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// What the compositor should do with a key event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PressOutcome {
    /// Run this action, and keep the key from the focused client.
    Fire(Action),
    /// Keep the key from the focused client; nothing to run.
    Swallow,
    /// Not bound — forward to the focused client.
    Forward,
}

/// Resolved bindings, keyed by `(keysym, modifiers)`.
#[derive(Clone, Debug, Default)]
pub struct KeyBindings {
    map: HashMap<(u32, ModMask), Slot>,
    long_press: Duration,
}

#[derive(Clone, Debug, Default)]
struct Slot {
    short: Option<Action>,
    long: Option<Action>,
}

impl KeyBindings {
    /// Build from resolved `(keysym, mods, press, action)` tuples. A duplicate
    /// `(keysym, mods, press)` triple means the last entry wins.
    pub fn new(
        entries: impl IntoIterator<Item = (u32, ModMask, PressKind, Action)>,
        long_press: Duration,
    ) -> Self {
        let mut map: HashMap<(u32, ModMask), Slot> = HashMap::new();
        for (keysym, mods, press, action) in entries {
            let slot = map.entry((keysym, mods)).or_default();
            match press {
                PressKind::Short => slot.short = Some(action),
                PressKind::Long => slot.long = Some(action),
            }
        }
        KeyBindings { map, long_press }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn slot(&self, keysym: u32, mods: ModMask) -> Option<&Slot> {
        self.map.get(&(keysym, mods))
    }
}

/// A key currently held down.
#[derive(Clone, Debug)]
struct Held {
    mods: ModMask,
    pressed_at: Instant,
    long_fired: bool,
}

/// Tracks held keys and decides short vs long.
#[derive(Clone, Debug)]
pub struct PressTracker {
    bindings: KeyBindings,
    held: HashMap<u32, Held>,
}

impl PressTracker {
    pub fn new(bindings: KeyBindings) -> Self {
        PressTracker {
            bindings,
            held: HashMap::new(),
        }
    }

    pub fn bindings(&self) -> &KeyBindings {
        &self.bindings
    }

    /// Key went down. Any bound key is swallowed, including one that only has a
    /// long binding — an app that should see the key needs an explicit short
    /// binding.
    pub fn on_press(&mut self, keysym: u32, mods: ModMask, now: Instant) -> PressOutcome {
        if self.bindings.slot(keysym, mods).is_none() {
            return PressOutcome::Forward;
        }
        // A repeat press of a held key must not restart the long-press clock.
        self.held.entry(keysym).or_insert(Held {
            mods,
            pressed_at: now,
            long_fired: false,
        });
        PressOutcome::Swallow
    }

    /// Key came up. Fires the short binding only if the long one did not
    /// already fire for this press.
    pub fn on_release(&mut self, keysym: u32, now: Instant) -> PressOutcome {
        let Some(held) = self.held.remove(&keysym) else {
            // Never seen down (or unbound): forward unless it is bound with the
            // current-unknown modifiers, in which case swallowing is safer than
            // leaking a stray release to the client.
            return if self.bindings.map.keys().any(|(k, _)| *k == keysym) {
                PressOutcome::Swallow
            } else {
                PressOutcome::Forward
            };
        };

        if held.long_fired {
            return PressOutcome::Swallow;
        }
        let elapsed = now.saturating_duration_since(held.pressed_at);
        if elapsed >= self.bindings.long_press {
            // Held past the threshold but never polled; the long binding owns
            // this press either way.
            return PressOutcome::Swallow;
        }
        match self
            .bindings
            .slot(keysym, held.mods)
            .and_then(|s| s.short.clone())
        {
            Some(action) => PressOutcome::Fire(action),
            None => PressOutcome::Swallow,
        }
    }

    /// Fire any long binding whose threshold has been crossed. Returns one
    /// action per call; callers poll in a loop.
    pub fn poll(&mut self, now: Instant) -> Option<Action> {
        let long_press = self.bindings.long_press;
        // Deterministic order when two keys cross in the same tick: earliest press first.
        let mut ready: Vec<(u32, Instant)> = self
            .held
            .iter()
            .filter(|(_, h)| !h.long_fired)
            .filter(|(_, h)| now.saturating_duration_since(h.pressed_at) >= long_press)
            .map(|(k, h)| (*k, h.pressed_at))
            .collect();
        ready.sort_by_key(|(_, at)| *at);

        for (keysym, _) in ready {
            let mods = self.held[&keysym].mods;
            if let Some(action) = self.bindings.slot(keysym, mods).and_then(|s| s.long.clone()) {
                if let Some(h) = self.held.get_mut(&keysym) {
                    h.long_fired = true;
                }
                return Some(action);
            }
            // No long binding: mark it so we stop reconsidering this press.
            if let Some(h) = self.held.get_mut(&keysym) {
                h.long_fired = true;
            }
        }
        None
    }

    /// When the next long press would fire, for loops that want to sleep.
    pub fn next_deadline(&self) -> Option<Instant> {
        let long_press = self.bindings.long_press;
        self.held
            .iter()
            .filter(|(_, h)| !h.long_fired)
            .filter(|(keysym, h)| {
                self.bindings
                    .slot(**keysym, h.mods)
                    .is_some_and(|s| s.long.is_some())
            })
            .map(|(_, h)| h.pressed_at + long_press)
            .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Action, ModMask, PressKind};

    const VOL_UP: u32 = 100;
    const VOL_DOWN: u32 = 200;
    const UNBOUND: u32 = 999;

    fn bindings() -> KeyBindings {
        KeyBindings::new(
            vec![
                (
                    VOL_UP,
                    ModMask::NONE,
                    PressKind::Short,
                    Action::Command("short".into()),
                ),
                (
                    VOL_UP,
                    ModMask::NONE,
                    PressKind::Long,
                    Action::Command("long".into()),
                ),
                (
                    VOL_DOWN,
                    ModMask::NONE,
                    PressKind::Long,
                    Action::Command("down-long".into()),
                ),
            ],
            Duration::from_millis(500),
        )
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn tracker() -> (PressTracker, Instant) {
        (PressTracker::new(bindings()), Instant::now())
    }

    #[test]
    fn short_press_fires_on_release() {
        let (mut t, t0) = tracker();
        assert_eq!(t.on_press(VOL_UP, ModMask::NONE, t0), PressOutcome::Swallow);
        assert_eq!(
            t.on_release(VOL_UP, at(t0, 100)),
            PressOutcome::Fire(Action::Command("short".into()))
        );
    }

    #[test]
    fn long_press_fires_at_the_threshold_without_release() {
        let (mut t, t0) = tracker();
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(t.poll(at(t0, 499)), None);
        assert_eq!(t.poll(at(t0, 500)), Some(Action::Command("long".into())));
    }

    #[test]
    fn short_is_suppressed_once_long_fired() {
        let (mut t, t0) = tracker();
        t.on_press(VOL_UP, ModMask::NONE, t0);
        t.poll(at(t0, 500));
        assert_eq!(t.on_release(VOL_UP, at(t0, 900)), PressOutcome::Swallow);
    }

    #[test]
    fn long_fires_only_once_while_held() {
        let (mut t, t0) = tracker();
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert!(t.poll(at(t0, 500)).is_some());
        assert_eq!(t.poll(at(t0, 700)), None);
    }

    #[test]
    fn key_with_only_a_long_binding_still_swallows_the_short_press() {
        let (mut t, t0) = tracker();
        assert_eq!(
            t.on_press(VOL_DOWN, ModMask::NONE, t0),
            PressOutcome::Swallow
        );
        assert_eq!(t.on_release(VOL_DOWN, at(t0, 100)), PressOutcome::Swallow);
    }

    #[test]
    fn unbound_keys_forward_both_ways() {
        let (mut t, t0) = tracker();
        assert_eq!(
            t.on_press(UNBOUND, ModMask::NONE, t0),
            PressOutcome::Forward
        );
        assert_eq!(t.on_release(UNBOUND, at(t0, 10)), PressOutcome::Forward);
    }

    #[test]
    fn wrong_modifiers_do_not_match() {
        let mods = ModMask {
            ctrl: true,
            ..ModMask::NONE
        };
        let (mut t, t0) = tracker();
        assert_eq!(t.on_press(VOL_UP, mods, t0), PressOutcome::Forward);
    }

    #[test]
    fn repeat_press_of_a_held_key_is_ignored() {
        let (mut t, t0) = tracker();
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(
            t.on_press(VOL_UP, ModMask::NONE, at(t0, 50)),
            PressOutcome::Swallow
        );
        assert!(t.poll(at(t0, 500)).is_some());
    }

    #[test]
    fn next_deadline_is_the_earliest_of_two_held_keys() {
        let (mut t, t0) = tracker();
        t.on_press(VOL_DOWN, ModMask::NONE, at(t0, 100));
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(t.next_deadline(), Some(at(t0, 500)));
    }

    #[test]
    fn next_deadline_is_none_once_everything_fired() {
        let (mut t, t0) = tracker();
        t.on_press(VOL_UP, ModMask::NONE, t0);
        t.poll(at(t0, 500));
        assert_eq!(t.next_deadline(), None);
    }

    #[test]
    fn release_of_a_never_pressed_key_is_harmless() {
        let (mut t, t0) = tracker();
        assert_eq!(t.on_release(VOL_UP, t0), PressOutcome::Swallow);
    }

    #[test]
    fn last_duplicate_binding_wins() {
        let b = KeyBindings::new(
            vec![
                (
                    VOL_UP,
                    ModMask::NONE,
                    PressKind::Short,
                    Action::Command("first".into()),
                ),
                (
                    VOL_UP,
                    ModMask::NONE,
                    PressKind::Short,
                    Action::Command("second".into()),
                ),
            ],
            Duration::from_millis(500),
        );
        let mut t = PressTracker::new(b);
        let t0 = Instant::now();
        t.on_press(VOL_UP, ModMask::NONE, t0);
        assert_eq!(
            t.on_release(VOL_UP, at(t0, 10)),
            PressOutcome::Fire(Action::Command("second".into()))
        );
    }
}
