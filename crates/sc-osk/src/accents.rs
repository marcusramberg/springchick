//! What a long press offers, from the springboard prototype's layouts. Keyed
//! by what the key types, so it follows the active keymap.

const TABLE: &[(&str, &str)] = &[
    ("a", "à á â ä æ ã å ā ą"),
    ("c", "ç ć č"),
    ("e", "è é ê ë ē ė ę"),
    ("g", "ğ ġ ģ ґ"),
    ("i", "î ï í ī į ì"),
    ("l", "ł"),
    ("n", "ñ ń"),
    ("o", "ô ö ò ó œ ø ō õ"),
    ("s", "ß ś š"),
    ("u", "û ü ù ú ū"),
    ("y", "ÿ ý"),
    ("z", "ž ź ż"),
    // Cyrillic and Greek: letters xkb puts out of reach of the three rows, or
    // only behind a dead key.
    ("е", "ё"),
    ("α", "ά"),
    ("ε", "έ"),
    ("η", "ή"),
    ("ι", "ί ϊ ΐ"),
    ("ο", "ό"),
    ("υ", "ύ ϋ ΰ"),
    ("ω", "ώ"),
    ("-", "– — •"),
    ("$", "€ £ ¥ ¢ ₽ ₩"),
    ("&", "§"),
    ("\"", "„ “ ” « »"),
    (".", "…"),
    ("?", "¿"),
    ("!", "¡"),
    ("'", "‘ ’ ‚ `"),
    ("%", "‰"),
];

/// `base` first, then its variants, cased like `base`. Empty without variants.
pub fn variants(base: &str) -> Vec<String> {
    let lower = base.to_lowercase();
    let upper = base != lower;
    let Some((_, list)) = TABLE.iter().find(|(k, _)| *k == lower) else {
        return Vec::new();
    };
    std::iter::once(base.to_string())
        .chain(list.split(' ').map(|v| {
            if upper {
                v.to_uppercase()
            } else {
                v.to_string()
            }
        }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_first_then_variants_in_its_case() {
        assert_eq!(variants("o")[..3], ["o", "ô", "ö"]);
        assert!(variants("a").contains(&"å".to_string()));
        assert_eq!(variants("A")[..3], ["A", "À", "Á"]);
        assert_eq!(variants("?"), ["?", "¿"]);
        assert!(variants("q").is_empty());
    }
}
