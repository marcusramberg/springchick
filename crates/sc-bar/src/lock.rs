//! The lock screen's own part: the cover (date, clock, notifications) that
//! slides up off the passcode pad, and the pad. `main` owns the surface and
//! asks PAM.

use sc_anim::Spring;

use crate::center::{rubber_band, spring, View};
use crate::panel::BAR_H;

pub const KEY_DIAMETER: f32 = 78.0;
const KEY_SPACING: f32 = 100.0;
const KEY_PITCH: f32 = 96.0;
/// From the title down to the dots, and to the first row of keys.
pub const DOTS: f32 = 40.0;
const KEYS: f32 = 130.0;
const PAD_HEIGHT: f32 = KEYS + 3.0 * KEY_PITCH + KEY_DIAMETER / 2.0;
const MAX_PIN: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Digit(u8),
    /// Deletes the last digit, or goes back to the cover when there is none.
    Back,
    Submit,
}

pub struct Lock {
    /// How far the cover has slid up, px; the screen's height shows the pad.
    reveal: Spring,
    pin: String,
    /// PAM has the PIN.
    checking: bool,
    shake: Spring,
    pressed: Option<Key>,
    /// Where the finger on the cover went down, and the cover's place then.
    drag: Option<(f32, f32)>,
}

impl Default for Lock {
    fn default() -> Self {
        Lock {
            reveal: spring(0.0, 0.42, 1.0),
            pin: String::new(),
            checking: false,
            // Underdamped: the dots wiggle after a wrong PIN.
            shake: spring(0.0, 0.12, 0.25),
            pressed: None,
            drag: None,
        }
    }
}

impl Lock {
    pub fn reveal(&self) -> f32 {
        self.reveal.value
    }

    /// The cover is down, and staying there.
    pub fn covered(&self) -> bool {
        self.drag.is_none() && self.reveal.target == 0.0 && self.reveal.value.abs() < 1.0
    }

    /// The pad is up, or on its way.
    pub fn pad_up(&self, h: f32) -> bool {
        self.reveal.target >= h
    }

    pub fn show_pad(&mut self, h: f32) {
        self.reveal.target = h;
    }

    pub fn digits(&self) -> usize {
        self.pin.len()
    }

    pub fn checking(&self) -> bool {
        self.checking
    }

    pub fn shake(&self) -> f32 {
        self.shake.value
    }

    pub fn pressed(&self) -> Option<Key> {
        self.pressed
    }

    pub fn is_animating(&self) -> bool {
        self.drag.is_none() && !(self.reveal.is_settled() && self.shake.is_settled())
    }

    pub fn step(&mut self, dt: f32) {
        if self.drag.is_none() {
            self.reveal.step(dt);
        }
        self.shake.step(dt);
    }

    pub fn touch_down(&mut self, p: (f32, f32), w: f32, h: f32) {
        if self.pad_up(h) {
            if !self.checking {
                self.pressed = key_at(p, w, h);
            }
        } else {
            self.drag = Some((p.1, self.reveal.value));
        }
    }

    pub fn touch_move(&mut self, p: (f32, f32), w: f32, h: f32) {
        if let Some((start, from)) = self.drag {
            let up = from + start - p.1;
            self.reveal.value = if up < 0.0 {
                -rubber_band(-up, h)
            } else {
                up.min(h)
            };
        }
        if self.pressed.is_some() && key_at(p, w, h) != self.pressed {
            self.pressed = None;
        }
    }

    /// The finger lifted, moving at `vy` px/s; returns a PIN to check.
    pub fn touch_up(&mut self, vy: f32, h: f32) -> Option<String> {
        if self.drag.take().is_some() {
            let up = vy < -300.0 || (vy < 300.0 && self.reveal.value > h * 0.25);
            self.reveal.velocity = -vy;
            self.reveal.target = if up { h } else { 0.0 };
            return None;
        }
        match self.pressed.take()? {
            Key::Digit(d) if self.pin.len() < MAX_PIN => self.pin.push(char::from(b'0' + d)),
            Key::Digit(_) => {}
            Key::Back if self.pin.is_empty() => self.reveal.target = 0.0,
            Key::Back => {
                self.pin.pop();
            }
            Key::Submit if !self.pin.is_empty() => {
                self.checking = true;
                return Some(self.pin.clone());
            }
            Key::Submit => {}
        }
        None
    }

    /// The compositor took the finger away.
    pub fn cancel_touch(&mut self) {
        self.drag = None;
        self.pressed = None;
    }

    pub fn failed(&mut self) {
        self.checking = false;
        self.pin.clear();
        self.shake.velocity = 900.0;
    }
}

fn k(w: f32, h: f32) -> f32 {
    View { top: 0.0, w, h }.k()
}

/// The pad's title line: the pad sits in the middle of the screen.
pub fn pad_top(w: f32, h: f32) -> f32 {
    ((h - PAD_HEIGHT * k(w, h)) / 2.0).max(BAR_H + 24.0)
}

pub fn key_for(row: usize, column: usize) -> Key {
    match (row, column) {
        (3, 0) => Key::Back,
        (3, 1) => Key::Digit(0),
        (3, _) => Key::Submit,
        _ => Key::Digit((row * 3 + column + 1) as u8),
    }
}

pub fn key_center(row: usize, column: usize, w: f32, h: f32) -> (f32, f32) {
    let k = k(w, h);
    (
        w / 2.0 + (column as f32 - 1.0) * KEY_SPACING * k,
        pad_top(w, h) + (KEYS + row as f32 * KEY_PITCH) * k,
    )
}

fn key_at(p: (f32, f32), w: f32, h: f32) -> Option<Key> {
    let reach = (KEY_DIAMETER / 2.0 + 6.0) * k(w, h);
    (0..4)
        .flat_map(|row| (0..3).map(move |column| (row, column)))
        .find(|&(row, column)| {
            let c = key_center(row, column, w, h);
            (p.0 - c.0).hypot(p.1 - c.1) < reach
        })
        .map(|(row, column)| key_for(row, column))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 390.0;
    const H: f32 = 844.0;

    fn tap(lock: &mut Lock, key: (usize, usize)) -> Option<String> {
        lock.touch_down(key_center(key.0, key.1, W, H), W, H);
        lock.touch_up(0.0, H)
    }

    #[test]
    fn swipe_reveals_the_pad_and_the_pin_is_submitted() {
        let mut lock = Lock::default();
        lock.touch_down((195.0, 800.0), W, H);
        lock.touch_move((195.0, 500.0), W, H);
        assert_eq!(lock.touch_up(-800.0, H), None);
        assert!(lock.pad_up(H));

        tap(&mut lock, (0, 0));
        tap(&mut lock, (1, 1));
        tap(&mut lock, (3, 1));
        tap(&mut lock, (3, 0));
        tap(&mut lock, (2, 2));
        assert_eq!(tap(&mut lock, (3, 2)), Some("159".to_string()));

        // Nothing more goes in while PAM has it.
        assert_eq!(tap(&mut lock, (0, 0)), None);
        lock.failed();
        assert!(lock.pin.is_empty() && !lock.checking);
    }

    #[test]
    fn the_cover_follows_the_finger_both_ways() {
        let mut lock = Lock::default();
        lock.touch_down((195.0, 800.0), W, H);
        lock.touch_move((195.0, 800.0 - H / 2.0), W, H);
        assert_eq!(lock.reveal(), H / 2.0);
        lock.touch_move((195.0, 900.0), W, H);
        assert!(lock.reveal() < 0.0 && lock.reveal() > -100.0);
    }

    #[test]
    fn a_short_slow_swipe_falls_back() {
        let mut lock = Lock::default();
        lock.touch_down((195.0, 800.0), W, H);
        lock.touch_move((195.0, 760.0), W, H);
        lock.touch_up(0.0, H);
        assert!(!lock.pad_up(H));
        // Back down, the cover is the list's again, wherever the spring stopped.
        for _ in 0..600 {
            lock.step(1.0 / 120.0);
        }
        assert!(lock.covered());
    }

    #[test]
    fn cancel_on_an_empty_pin_goes_back_to_the_cover() {
        let mut lock = Lock::default();
        lock.show_pad(H);
        tap(&mut lock, (3, 0));
        assert!(!lock.pad_up(H));
    }

    #[test]
    fn nothing_is_submitted_without_a_digit() {
        let mut lock = Lock::default();
        lock.show_pad(H);
        assert_eq!(tap(&mut lock, (3, 2)), None);
        assert!(!lock.checking);
    }
}
