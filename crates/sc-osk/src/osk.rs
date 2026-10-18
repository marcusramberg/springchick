//! Touches to key events, like a phone keyboard on top of a real one: keys go
//! down with the finger and up with it, so the compositor's repeat and
//! modifiers just work. Shift and Ctrl/Alt/Super latch: one tap holds them for
//! the next key, a double tap locks them.

use std::collections::HashMap;

use crate::grid::{code, Bar, Key, Mod, Page};

/// Two taps this close lock a latch.
const DOUBLE_TAP: f64 = 0.35;
/// Cursor-key travel per arrow press, logical px; text lines are taller
/// than characters are wide.
const STEP_X: f32 = 12.0;
const STEP_Y: f32 = 18.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Latch {
    #[default]
    Off,
    Once,
    Locked,
}

impl Latch {
    fn tap(&mut self, last_tap: &mut f64, now: f64) {
        *self = match *self {
            Latch::Off => Latch::Once,
            Latch::Once if now - *last_tap < DOUBLE_TAP => Latch::Locked,
            Latch::Once | Latch::Locked => Latch::Off,
        };
        *last_tap = now;
    }

    fn used(&mut self) {
        if *self == Latch::Once {
            *self = Latch::Off;
        }
    }

    pub fn on(self) -> bool {
        self != Latch::Off
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    Press(u16),
    Release(u16),
    NextLayout,
}

#[derive(Default)]
pub struct Osk {
    pub page: Page,
    pub shift: Latch,
    pub mods: [Latch; 3],
    pub bar: Bar,
    last_tap: [f64; 4],
    /// Per touch: the key it pressed and the codes to release, in order.
    held: HashMap<i32, (Key, Vec<u16>)>,
    cursor: Option<CursorDrag>,
}

struct CursorDrag {
    touch: i32,
    /// Where the last arrow fired.
    anchor: (f32, f32),
    moved: bool,
}

impl Osk {
    /// Keys a finger is holding, for drawing them pressed.
    pub fn pressed(&self) -> impl Iterator<Item = Key> + '_ {
        self.held.values().map(|(k, _)| *k)
    }

    pub fn mod_latch(&self, m: Mod) -> Latch {
        self.mods[m as usize]
    }

    /// `at` in logical px; `now` in seconds, any epoch.
    pub fn down(&mut self, touch: i32, key: Key, at: (f32, f32), now: f64) -> Vec<Out> {
        let mut out = Vec::new();
        let press = |out: &mut Vec<Out>, wrap: Vec<u16>, code: u16| {
            out.extend(wrap.iter().map(|&c| Out::Press(c)));
            out.push(Out::Press(code));
            let mut release = vec![code];
            release.extend(wrap.iter().rev());
            release
        };
        let held = match key {
            Key::Char { code, shift } => {
                let mut wrap = self.mod_codes();
                if shift || (self.page == Page::Letters && self.shift.on()) {
                    wrap.push(code::LEFTSHIFT);
                }
                self.shift.used();
                self.use_mods();
                press(&mut out, wrap, code)
            }
            Key::Plain(code) => {
                let wrap = self.mod_codes();
                self.use_mods();
                press(&mut out, wrap, code)
            }
            Key::Shift => {
                self.shift.tap(&mut self.last_tap[3], now);
                Vec::new()
            }
            Key::Mod(m) => {
                self.mods[m as usize].tap(&mut self.last_tap[m as usize], now);
                Vec::new()
            }
            Key::Page(p) => {
                self.page = p;
                Vec::new()
            }
            Key::NextLayout => {
                out.push(Out::NextLayout);
                Vec::new()
            }
            Key::Paste => {
                let mut wrap = vec![code::LEFTCTRL];
                if self.bar == Bar::Terminal {
                    wrap.push(code::LEFTSHIFT);
                }
                // Tapped, not held: a held paste would repeat.
                let release = press(&mut out, wrap, code::V);
                out.extend(release.into_iter().map(Out::Release));
                Vec::new()
            }
            Key::Cursor => {
                self.cursor = Some(CursorDrag {
                    touch,
                    anchor: at,
                    moved: false,
                });
                Vec::new()
            }
        };
        self.held.insert(touch, (key, held));
        out
    }

    /// Only the cursor key acts on travel: an arrow per step, with latched
    /// modifiers and shift (so shift selects).
    pub fn motion(&mut self, touch: i32, at: (f32, f32)) -> Vec<Out> {
        let mut wrap = self.mod_codes();
        if self.shift.on() {
            wrap.push(code::LEFTSHIFT);
        }
        let Some(drag) = self.cursor.as_mut().filter(|d| d.touch == touch) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        loop {
            let (dx, dy) = (at.0 - drag.anchor.0, at.1 - drag.anchor.1);
            let (arrow, step) = if dx.abs() >= STEP_X && dx.abs() / STEP_X >= dy.abs() / STEP_Y {
                let s = STEP_X.copysign(dx);
                (if dx > 0.0 { code::RIGHT } else { code::LEFT }, (s, 0.0))
            } else if dy.abs() >= STEP_Y {
                let s = STEP_Y.copysign(dy);
                (if dy > 0.0 { code::DOWN } else { code::UP }, (0.0, s))
            } else {
                break;
            };
            out.extend(wrap.iter().map(|&c| Out::Press(c)));
            out.push(Out::Press(arrow));
            out.push(Out::Release(arrow));
            out.extend(wrap.iter().rev().map(|&c| Out::Release(c)));
            drag.anchor = (drag.anchor.0 + step.0, drag.anchor.1 + step.1);
            drag.moved = true;
        }
        out
    }

    pub fn up(&mut self, touch: i32) -> Vec<Out> {
        if self.cursor.as_ref().is_some_and(|d| d.touch == touch)
            && self.cursor.take().is_some_and(|d| d.moved)
        {
            self.shift.used();
            self.use_mods();
        }
        self.held
            .remove(&touch)
            .map(|(_, codes)| codes.into_iter().map(Out::Release).collect())
            .unwrap_or_default()
    }

    /// Everything still held goes up: hiding, or a cancelled touch.
    pub fn release_all(&mut self) -> Vec<Out> {
        let touches: Vec<i32> = self.held.keys().copied().collect();
        touches.into_iter().flat_map(|t| self.up(t)).collect()
    }

    fn mod_codes(&self) -> Vec<u16> {
        Mod::ALL
            .into_iter()
            .filter(|&m| self.mods[m as usize].on())
            .map(Mod::code)
            .collect()
    }

    fn use_mods(&mut self) {
        self.mods.iter_mut().for_each(Latch::used);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Out::{Press, Release};

    const A: Key = Key::Char {
        code: 30,
        shift: false,
    };

    #[test]
    fn a_key_goes_down_and_up_with_the_finger() {
        let mut k = Osk::default();
        assert_eq!(k.down(0, A, (0.0, 0.0), 0.0), [Press(30)]);
        assert_eq!(k.pressed().collect::<Vec<_>>(), [A]);
        assert_eq!(k.up(0), [Release(30)]);
        assert_eq!(k.up(0), []);
    }

    #[test]
    fn shift_once_wraps_the_next_letter_only() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, (0.0, 0.0), 0.0);
        k.up(0);
        assert_eq!(k.down(1, A, (0.0, 0.0), 1.0), [Press(42), Press(30)]);
        assert_eq!(k.up(1), [Release(30), Release(42)]);
        assert_eq!(k.down(2, A, (0.0, 0.0), 2.0), [Press(30)]);
    }

    #[test]
    fn double_tap_locks_and_a_third_unlocks() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, (0.0, 0.0), 0.0);
        k.down(0, Key::Shift, (0.0, 0.0), 0.2);
        assert_eq!(k.shift, Latch::Locked);
        k.down(1, A, (0.0, 0.0), 1.0);
        k.up(1);
        assert_eq!(k.shift, Latch::Locked);
        k.down(0, Key::Shift, (0.0, 0.0), 2.0);
        assert_eq!(k.shift, Latch::Off);
    }

    #[test]
    fn slow_second_tap_turns_shift_off() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, (0.0, 0.0), 0.0);
        k.down(0, Key::Shift, (0.0, 0.0), 1.0);
        assert_eq!(k.shift, Latch::Off);
    }

    #[test]
    fn ctrl_latches_onto_plain_keys_too() {
        let mut k = Osk::default();
        k.down(0, Key::Mod(Mod::Ctrl), (0.0, 0.0), 0.0);
        assert_eq!(
            k.down(1, Key::Plain(code::BACKSPACE), (0.0, 0.0), 1.0),
            [Press(29), Press(14)]
        );
        assert_eq!(k.up(1), [Release(14), Release(29)]);
        assert_eq!(k.mod_latch(Mod::Ctrl), Latch::Off);
    }

    #[test]
    fn shift_doesnt_touch_space_or_numbers() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, (0.0, 0.0), 0.0);
        assert_eq!(
            k.down(1, Key::Plain(code::SPACE), (0.0, 0.0), 1.0),
            [Press(57)]
        );
        assert_eq!(k.shift, Latch::Once);
        k.page = Page::Numbers;
        assert_eq!(
            k.down(
                2,
                Key::Char {
                    code: 2,
                    shift: false
                },
                (0.0, 0.0),
                1.5
            ),
            [Press(2)]
        );
    }

    #[test]
    fn symbols_carry_their_own_shift() {
        let mut k = Osk {
            page: Page::Symbols,
            ..Default::default()
        };
        assert_eq!(
            k.down(
                0,
                Key::Char {
                    code: 2,
                    shift: true
                },
                (0.0, 0.0),
                0.0
            ),
            [Press(42), Press(2)]
        );
    }

    #[test]
    fn paste_is_a_chord_per_app() {
        let mut k = Osk::default();
        assert_eq!(
            k.down(0, Key::Paste, (0.0, 0.0), 0.0),
            [Press(29), Press(47), Release(47), Release(29)]
        );
        k.bar = Bar::Terminal;
        assert_eq!(
            k.down(1, Key::Paste, (0.0, 0.0), 0.0),
            [
                Press(29),
                Press(42),
                Press(47),
                Release(47),
                Release(42),
                Release(29)
            ]
        );
    }

    #[test]
    fn cursor_key_steps_in_the_swipe_direction() {
        let mut k = Osk::default();
        k.down(0, Key::Cursor, (100.0, 10.0), 0.0);
        assert_eq!(k.motion(0, (105.0, 12.0)), []);
        assert_eq!(
            k.motion(0, (126.0, 12.0)),
            [Press(106), Release(106), Press(106), Release(106)]
        );
        assert_eq!(k.motion(0, (126.0, -10.0)), [Press(103), Release(103)]);
        assert_eq!(k.motion(1, (0.0, 0.0)), [], "another finger");
        k.up(0);
        assert_eq!(k.motion(0, (300.0, 0.0)), []);
    }

    #[test]
    fn shift_selects_along_a_cursor_swipe_and_is_used_up() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, (0.0, 0.0), 0.0);
        k.up(0);
        k.down(1, Key::Cursor, (0.0, 0.0), 1.0);
        assert_eq!(
            k.motion(1, (-13.0, 0.0)),
            [Press(42), Press(105), Release(105), Release(42)]
        );
        k.up(1);
        assert_eq!(k.shift, Latch::Off);
    }

    #[test]
    fn two_fingers_and_release_all() {
        let mut k = Osk::default();
        k.down(0, A, (0.0, 0.0), 0.0);
        k.down(
            1,
            Key::Char {
                code: 31,
                shift: false,
            },
            (0.0, 0.0),
            0.1,
        );
        let mut up = k.release_all();
        up.sort_by_key(|o| match o {
            Release(c) => *c,
            _ => 0,
        });
        assert_eq!(up, [Release(30), Release(31)]);
    }
}
