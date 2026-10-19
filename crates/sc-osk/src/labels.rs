//! Key labels from the seat's keymap, as the compositor sends it on
//! `wl_keyboard`.

use std::collections::HashMap;

use xkbcommon::xkb;

pub struct Keymap(xkb::Keymap);

impl Keymap {
    pub fn from_string(text: String) -> Option<Self> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_string(
            &ctx,
            text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        )
        .map(Keymap)
    }

    /// `layout` and `options` as in `[xkb]`; empty means xkbcommon's default.
    pub fn from_names(layout: &str, options: &str) -> Option<Self> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let options = (!options.is_empty()).then(|| options.to_string());
        xkb::Keymap::new_from_names(&ctx, "", "", layout, "", options, xkb::COMPILE_NO_FLAGS)
            .map(Keymap)
    }

    pub fn layout_names(&self) -> Vec<String> {
        (0..self.0.num_layouts())
            .map(|i| self.0.layout_get_name(i).to_string())
            .collect()
    }

    /// What evdev `code` types in `layout` at `level` ([`SHIFT`], [`ALTGR`]).
    /// `None` for dead keys and keys that type nothing.
    pub fn label(&self, code: u16, level: u8, layout: u32) -> Option<String> {
        let kc = xkb::Keycode::new(code as u32 + 8);
        let sym = *self
            .0
            .key_get_syms_by_level(kc, layout, level as u32)
            .first()?;
        let s = xkb::keysym_to_utf8(sym);
        let s = s.trim_end_matches('\0');
        (!s.is_empty() && !s.chars().any(char::is_control)).then(|| s.to_string())
    }
}

impl Keymap {
    /// Which key and level types each character in `layout`, the plainest
    /// level preferred. A Norwegian layout only reaches `@ $ [ ] { }` through
    /// AltGr, so stopping at shift leaves them untypeable.
    pub fn reverse(&self, layout: u32) -> HashMap<char, (u16, u8)> {
        let mut map = HashMap::new();
        for level in 0..=SHIFT | ALTGR {
            for code in 1..=LAST_TYPING_KEY {
                let Some(label) = self.label(code, level, layout) else {
                    continue;
                };
                let mut chars = label.chars();
                if let (Some(c), None) = (chars.next(), chars.next()) {
                    map.entry(c).or_insert((code, level));
                }
            }
        }
        map
    }
}

/// Keymap level bits, as `level` in [`Keymap::label`] and [`Keymap::reverse`].
pub const SHIFT: u8 = 1;
pub const ALTGR: u8 = 2;

/// Past this are keypad and media keys, which type the same characters.
const LAST_TYPING_KEY: u16 = 88;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_come_from_the_layout() {
        let k = Keymap::from_names("us,de", "").expect("xkeyboard-config");
        assert_eq!(k.layout_names().len(), 2);
        assert_eq!(k.label(16, 0, 0).as_deref(), Some("q"));
        assert_eq!(k.label(16, SHIFT, 0).as_deref(), Some("Q"));
        assert_eq!(k.label(21, 0, 0).as_deref(), Some("y"));
        assert_eq!(k.label(21, 0, 1).as_deref(), Some("z"));
        assert_eq!(k.label(26, 0, 1).as_deref(), Some("ü"));
        assert_eq!(k.label(3, SHIFT, 1).as_deref(), Some("\""));
        assert_eq!(k.label(1, 0, 0), None, "escape types nothing");
    }

    /// What [`reverse`] promises is that pressing those codes with those
    /// modifiers types the character. Level 2 being AltGr is a property of the
    /// key type, so check it against a real xkb state machine.
    ///
    /// [`reverse`]: Keymap::reverse
    #[test]
    fn altgr_really_types_the_altgr_level() {
        let k = Keymap::from_names("no", "").expect("xkeyboard-config");
        let reverse = k.reverse(0);
        for want in "@$[]{}".chars() {
            let (code, level) = reverse[&want];
            assert_eq!(level, ALTGR);
            let mut st = xkb::State::new(&k.0);
            // What the compositor sees from our uinput chord.
            st.update_key(
                xkb::Keycode::new(crate::grid::code::RIGHTALT as u32 + 8),
                xkb::KeyDirection::Down,
            );
            let kc = xkb::Keycode::new(code as u32 + 8);
            assert_eq!(st.key_get_utf8(kc), want.to_string(), "AltGr+{code}");
        }
    }

    /// A Norwegian layout hides most ASCII punctuation behind AltGr.
    #[test]
    fn reverse_reaches_altgr_symbols() {
        let k = Keymap::from_names("no", "").expect("xkeyboard-config");
        let no = k.reverse(0);
        for c in "@$[]{}".chars() {
            assert_eq!(no[&c].1, ALTGR, "{c}");
        }
        for c in "\\|<".chars() {
            assert_eq!(no[&c].1, 0, "{c}");
        }
        // Dead keys type nothing on their own; they stay committed text.
        assert!(!no.contains_key(&'^') && !no.contains_key(&'~'));
    }

    #[test]
    fn xkb_caps_a_keymap_at_four_groups() {
        let k = Keymap::from_names("no,us,de,fr,se,ru,gr", "").expect("xkeyboard-config");
        assert_eq!(k.layout_names().len(), 4);
    }

    #[test]
    fn reverse_finds_shifted_symbols_per_layout() {
        let k = Keymap::from_names("us,de", "").expect("xkeyboard-config");
        let us = k.reverse(0);
        assert_eq!(us[&'('], (10, SHIFT));
        assert_eq!(us[&'-'], (12, 0));
        assert_eq!(us.get(&'€'), None);
        let de = k.reverse(1);
        assert_eq!(de[&'('], (9, SHIFT));
        assert_eq!(de[&'z'], (21, 0));
        assert_eq!(de[&'@'], (16, ALTGR), "AltGr is in reach");
    }
}
