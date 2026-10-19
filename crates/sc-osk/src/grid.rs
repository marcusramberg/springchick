//! Key grids. Keys are evdev codes and their labels come from the keymap, so
//! one grid serves every xkb layout: a German keymap shows QWERTZ and ü.

pub use sc_layout::Rect;

pub mod code {
    pub const ESC: u16 = 1;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const ENTER: u16 = 28;
    pub const LEFTCTRL: u16 = 29;
    pub const LEFTSHIFT: u16 = 42;
    pub const X: u16 = 45;
    pub const C: u16 = 46;
    pub const V: u16 = 47;
    pub const LEFTALT: u16 = 56;
    pub const SPACE: u16 = 57;
    pub const UP: u16 = 103;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const DOWN: u16 = 108;
    pub const LEFTMETA: u16 = 125;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Letters,
    Numbers,
    Symbols,
    Emoji,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mod {
    Ctrl,
    Alt,
    Super,
}

impl Mod {
    pub const ALL: [Mod; 3] = [Mod::Ctrl, Mod::Alt, Mod::Super];

    pub fn code(self) -> u16 {
        match self {
            Mod::Ctrl => code::LEFTCTRL,
            Mod::Alt => code::LEFTALT,
            Mod::Super => code::LEFTMETA,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clip {
    Cut,
    Copy,
    Paste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A character key; `shift` types it shifted.
    Char {
        code: u16,
        shift: bool,
    },
    /// A character the keymap can't type, committed as text.
    Text(char),
    /// Scrolls the emoji picker to a category ([`crate::emoji::ICONS`]).
    Category(u8),
    /// Space, return, backspace, tab, arrows: modifiers apply, shift doesn't.
    Plain(u16),
    Shift,
    Page(Page),
    NextLayout,
    Mod(Mod),
    /// The app's own shortcut: Ctrl+X/C/V, Ctrl+Shift in terminals.
    Clip(Clip),
    /// Swiped like a trackpad: arrow presses in the direction of travel.
    Cursor,
}

/// The row above the keys, chosen by the app in front.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bar {
    #[default]
    Default,
    Terminal,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub key: Key,
    pub label: String,
    /// The whole cell, which is also the touch target.
    pub rect: Rect,
}

/// What the grid needs from the keymap and the shell.
pub struct Labels<'a> {
    /// `(evdev code, shifted)` → text, `None` for keys that type nothing.
    pub key: &'a dyn Fn(u16, bool) -> Option<String>,
    /// The key and shift state that type a character, if any.
    pub find: &'a dyn Fn(char) -> Option<(u16, bool)>,
    /// Shown on the space bar.
    pub layout_name: &'a str,
    pub layouts: usize,
}

/// Letter rows: these ranges, keeping keys whose label is a letter.
const LETTER_ROWS: [std::ops::RangeInclusive<u16>; 3] = [16..=27, 30..=40, 44..=53];
/// As iOS lays them out; each types through the keymap where it can.
const NUMBERS: [&str; 3] = ["1234567890", "-/:;()$&@\"", ".,?!'"];
const SYMBOLS: [&str; 3] = ["[]{}#%^*+=", "_\\|~<>€$£•", ".,?!'"];

/// Bar height as a fraction of a key row.
const BAR_ROW: f32 = 0.8;
const PAD: f32 = 3.0;

fn is_letter(s: &str) -> bool {
    !s.is_empty() && s.chars().all(char::is_alphabetic)
}

/// Lays out `page` in `width`×`height`; `upper` shows letters shifted.
pub fn layout(
    page: Page,
    bar: Bar,
    upper: bool,
    width: f32,
    height: f32,
    labels: &Labels<'_>,
) -> Vec<Placed> {
    let label = |key: Key| -> String {
        match key {
            Key::Char { code, shift } => (labels.key)(code, shift).unwrap_or_default(),
            Key::Text(c) => c.to_string(),
            Key::Category(i) => crate::emoji::ICONS[i as usize].to_string(),
            Key::Page(Page::Emoji) => "😀".into(),
            Key::Plain(code::SPACE) => labels.layout_name.to_string(),
            Key::Plain(code::ENTER) => "return".into(),
            Key::Plain(code::BACKSPACE) => "⌫".into(),
            Key::Plain(code::TAB) => "tab".into(),
            Key::Plain(code::ESC) => "esc".into(),
            Key::Plain(_) => String::new(),
            Key::Shift => "⇧".into(),
            Key::Page(Page::Letters) => "ABC".into(),
            Key::Page(Page::Numbers) => "123".into(),
            Key::Page(Page::Symbols) => "#+=".into(),
            Key::NextLayout => "lang".into(),
            Key::Mod(Mod::Ctrl) => "ctrl".into(),
            Key::Mod(Mod::Alt) => "alt".into(),
            Key::Mod(Mod::Super) => "super".into(),
            Key::Clip(Clip::Cut) => "cut".into(),
            Key::Clip(Clip::Copy) => "copy".into(),
            Key::Clip(Clip::Paste) => "paste".into(),
            Key::Cursor => "← ↑↓ →".into(),
        }
    };
    let letters = |codes: std::ops::RangeInclusive<u16>| -> Vec<Key> {
        codes
            .filter(|&c| is_letter(&(labels.key)(c, false).unwrap_or_default()))
            .map(|code| Key::Char { code, shift: false })
            .collect()
    };
    let symbols = |row: &str| -> Vec<Key> {
        row.chars()
            .map(|c| match (labels.find)(c) {
                Some((code, shift)) => Key::Char { code, shift },
                None => Key::Text(c),
            })
            .collect()
    };

    let (rows, row3_left): ([Vec<Key>; 3], Key) = match page {
        Page::Letters => (LETTER_ROWS.map(letters), Key::Shift),
        Page::Numbers => (NUMBERS.map(symbols), Key::Page(Page::Symbols)),
        Page::Symbols => (SYMBOLS.map(symbols), Key::Page(Page::Numbers)),
        // The picker draws over the key rows; see `emoji_area`.
        Page::Emoji => (Default::default(), Key::Page(Page::Numbers)),
    };

    let avail = width - 2.0 * PAD;
    let row_h = height / (4.0 + BAR_ROW);
    let bar_h = row_h * BAR_ROW;
    let units = rows[0]
        .len()
        .max(rows[1].len())
        .max(rows[2].len() + 3)
        .max(10) as f32;
    let unit = avail / units;
    let mut out = Vec::new();
    let mut put = |key: Key, x: f32, y: f32, w: f32, h: f32| {
        out.push(Placed {
            key,
            label: label(key),
            rect: Rect { x, y, w, h },
        })
    };

    let bar_keys: &[Key] = match bar {
        Bar::Terminal => &[
            Key::Plain(code::ESC),
            Key::Plain(code::TAB),
            Key::Mod(Mod::Ctrl),
            Key::Mod(Mod::Alt),
            Key::Mod(Mod::Super),
            Key::Cursor,
            Key::Clip(Clip::Copy),
            Key::Clip(Clip::Paste),
        ],
        Bar::Default => &[
            Key::Plain(code::TAB),
            Key::Mod(Mod::Ctrl),
            Key::Cursor,
            Key::Clip(Clip::Cut),
            Key::Clip(Clip::Copy),
            Key::Clip(Clip::Paste),
        ],
    };
    // The cursor key is a swipe target: twice the width.
    let weight = |k: Key| if k == Key::Cursor { 2.0 } else { 1.0 };
    let bw = avail / bar_keys.iter().map(|&k| weight(k)).sum::<f32>();
    let mut x = PAD;
    for &k in bar_keys {
        put(k, x, 0.0, bw * weight(k), bar_h);
        x += bw * weight(k);
    }

    if page == Page::Emoji {
        let y = bar_h + 3.0 * row_h;
        let side = unit * 1.5;
        put(Key::Page(Page::Letters), PAD, y, side, row_h);
        let n = crate::emoji::ICONS.len();
        let cw = (avail - 2.0 * side) / n as f32;
        for i in 0..n {
            put(
                Key::Category(i as u8),
                PAD + side + i as f32 * cw,
                y,
                cw,
                row_h,
            );
        }
        put(
            Key::Plain(code::BACKSPACE),
            PAD + avail - side,
            y,
            side,
            row_h,
        );
        return out;
    }

    let mut y = bar_h;
    for (i, row) in rows.iter().enumerate() {
        // iOS stretches the few punctuation keys between #+= and backspace.
        let w = if i == 2 && page != Page::Letters {
            (avail - unit * 3.5) / row.len() as f32
        } else {
            unit
        };
        let x0 = PAD + (avail - row.len() as f32 * w) / 2.0;
        if i == 2 {
            put(row3_left, PAD, y, unit * 1.5, row_h);
            put(
                Key::Plain(code::BACKSPACE),
                PAD + avail - unit * 1.5,
                y,
                unit * 1.5,
                row_h,
            );
        }
        for (j, &k) in row.iter().enumerate() {
            put(k, x0 + j as f32 * w, y, w, row_h);
        }
        y += row_h;
    }

    let page_key = Key::Page(if page == Page::Letters {
        Page::Numbers
    } else {
        Page::Letters
    });
    let side = unit * 1.5;
    let mut x = PAD;
    put(page_key, x, y, side, row_h);
    x += side;
    if page != Page::Emoji {
        put(Key::Page(Page::Emoji), x, y, side, row_h);
        x += side;
    }
    if labels.layouts > 1 {
        put(Key::NextLayout, x, y, side, row_h);
        x += side;
    }
    let ret = unit * 2.5;
    put(Key::Plain(code::SPACE), x, y, PAD + avail - ret - x, row_h);
    put(Key::Plain(code::ENTER), PAD + avail - ret, y, ret, row_h);

    if upper && page == Page::Letters {
        for p in &mut out {
            if let Key::Char { code, .. } = p.key {
                p.label = (labels.key)(code, true).unwrap_or_default();
            }
        }
    }
    out
}

/// Where the emoji picker goes: the three key rows.
pub fn emoji_area(width: f32, height: f32) -> Rect {
    let row_h = height / (4.0 + BAR_ROW);
    Rect {
        x: PAD,
        y: row_h * BAR_ROW,
        w: width - 2.0 * PAD,
        h: 3.0 * row_h,
    }
}

pub fn hit(grid: &[Placed], x: f32, y: f32) -> Option<Key> {
    grid.iter().find(|p| p.rect.contains(x, y)).map(|p| p.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small US (and with `de`, German) keymap stand-in.
    fn keymap(de: bool) -> impl Fn(u16, bool) -> Option<String> {
        move |c, shift| {
            let us = "  1234567890-=  qwertyuiop[]  asdfghjkl;'` \\zxcvbnm,./";
            let ch = if de {
                match c {
                    21 => 'z',
                    44 => 'y',
                    26 => 'ü',
                    39 => 'ö',
                    40 => 'ä',
                    12 => 'ß',
                    _ => us.chars().nth(c as usize)?,
                }
            } else {
                us.chars().nth(c as usize)?
            };
            if ch == ' ' {
                return None;
            }
            Some(if shift {
                ch.to_uppercase().collect()
            } else {
                ch.to_string()
            })
        }
    }

    fn grid(page: Page, de: bool, layouts: usize) -> Vec<Placed> {
        let k = keymap(de);
        let find = |c: char| {
            (1..60u16)
                .flat_map(|code| [(code, false), (code, true)])
                .find(|&(code, shift)| k(code, shift) == Some(c.to_string()))
        };
        let labels = Labels {
            key: &k,
            find: &find,
            layout_name: "English (US)",
            layouts,
        };
        layout(page, Bar::Default, false, 400.0, 260.0, &labels)
    }

    fn row(g: &[Placed], y: f32) -> String {
        g.iter()
            .filter(|p| {
                matches!(p.key, Key::Char { .. }) && p.rect.y <= y && y < p.rect.y + p.rect.h
            })
            .map(|p| p.label.as_str())
            .collect()
    }

    #[test]
    fn letters_follow_the_keymap() {
        let us = grid(Page::Letters, false, 1);
        let row_h = 260.0 / 4.8;
        let y = |i: f32| 0.8 * row_h + (i + 0.5) * row_h;
        assert_eq!(row(&us, y(0.0)), "qwertyuiop");
        assert_eq!(row(&us, y(1.0)), "asdfghjkl");
        assert_eq!(row(&us, y(2.0)), "zxcvbnm");
        let de = grid(Page::Letters, true, 2);
        assert_eq!(row(&de, y(0.0)), "qwertzuiopü");
        assert_eq!(row(&de, y(1.0)), "asdfghjklöä");
        assert_eq!(row(&de, y(2.0)), "yxcvbnm");
    }

    #[test]
    fn globe_only_with_several_layouts() {
        let has = |g: &[Placed]| g.iter().any(|p| p.key == Key::NextLayout);
        assert!(!has(&grid(Page::Letters, false, 1)));
        assert!(has(&grid(Page::Letters, false, 2)));
    }

    #[test]
    fn numbers_page_is_ios_with_keymap_keys_where_possible() {
        let g = grid(Page::Numbers, false, 1);
        let row_h = 260.0 / 4.8;
        let labels = |y: f32| -> String {
            g.iter()
                .filter(|p| {
                    matches!(p.key, Key::Char { .. } | Key::Text(_))
                        && p.rect.y <= y
                        && y < p.rect.y + p.rect.h
                })
                .map(|p| p.label.as_str())
                .collect()
        };
        assert_eq!(labels(0.8 * row_h + 0.5 * row_h), "1234567890");
        assert_eq!(labels(0.8 * row_h + 1.5 * row_h), "-/:;()$&@\"");
        assert_eq!(labels(0.8 * row_h + 2.5 * row_h), ".,?!'");
        // The test keymap types '-' but has no shifted symbols, so no '('.
        assert!(g.iter().any(|p| p.key
            == Key::Char {
                code: 12,
                shift: false
            }));
        assert!(g.iter().any(|p| p.key == Key::Text('(')));
        assert!(g.iter().any(|p| p.key == Key::Page(Page::Symbols)));
    }

    #[test]
    fn cells_stay_inside_and_dont_overlap() {
        for page in [Page::Letters, Page::Numbers, Page::Symbols] {
            let g = grid(page, true, 2);
            for (i, a) in g.iter().enumerate() {
                assert!(a.rect.x >= 0.0 && a.rect.x + a.rect.w <= 400.01, "{a:?}");
                assert!(a.rect.y + a.rect.h <= 260.01, "{a:?}");
                for b in &g[i + 1..] {
                    assert!(!a.rect.intersects(&b.rect), "{a:?} vs {b:?}");
                }
            }
        }
    }

    #[test]
    fn emoji_page_and_its_key() {
        assert!(grid(Page::Letters, false, 1)
            .iter()
            .any(|p| p.key == Key::Page(Page::Emoji)));
        let g = grid(Page::Emoji, false, 1);
        let cats = g
            .iter()
            .filter(|p| matches!(p.key, Key::Category(_)))
            .count();
        assert_eq!(cats, crate::emoji::ICONS.len());
        assert!(g.iter().any(|p| p.key == Key::Page(Page::Letters)));
        assert!(g.iter().any(|p| p.key == Key::Plain(code::BACKSPACE)));
        let area = emoji_area(400.0, 260.0);
        assert!(
            !g.iter().any(|p| p.rect.intersects(&area)),
            "keys clear the picker"
        );
    }

    /// Real xkeyboard-config data: one grid has to serve every layout.
    #[test]
    fn real_layouts_lay_themselves_out() {
        let row_h = 260.0 / 4.8;
        let y = |i: f32| 0.8 * row_h + (i + 0.5) * row_h;
        for (name, want) in [
            ("no", ["qwertyuiopå", "asdfghjkløæ", "zxcvbnm"]),
            ("us", ["qwertyuiop", "asdfghjkl", "zxcvbnm"]),
            ("de", ["qwertzuiopü", "asdfghjklöä", "yxcvbnm"]),
            ("fr", ["azertyuiop", "qsdfghjklmù", "wxcvbn"]),
            ("se", ["qwertyuiopå", "asdfghjklöä", "zxcvbnm"]),
            ("ru", ["йцукенгшщзхъ", "фывапролджэ", "ячсмитьбю"]),
            ("gr", ["ςερτυθιοπ", "ασδφγηξκλ", "ζχψωβνμ"]),
        ] {
            let km = crate::labels::Keymap::from_names(name, "").expect("xkeyboard-config");
            let key = |c: u16, shift: bool| km.label(c, shift, 0);
            let reverse = km.reverse(0);
            let find = |c: char| reverse.get(&c).copied();
            let labels = Labels {
                key: &key,
                find: &find,
                layout_name: name,
                layouts: 1,
            };
            let g = layout(Page::Letters, Bar::Default, false, 400.0, 260.0, &labels);
            let got: Vec<String> = (0..3).map(|r| row(&g, y(r as f32))).collect();
            assert_eq!(got, want, "layout {name}");
            for a in &g {
                assert!(
                    a.rect.x >= 0.0 && a.rect.x + a.rect.w <= 400.01,
                    "{name}: {a:?}"
                );
            }
        }
    }

    #[test]
    fn hit_finds_the_cell() {
        let g = grid(Page::Letters, false, 1);
        let q = g.iter().find(|p| p.label == "q").unwrap();
        assert_eq!(
            hit(&g, q.rect.center_x(), q.rect.center_y()),
            Some(Key::Char {
                code: 16,
                shift: false
            })
        );
    }
}
