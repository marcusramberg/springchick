//! Key labels from the seat's keymap, as the compositor sends it on
//! `wl_keyboard`.

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

    /// What evdev `code` types in `layout`, shifted or not. `None` for dead
    /// keys and keys that type nothing.
    pub fn label(&self, code: u16, shift: bool, layout: u32) -> Option<String> {
        let kc = xkb::Keycode::new(code as u32 + 8);
        let sym = *self
            .0
            .key_get_syms_by_level(kc, layout, shift as u32)
            .first()?;
        let s = xkb::keysym_to_utf8(sym);
        let s = s.trim_end_matches('\0');
        (!s.is_empty() && !s.chars().any(char::is_control)).then(|| s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_come_from_the_layout() {
        let k = Keymap::from_names("us,de", "").expect("xkeyboard-config");
        assert_eq!(k.layout_names().len(), 2);
        assert_eq!(k.label(16, false, 0).as_deref(), Some("q"));
        assert_eq!(k.label(16, true, 0).as_deref(), Some("Q"));
        assert_eq!(k.label(21, false, 0).as_deref(), Some("y"));
        assert_eq!(k.label(21, false, 1).as_deref(), Some("z"));
        assert_eq!(k.label(26, false, 1).as_deref(), Some("ü"));
        assert_eq!(k.label(3, true, 1).as_deref(), Some("\""));
        assert_eq!(k.label(1, false, 0), None, "escape types nothing");
    }
}
