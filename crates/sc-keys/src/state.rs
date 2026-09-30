//! Short/long press state machine. A long press fires as soon as the threshold
//! passes, and suppresses the short binding on release.

use sc_config::{Action, ModMask, PressKind};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PressOutcome {
    /// Fire, and keep the key from the client.
    Fire(Action),
    /// Keep the key from the client.
    Swallow,
    Forward,
}

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
    /// Last duplicate wins.
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

    /// Diagnostics only: matching needs the modifiers too (see
    /// [`PressTracker::on_release`]).
    pub fn binds_keysym(&self, keysym: u32) -> bool {
        self.map.keys().any(|(k, _)| *k == keysym)
    }

    /// Short or long: the blanking policy asks before that's decided.
    pub fn binds_action(&self, keysym: u32, mods: ModMask, action: &Action) -> bool {
        self.slot(keysym, mods).is_some_and(|slot| {
            slot.short.as_ref() == Some(action) || slot.long.as_ref() == Some(action)
        })
    }
}

#[derive(Clone, Debug)]
struct Held {
    mods: ModMask,
    pressed_at: Instant,
    long_fired: bool,
}

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

    /// Any bound key is swallowed, even with only a long binding.
    pub fn on_press(&mut self, keysym: u32, mods: ModMask, now: Instant) -> PressOutcome {
        if self.bindings.slot(keysym, mods).is_none() {
            return PressOutcome::Forward;
        }
        // A repeat press must not restart the long-press clock.
        self.held.entry(keysym).or_insert(Held {
            mods,
            pressed_at: now,
            long_fired: false,
        });
        PressOutcome::Swallow
    }

    pub fn on_release(&mut self, keysym: u32, now: Instant) -> PressOutcome {
        let Some(held) = self.held.remove(&keysym) else {
            // No record of the press, so it was forwarded and the client needs the
            // release. Don't swallow on a keysym-only match: with `Super+s` bound, a
            // bare `s` would repeat forever.
            return PressOutcome::Forward;
        };

        if held.long_fired {
            return PressOutcome::Swallow;
        }
        let elapsed = now.saturating_duration_since(held.pressed_at);
        if elapsed >= self.bindings.long_press {
            // Held past the threshold but never polled; the long binding owns it.
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

    /// One action per call; callers loop.
    pub fn poll(&mut self, now: Instant) -> Option<Action> {
        let long_press = self.bindings.long_press;
        // Earliest press first when two cross in the same tick.
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
            if let Some(action) = self
                .bindings
                .slot(keysym, mods)
                .and_then(|s| s.long.clone())
            {
                if let Some(h) = self.held.get_mut(&keysym) {
                    h.long_fired = true;
                }
                return Some(action);
            }
            if let Some(h) = self.held.get_mut(&keysym) {
                h.long_fired = true;
            }
        }
        None
    }

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
    use sc_config::{Action, ModMask, PressKind};

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
    fn release_of_a_never_pressed_key_is_forwarded() {
        let (mut t, t0) = tracker();
        assert_eq!(t.on_release(VOL_UP, t0), PressOutcome::Forward);
    }

    #[test]
    fn a_bare_key_bound_only_under_modifiers_passes_through_both_ways() {
        // `Super+s` bound, bare `s` not: the bare release must be forwarded.
        const S: u32 = 0x73;
        let logo = ModMask {
            logo: true,
            ..ModMask::NONE
        };
        let mut t = PressTracker::new(KeyBindings::new(
            vec![(S, logo, PressKind::Short, Action::Command("home".into()))],
            Duration::from_millis(500),
        ));
        let t0 = Instant::now();
        assert_eq!(t.on_press(S, ModMask::NONE, t0), PressOutcome::Forward);
        assert_eq!(t.on_release(S, at(t0, 30)), PressOutcome::Forward);

        assert_eq!(t.on_press(S, logo, at(t0, 100)), PressOutcome::Swallow);
        assert!(matches!(
            t.on_release(S, at(t0, 130)),
            PressOutcome::Fire(_)
        ));
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
