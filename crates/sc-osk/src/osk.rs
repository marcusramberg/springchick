//! Touches to key events, like a phone keyboard on top of a real one.
//! Character keys type on release, so a long press can offer variants
//! instead of repeating; other keys go down and up with the finger, so the
//! compositor's repeat works for backspace and space. Shift and Ctrl/Alt/Super
//! latch: one tap holds them for the next key, a double tap locks them.

use std::collections::HashMap;

use crate::grid::{code, Bar, Clip, Key, Mod, Page};

/// Two taps this close lock a latch.
const DOUBLE_TAP: f64 = 0.35;
/// Holding a character key this long offers its variants.
pub const LONG_PRESS: f64 = 0.45;
/// Cursor-key travel per arrow press, logical px; text lines are taller
/// than characters are wide.
const STEP_X: f32 = 12.0;
const STEP_Y: f32 = 18.0;
/// Width of one variant in the long-press popup, logical px.
pub const POPUP_CELL: f32 = 36.0;

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Out {
    Press(u16),
    Release(u16),
    NextLayout,
    /// Committed through the input method: not every variant is in the keymap.
    Text(String),
}

/// The variants a long press offers, laid out from `x0` in `POPUP_CELL`s.
pub struct Popup {
    pub touch: i32,
    pub key: Key,
    pub options: Vec<String>,
    pub selected: usize,
    pub x0: f32,
}

struct Tap {
    key: Key,
    since: f64,
    x: f32,
    /// The long press was looked at; no second deadline.
    checked: bool,
}

#[derive(Default)]
pub struct Osk {
    pub page: Page,
    pub shift: Latch,
    pub mods: [Latch; 3],
    pub bar: Bar,
    pub popup: Option<Popup>,
    last_tap: [f64; 4],
    /// Per touch: the key it holds and the codes to release, in order.
    held: HashMap<i32, (Key, Vec<u16>)>,
    /// Character keys waiting for release or a long press.
    taps: HashMap<i32, Tap>,
    cursor: Option<CursorDrag>,
}

struct CursorDrag {
    touch: i32,
    /// Where the last arrow fired.
    anchor: (f32, f32),
    moved: bool,
}

fn chord(out: &mut Vec<Out>, wrap: &[u16], code: u16) {
    out.extend(wrap.iter().map(|&c| Out::Press(c)));
    out.push(Out::Press(code));
    out.push(Out::Release(code));
    out.extend(wrap.iter().rev().map(|&c| Out::Release(c)));
}

impl Osk {
    /// Keys a finger is holding, for drawing them pressed.
    pub fn pressed(&self) -> impl Iterator<Item = Key> + '_ {
        self.held.values().map(|(k, _)| *k)
    }

    pub fn mod_latch(&self, m: Mod) -> Latch {
        self.mods[m as usize]
    }

    /// Whether a character key types shifted right now.
    pub fn shifted(&self, key: Key) -> bool {
        matches!(key, Key::Char { shift: true, .. })
            || (self.page == Page::Letters && self.shift.on())
    }

    /// When [`Self::tick`] next has something to do.
    pub fn deadline(&self) -> Option<f64> {
        self.taps
            .values()
            .filter(|t| !t.checked)
            .map(|t| t.since + LONG_PRESS)
            .reduce(f64::min)
    }

    /// Opens the popup for a character key held past [`LONG_PRESS`], if
    /// `variants(key, shifted)` has any for it. `width`: the keyboard's, to
    /// keep the popup on screen. Returns whether a popup opened.
    pub fn tick(
        &mut self,
        now: f64,
        width: f32,
        variants: impl Fn(Key, bool) -> Vec<String>,
    ) -> bool {
        let due = self
            .taps
            .iter()
            .find(|(_, t)| !t.checked && now >= t.since + LONG_PRESS)
            .map(|(&touch, t)| (touch, t.key));
        let Some((touch, key)) = due else {
            return false;
        };
        let options = variants(key, self.shifted(key));
        let tap = self.taps.get_mut(&touch).expect("found above");
        tap.checked = true;
        if options.len() < 2 || self.popup.is_some() {
            return false;
        }
        // The base option sits under the finger.
        let span = options.len() as f32 * POPUP_CELL;
        let x0 = (tap.x - POPUP_CELL / 2.0).min(width - span).max(0.0);
        self.popup = Some(Popup {
            touch,
            key: tap.key,
            options,
            selected: 0,
            x0,
        });
        true
    }

    /// `at` in logical px; `now` in seconds, any epoch.
    pub fn down(&mut self, touch: i32, key: Key, at: (f32, f32), now: f64) -> Vec<Out> {
        let mut out = Vec::new();
        let mut held = Vec::new();
        match key {
            Key::Char { .. } | Key::Text(_) => {
                self.taps.insert(
                    touch,
                    Tap {
                        key,
                        since: now,
                        x: at.0,
                        checked: false,
                    },
                );
            }
            Key::Plain(code) => {
                let wrap = self.mod_codes();
                self.use_mods();
                out.extend(wrap.iter().map(|&c| Out::Press(c)));
                out.push(Out::Press(code));
                held.push(code);
                held.extend(wrap.iter().rev());
            }
            Key::Shift => self.shift.tap(&mut self.last_tap[3], now),
            Key::Mod(m) => self.mods[m as usize].tap(&mut self.last_tap[m as usize], now),
            Key::Page(p) => self.page = p,
            Key::NextLayout => out.push(Out::NextLayout),
            Key::Clip(clip) => {
                let mut wrap = vec![code::LEFTCTRL];
                if self.bar == Bar::Terminal {
                    wrap.push(code::LEFTSHIFT);
                }
                let letter = match clip {
                    Clip::Cut => code::X,
                    Clip::Copy => code::C,
                    Clip::Paste => code::V,
                };
                // Tapped, not held: a held paste would repeat.
                chord(&mut out, &wrap, letter);
            }
            Key::Cursor => {
                self.cursor = Some(CursorDrag {
                    touch,
                    anchor: at,
                    moved: false,
                });
            }
        }
        self.held.insert(touch, (key, held));
        out
    }

    /// The cursor key steps an arrow per travel, with latched modifiers and
    /// shift (so shift selects); a popup follows the finger.
    pub fn motion(&mut self, touch: i32, at: (f32, f32)) -> Vec<Out> {
        if let Some(p) = self.popup.as_mut().filter(|p| p.touch == touch) {
            let i = ((at.0 - p.x0) / POPUP_CELL).floor().max(0.0) as usize;
            p.selected = i.min(p.options.len() - 1);
            return Vec::new();
        }
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
            chord(&mut out, &wrap, arrow);
            drag.anchor = (drag.anchor.0 + step.0, drag.anchor.1 + step.1);
            drag.moved = true;
        }
        out
    }

    pub fn up(&mut self, touch: i32) -> Vec<Out> {
        let mut out = Vec::new();
        if self.cursor.as_ref().is_some_and(|d| d.touch == touch)
            && self.cursor.take().is_some_and(|d| d.moved)
        {
            self.shift.used();
            self.use_mods();
        }
        if let Some(tap) = self.taps.remove(&touch) {
            if self.popup.as_ref().is_some_and(|p| p.touch == touch) {
                let p = self.popup.take().expect("checked");
                out.push(Out::Text(p.options[p.selected].clone()));
            } else if let Key::Char { code, .. } = tap.key {
                let mut wrap = self.mod_codes();
                if self.shifted(tap.key) {
                    wrap.push(code::LEFTSHIFT);
                }
                chord(&mut out, &wrap, code);
            } else if let Key::Text(c) = tap.key {
                out.push(Out::Text(c.to_string()));
            }
            self.shift.used();
            self.use_mods();
        }
        if let Some((_, codes)) = self.held.remove(&touch) {
            out.extend(codes.into_iter().map(Out::Release));
        }
        out
    }

    /// Everything still held goes up and nothing types: hiding, or a cancel.
    pub fn release_all(&mut self) -> Vec<Out> {
        self.taps.clear();
        self.popup = None;
        self.cursor = None;
        self.held
            .drain()
            .flat_map(|(_, (_, codes))| codes)
            .map(Out::Release)
            .collect()
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
    use Out::{Press, Release, Text};

    const A: Key = Key::Char {
        code: 30,
        shift: false,
    };
    const O: (f32, f32) = (0.0, 0.0);

    fn ab(key: Key, _shifted: bool) -> Vec<String> {
        match key {
            A => vec!["a".into(), "à".into(), "å".into()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn a_character_types_on_release() {
        let mut k = Osk::default();
        assert_eq!(k.down(0, A, O, 0.0), []);
        assert_eq!(k.pressed().collect::<Vec<_>>(), [A]);
        assert_eq!(k.up(0), [Press(30), Release(30)]);
        assert_eq!(k.up(0), []);
    }

    #[test]
    fn backspace_goes_down_with_the_finger_so_it_repeats() {
        let mut k = Osk::default();
        assert_eq!(k.down(0, Key::Plain(code::BACKSPACE), O, 0.0), [Press(14)]);
        assert_eq!(k.up(0), [Release(14)]);
    }

    #[test]
    fn long_press_offers_variants_and_types_the_pick() {
        let mut k = Osk::default();
        k.down(0, A, (100.0, 50.0), 0.0);
        assert_eq!(k.deadline(), Some(LONG_PRESS));
        assert!(!k.tick(0.2, 400.0, ab));
        assert!(k.tick(0.5, 400.0, ab));
        assert_eq!(k.deadline(), None);
        let x0 = k.popup.as_ref().unwrap().x0;
        assert_eq!(x0, 100.0 - POPUP_CELL / 2.0);
        k.motion(0, (x0 + 2.5 * POPUP_CELL, 50.0));
        assert_eq!(k.popup.as_ref().unwrap().selected, 2);
        assert_eq!(k.up(0), [Text("å".into())]);
        assert!(k.popup.is_none());
    }

    #[test]
    fn long_press_without_variants_still_types_the_key() {
        let mut k = Osk::default();
        let q = Key::Char {
            code: 16,
            shift: false,
        };
        k.down(0, q, O, 0.0);
        assert!(!k.tick(1.0, 400.0, ab));
        assert_eq!(k.deadline(), None);
        assert_eq!(k.up(0), [Press(16), Release(16)]);
    }

    #[test]
    fn popup_stays_on_screen() {
        let mut k = Osk::default();
        k.down(0, A, (395.0, 0.0), 0.0);
        k.tick(1.0, 400.0, ab);
        assert_eq!(k.popup.as_ref().unwrap().x0, 400.0 - 3.0 * POPUP_CELL);
    }

    #[test]
    fn shift_once_wraps_the_next_letter_only() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, O, 0.0);
        k.up(0);
        k.down(1, A, O, 1.0);
        assert_eq!(k.up(1), [Press(42), Press(30), Release(30), Release(42)]);
        k.down(2, A, O, 2.0);
        assert_eq!(k.up(2), [Press(30), Release(30)]);
    }

    #[test]
    fn double_tap_locks_and_a_third_unlocks() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, O, 0.0);
        k.down(0, Key::Shift, O, 0.2);
        assert_eq!(k.shift, Latch::Locked);
        k.down(1, A, O, 1.0);
        k.up(1);
        assert_eq!(k.shift, Latch::Locked);
        k.down(0, Key::Shift, O, 2.0);
        assert_eq!(k.shift, Latch::Off);
    }

    #[test]
    fn slow_second_tap_turns_shift_off() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, O, 0.0);
        k.down(0, Key::Shift, O, 1.0);
        assert_eq!(k.shift, Latch::Off);
    }

    #[test]
    fn ctrl_latches_onto_plain_keys_too() {
        let mut k = Osk::default();
        k.down(0, Key::Mod(Mod::Ctrl), O, 0.0);
        assert_eq!(
            k.down(1, Key::Plain(code::BACKSPACE), O, 1.0),
            [Press(29), Press(14)]
        );
        assert_eq!(k.up(1), [Release(14), Release(29)]);
        assert_eq!(k.mod_latch(Mod::Ctrl), Latch::Off);
    }

    #[test]
    fn ctrl_then_a_letter_is_a_shortcut() {
        let mut k = Osk::default();
        k.down(0, Key::Mod(Mod::Ctrl), O, 0.0);
        k.down(1, A, O, 1.0);
        assert_eq!(k.up(1), [Press(29), Press(30), Release(30), Release(29)]);
    }

    #[test]
    fn shift_doesnt_touch_space_or_numbers() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, O, 0.0);
        assert_eq!(k.down(1, Key::Plain(code::SPACE), O, 1.0), [Press(57)]);
        assert_eq!(k.shift, Latch::Once);
        k.page = Page::Numbers;
        k.down(
            2,
            Key::Char {
                code: 2,
                shift: false,
            },
            O,
            1.5,
        );
        assert_eq!(k.up(2), [Press(2), Release(2)]);
    }

    #[test]
    fn symbols_carry_their_own_shift() {
        let mut k = Osk {
            page: Page::Symbols,
            ..Default::default()
        };
        k.down(
            0,
            Key::Char {
                code: 2,
                shift: true,
            },
            O,
            0.0,
        );
        assert_eq!(k.up(0), [Press(42), Press(2), Release(2), Release(42)]);
    }

    #[test]
    fn clipboard_chords_per_app() {
        let mut k = Osk::default();
        assert_eq!(
            k.down(0, Key::Clip(Clip::Copy), O, 0.0),
            [Press(29), Press(46), Release(46), Release(29)]
        );
        assert_eq!(
            k.down(1, Key::Clip(Clip::Cut), O, 0.0),
            [Press(29), Press(45), Release(45), Release(29)]
        );
        k.bar = Bar::Terminal;
        assert_eq!(
            k.down(2, Key::Clip(Clip::Paste), O, 0.0),
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
        assert_eq!(k.motion(1, O), [], "another finger");
        k.up(0);
        assert_eq!(k.motion(0, (300.0, 0.0)), []);
    }

    #[test]
    fn shift_selects_along_a_cursor_swipe_and_is_used_up() {
        let mut k = Osk::default();
        k.down(0, Key::Shift, O, 0.0);
        k.up(0);
        k.down(1, Key::Cursor, O, 1.0);
        assert_eq!(
            k.motion(1, (-13.0, 0.0)),
            [Press(42), Press(105), Release(105), Release(42)]
        );
        k.up(1);
        assert_eq!(k.shift, Latch::Off);
    }

    #[test]
    fn a_character_outside_the_keymap_is_committed() {
        let mut k = Osk::default();
        k.down(0, Key::Text('€'), O, 0.0);
        assert_eq!(k.up(0), [Text("€".into())]);
    }

    #[test]
    fn release_all_types_nothing_pending() {
        let mut k = Osk::default();
        k.down(0, A, O, 0.0);
        k.down(1, Key::Plain(code::BACKSPACE), O, 0.1);
        assert_eq!(k.release_all(), [Release(14)]);
        assert_eq!(k.up(0), []);
    }
}
