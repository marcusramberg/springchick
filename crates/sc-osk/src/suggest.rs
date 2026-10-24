//! The word being typed, and the strip of completions over the bar.
//!
//! Typing goes out as uinput key events, so the app knows the text and we do
//! not. The word has to be rebuilt from what we sent, and any sign of having
//! lost track of it — a shortcut, an arrow key, a word we did not type —
//! clears it rather than completing against a guess.

use crate::grid::{code, Mod};
use crate::labels::{ALTGR, SHIFT};
use crate::osk::Out;
use sc_layout::Rect;

/// Completions offered at once. More than three stops being readable.
pub const SLOTS: usize = 3;

#[derive(Default)]
pub struct Tracker {
    word: String,
    /// The word before `word`, for predicting the next one. Gone whenever we
    /// stop knowing where the cursor is.
    prev: Option<String>,
    shift: bool,
    altgr: bool,
    /// Held ctrl/alt/super: what we type then is a shortcut, not a word.
    mods: usize,
}

impl Tracker {
    pub fn word(&self) -> &str {
        &self.word
    }

    pub fn prev(&self) -> Option<&str> {
        self.prev.as_deref()
    }

    pub fn clear(&mut self) {
        self.word.clear();
        self.prev = None;
    }

    /// A suggestion replaced the word in progress: it is the word before the
    /// next one now.
    pub fn accepted(&mut self, word: &str) {
        self.word.clear();
        self.prev = Some(word.to_string());
    }

    /// Follows `outs` and returns a word that just ended, for learning.
    pub fn feed(
        &mut self,
        outs: &[Out],
        label: &dyn Fn(u16, u8) -> Option<String>,
    ) -> Option<String> {
        let mut done = None;
        let mut lost = false;
        let mut finish = |w: &mut String| {
            if !w.is_empty() {
                done = Some(std::mem::take(w));
            }
        };
        for out in outs {
            match out {
                &Out::Press(c) if c == code::LEFTSHIFT => self.shift = true,
                &Out::Release(c) if c == code::LEFTSHIFT => self.shift = false,
                &Out::Press(c) if c == code::RIGHTALT => self.altgr = true,
                &Out::Release(c) if c == code::RIGHTALT => self.altgr = false,
                &Out::Press(c) if Mod::ALL.iter().any(|m| m.code() == c) => {
                    self.mods += 1;
                    self.word.clear();
                    lost = true;
                }
                &Out::Release(c) if Mod::ALL.iter().any(|m| m.code() == c) => {
                    self.mods = self.mods.saturating_sub(1);
                }
                &Out::Press(_) if self.mods > 0 => {
                    self.word.clear();
                    lost = true;
                }
                &Out::Press(code::BACKSPACE) => {
                    // Backspacing into the previous word loses its text, so
                    // stop completing rather than complete the wrong stem.
                    if self.word.pop().is_none() {
                        lost = true;
                    }
                }
                &Out::Press(code::SPACE | code::ENTER | code::TAB) => finish(&mut self.word),
                &Out::Press(c) => {
                    let level = (u8::from(self.shift) * SHIFT) | (u8::from(self.altgr) * ALTGR);
                    match label(c, level) {
                        Some(text) if is_word(&text) => self.word.push_str(&text),
                        // Punctuation ends the word; a cursor key also loses
                        // where we are.
                        Some(_) => finish(&mut self.word),
                        None => {
                            finish(&mut self.word);
                            lost = true;
                        }
                    }
                }
                Out::Text(text) if is_word(text) => self.word.push_str(text),
                Out::Text(_) => finish(&mut self.word),
                Out::Release(_) | Out::NextLayout => {}
            }
        }
        if lost {
            self.prev = None;
        }
        if let Some(w) = &done {
            self.prev = Some(w.clone());
        }
        done
    }
}

fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphabetic() || c == '\'' || c == '-')
}

/// Equal cells across `area`, one per word.
pub fn strip(words: Vec<String>, area: Rect) -> Vec<(String, Rect)> {
    let w = area.w / words.len().max(1) as f32;
    words
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            (
                s,
                Rect {
                    x: area.x + i as f32 * w,
                    y: area.y,
                    w,
                    h: area.h,
                },
            )
        })
        .collect()
}

pub fn hit(cells: &[(String, Rect)], x: f32, y: f32) -> Option<usize> {
    cells.iter().position(|(_, r)| r.contains(x, y))
}

/// Swaps `typed` for `word` and adds a space: backspaces over what is there,
/// then types the word as real keys, so it lands in a terminal too.
///
/// Going through backspace rather than the input method is also what an
/// autocorrect would need, if one is ever wanted.
pub fn accept(typed: &str, word: &str, find: &dyn Fn(char) -> Option<(u16, u8)>) -> Vec<Out> {
    let mut out = Vec::new();
    for _ in 0..typed.chars().count() {
        out.push(Out::Press(code::BACKSPACE));
        out.push(Out::Release(code::BACKSPACE));
    }
    for c in word.chars() {
        match find(c) {
            Some((code, level)) => {
                let mut wrap = Vec::new();
                if level & SHIFT != 0 {
                    wrap.push(code::LEFTSHIFT);
                }
                if level & ALTGR != 0 {
                    wrap.push(code::RIGHTALT);
                }
                out.extend(wrap.iter().map(|&c| Out::Press(c)));
                out.push(Out::Press(code));
                out.push(Out::Release(code));
                out.extend(wrap.iter().rev().map(|&c| Out::Release(c)));
            }
            // Not in the keymap: the input method is the only way to send it.
            None => out.push(Out::Text(c.to_string())),
        }
    }
    out.push(Out::Press(code::SPACE));
    out.push(Out::Release(code::SPACE));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny US-like keymap: codes 30.. are "asdf", 2 is '1'/'!'.
    fn label(code: u16, level: u8) -> Option<String> {
        let c = match code {
            2 => return Some(if level & SHIFT != 0 { "!" } else { "1" }.into()),
            16 => 'q',
            30 => 'a',
            31 => 's',
            32 => 'd',
            52 => return Some(".".into()),
            _ => return None,
        };
        Some(if level & SHIFT != 0 {
            c.to_uppercase().to_string()
        } else {
            c.to_string()
        })
    }

    fn find(c: char) -> Option<(u16, u8)> {
        match c {
            'q' => Some((16, 0)),
            'a' => Some((30, 0)),
            'A' => Some((30, SHIFT)),
            's' => Some((31, 0)),
            'd' => Some((32, 0)),
            _ => None,
        }
    }

    fn tap(code: u16) -> Vec<Out> {
        vec![Out::Press(code), Out::Release(code)]
    }

    #[test]
    fn builds_the_word_from_the_keys_we_sent() {
        let mut t = Tracker::default();
        for c in [30, 31, 32] {
            assert_eq!(t.feed(&tap(c), &label), None);
        }
        assert_eq!(t.word(), "asd");
    }

    #[test]
    fn shift_and_altgr_reach_the_right_level() {
        let mut t = Tracker::default();
        t.feed(
            &[
                Out::Press(code::LEFTSHIFT),
                Out::Press(30),
                Out::Release(30),
                Out::Release(code::LEFTSHIFT),
            ],
            &label,
        );
        t.feed(&tap(31), &label);
        assert_eq!(t.word(), "As");
    }

    #[test]
    fn space_ends_the_word_and_hands_it_over_to_learn() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        t.feed(&tap(31), &label);
        assert_eq!(t.feed(&tap(code::SPACE), &label).as_deref(), Some("as"));
        assert_eq!(t.word(), "");
    }

    #[test]
    fn the_finished_word_becomes_prev_until_we_lose_the_cursor() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        t.feed(&tap(code::SPACE), &label);
        assert_eq!(t.prev(), Some("a"));
        t.feed(&tap(31), &label);
        assert_eq!(t.prev(), Some("a"), "still, while the next word is typed");
        t.feed(&tap(52), &label);
        assert_eq!(t.prev(), Some("s"), "punctuation keeps the sentence");
        t.feed(&tap(code::LEFT), &label);
        assert_eq!(t.prev(), None);
        t.accepted("asd");
        assert_eq!((t.prev(), t.word()), (Some("asd"), ""));
        t.feed(&tap(code::BACKSPACE), &label);
        assert_eq!(t.prev(), None, "backspaced into the app's text");
    }

    #[test]
    fn punctuation_ends_the_word_too() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        assert_eq!(
            t.feed(&tap(52), &label).as_deref(),
            Some("a"),
            "'.' ends it"
        );
        assert_eq!(t.word(), "");
    }

    #[test]
    fn backspace_walks_back_one_character() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        t.feed(&tap(31), &label);
        t.feed(&tap(code::BACKSPACE), &label);
        assert_eq!(t.word(), "a");
    }

    /// Past the start of our word is the app's text, which we never saw.
    #[test]
    fn backspacing_past_the_start_gives_up_rather_than_guesses() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        t.feed(&tap(code::BACKSPACE), &label);
        t.feed(&tap(code::BACKSPACE), &label);
        assert_eq!(t.word(), "");
    }

    #[test]
    fn a_shortcut_is_not_a_word() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        t.feed(
            &[
                Out::Press(code::LEFTCTRL),
                Out::Press(30),
                Out::Release(30),
                Out::Release(code::LEFTCTRL),
            ],
            &label,
        );
        assert_eq!(t.word(), "", "ctrl+a selected all, it did not type 'a'");
    }

    #[test]
    fn an_arrow_key_ends_the_word() {
        let mut t = Tracker::default();
        t.feed(&tap(30), &label);
        t.feed(&tap(code::LEFT), &label);
        assert_eq!(t.word(), "");
    }

    #[test]
    fn committed_text_counts_when_it_is_a_word() {
        let mut t = Tracker::default();
        t.feed(&[Out::Text("ø".into())], &label);
        t.feed(&tap(30), &label);
        assert_eq!(t.word(), "øa");
        t.feed(&[Out::Text("€".into())], &label);
        assert_eq!(t.word(), "");
    }

    #[test]
    fn accept_backspaces_then_types_the_word_and_a_space() {
        let outs = accept("as", "asd", &find);
        let bs = outs
            .iter()
            .filter(|o| **o == Out::Press(code::BACKSPACE))
            .count();
        assert_eq!(bs, 2, "one per character typed");
        assert_eq!(outs[outs.len() - 2], Out::Press(code::SPACE));
        let typed: Vec<u16> = outs
            .iter()
            .filter_map(|o| match o {
                Out::Press(c) if [16, 30, 31, 32].contains(c) => Some(*c),
                _ => None,
            })
            .collect();
        assert_eq!(typed, [30, 31, 32]);
    }

    #[test]
    fn accept_shifts_for_a_capital() {
        let outs = accept("a", "A", &find);
        let i = outs.iter().position(|o| *o == Out::Press(30)).unwrap();
        assert_eq!(outs[i - 1], Out::Press(code::LEFTSHIFT));
        assert_eq!(outs[i + 2], Out::Release(code::LEFTSHIFT));
    }

    #[test]
    fn accept_falls_back_to_the_input_method_off_the_keymap() {
        let outs = accept("", "aø", &find);
        assert!(outs.contains(&Out::Text("ø".into())));
    }

    #[test]
    fn strip_splits_the_area_and_hits() {
        let area = Rect {
            x: 10.0,
            y: 0.0,
            w: 300.0,
            h: 40.0,
        };
        let cells = strip(["a", "b", "c"].map(String::from).to_vec(), area);
        assert_eq!(cells.len(), 3);
        assert_eq!(hit(&cells, 15.0, 20.0), Some(0));
        assert_eq!(hit(&cells, 215.0, 20.0), Some(2));
        assert_eq!(hit(&cells, 15.0, 60.0), None);
    }
}
