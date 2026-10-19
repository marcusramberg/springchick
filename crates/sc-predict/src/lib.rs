#![forbid(unsafe_code)]
//! Word completion, by prefix.
//!
//! Only completion. There is no spell check and no correction, so a word is
//! never replaced by one you did not pick.
//!
//! The words you type are the whole ranking signal, because a hunspell `.dic`
//! carries no word frequencies and every stand-in for them measured badly:
//! ranked by length a Norwegian dictionary answers `kjø` with `kjøen kjøer
//! kjøld` and loses `kjøkken`, and penalising compounds or demanding a longer
//! prefix only reshuffles the noise. So a dictionary is off by default and
//! [`add_dic`] is opt-in, for cold-start suggestions before enough words are
//! learned, held to [`DICT_PREFIX`] to bound how much noise it can add.
//!
//! [`add_dic`]: Predict::add_dic

use std::collections::HashMap;
use std::path::Path;

/// Shorter prefixes match far too much to be worth showing.
pub const MIN_PREFIX: usize = 2;

/// A dictionary only answers prefixes this long: unranked, it is mostly noise
/// before then.
pub const DICT_PREFIX: usize = 4;

/// And only with words this many characters longer than the prefix, since a
/// completion saving one keystroke is not worth the tap.
pub const DICT_GAIN: usize = 2;

/// Dictionary matches to weigh. A short prefix has thousands, all equally
/// unranked, so reading more only costs time.
const SCAN: usize = 64;

#[derive(Default)]
pub struct Predict {
    /// Every stem, sorted and `\n`-separated: one allocation, not 334k.
    arena: String,
    /// Where each stem starts in `arena`, in sort order.
    index: Vec<u32>,
    /// Words typed before, and how often. Outranks the dictionary.
    learned: HashMap<String, u32>,
    unsaved: bool,
}

impl Predict {
    /// Adds the stems of a hunspell `.dic`. Several dictionaries can share one
    /// `Predict`, which is how a bilingual keyboard completes either language.
    ///
    /// Takes bytes, not a `str`: `nb_NO.dic` ships as ISO-8859-1.
    pub fn add_dic(&mut self, bytes: &[u8]) {
        let text = decode(bytes);
        let mut stems: Vec<&str> = text
            .lines()
            // The first line is the entry count, and `word/FLAGS` carries the
            // affix classes, which only a speller needs.
            .skip(1)
            .map(|l| l.split(['/', '\t']).next().unwrap_or("").trim())
            .filter(|w| w.chars().count() > MIN_PREFIX && w.chars().all(is_word_char))
            .collect();
        stems.extend(self.iter());
        stems.sort_unstable();
        stems.dedup();

        let mut arena = String::with_capacity(stems.iter().map(|s| s.len() + 1).sum());
        let mut index = Vec::with_capacity(stems.len());
        for s in stems {
            index.push(arena.len() as u32);
            arena.push_str(s);
            arena.push('\n');
        }
        self.arena = arena;
        self.index = index;
    }

    fn at(&self, i: usize) -> &str {
        let start = self.index[i] as usize;
        let rest = &self.arena[start..];
        &rest[..rest.find('\n').unwrap_or(rest.len())]
    }

    fn iter(&self) -> impl Iterator<Item = &str> {
        (0..self.index.len()).map(|i| self.at(i))
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The stems starting with `prefix`, at most [`SCAN`] of them.
    fn dict_matches<'a>(&'a self, prefix: &str, out: &mut Vec<&'a str>) {
        let start = self.index.partition_point(|&o| {
            let rest = &self.arena[o as usize..];
            &rest[..rest.find('\n').unwrap_or(rest.len())] < prefix
        });
        let min = prefix.chars().count() + DICT_GAIN;
        out.extend(
            (start..self.index.len())
                .map(|i| self.at(i))
                .take_while(|w| w.starts_with(prefix))
                .take(SCAN)
                .filter(|w| w.chars().count() >= min),
        );
    }

    /// Up to `n` completions of `prefix`, best first. Empty below
    /// [`MIN_PREFIX`], and never the prefix itself.
    ///
    /// Words you have typed come first, most-used first. A dictionary, if one
    /// was added, fills the rest from [`DICT_PREFIX`] on.
    pub fn complete(&self, prefix: &str, n: usize) -> Vec<&str> {
        if prefix.chars().count() < MIN_PREFIX {
            return Vec::new();
        }
        let mut learned: Vec<(u32, &str)> = self
            .learned
            .iter()
            .filter(|(w, _)| starts_with_fold(w, prefix) && *w != prefix)
            .map(|(w, &c)| (c, w.as_str()))
            .collect();
        learned.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));

        let mut dict = Vec::new();
        if prefix.chars().count() >= DICT_PREFIX {
            self.dict_matches(prefix, &mut dict);
            // A capitalised prefix still has to find "Oslo", and a lowercase
            // one still has to find it when you did not reach for shift.
            if let Some(other) = case_variants(prefix) {
                self.dict_matches(&other, &mut dict);
            }
            dict.sort_unstable_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
        }

        let mut out: Vec<&str> = Vec::with_capacity(n);
        for w in learned.into_iter().map(|(_, w)| w).chain(dict) {
            if out.len() == n {
                break;
            }
            if w != prefix && !out.iter().any(|o| o.eq_ignore_ascii_case(w)) {
                out.push(w);
            }
        }
        out
    }

    /// Remembers a word you finished, so it outranks the dictionary next time.
    pub fn learn(&mut self, word: &str) {
        if word.chars().count() <= MIN_PREFIX || !word.chars().all(is_word_char) {
            return;
        }
        *self.learned.entry(word.to_string()).or_insert(0) += 1;
        self.unsaved = true;
    }

    /// `COUNT WORD` a line, as [`Self::save`] writes it. Missing file is fine.
    pub fn load_learned(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            if let Some((count, word)) = line.split_once(' ') {
                if let Ok(count) = count.parse() {
                    self.learned.insert(word.to_string(), count);
                }
            }
        }
        self.unsaved = false;
    }

    /// Writes only when something was learned since the last save.
    pub fn save(&mut self, path: &Path) {
        if !self.unsaved {
            return;
        }
        self.unsaved = false;
        let mut lines: Vec<(u32, &str)> =
            self.learned.iter().map(|(w, &c)| (c, w.as_str())).collect();
        lines.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let body: String = lines
            .iter()
            .map(|(c, w)| format!("{c} {w}\n"))
            .collect::<Vec<_>>()
            .concat();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, body);
    }
}

/// UTF-8 if it is, else ISO-8859-1, which is what the Nordic dictionaries are.
fn decode(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Letters, and the marks that sit inside words. Digits and punctuation are
/// not words, and a `.dic` holds a few of each.
fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || c == '\'' || c == '-'
}

fn starts_with_fold(word: &str, prefix: &str) -> bool {
    let mut w = word.chars().flat_map(char::to_lowercase);
    prefix
        .chars()
        .flat_map(char::to_lowercase)
        .all(|p| w.next() == Some(p))
}

/// `prefix` with its first character's case flipped, if that changes anything.
fn case_variants(prefix: &str) -> Option<String> {
    let mut chars = prefix.chars();
    let first = chars.next()?;
    let flipped: String = if first.is_lowercase() {
        first.to_uppercase().chain(chars).collect()
    } else {
        first.to_lowercase().chain(chars).collect()
    };
    (flipped != prefix).then_some(flipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.dic` shape: a count line, then `stem/FLAGS`.
    const DIC: &str = "7\ntastatur/dBCEGz\ntastaturprogram/BEGH\ntast\ntaste\nOslo\nhus/A\n3.5\n";

    fn fixture() -> Predict {
        let mut p = Predict::default();
        p.add_dic(DIC.as_bytes());
        p
    }

    #[test]
    fn completes_from_a_dictionary_shortest_first() {
        let p = fixture();
        assert_eq!(p.complete("tast", 3), ["tastatur", "tastaturprogram"]);
        assert_eq!(p.complete("tastat", 3), ["tastatur", "tastaturprogram"]);
        assert_eq!(p.complete("zzzz", 3), [] as [&str; 0]);
    }

    /// Unranked, a dictionary is mostly noise on a short prefix, so it waits.
    #[test]
    fn a_dictionary_keeps_quiet_until_the_prefix_is_long_enough() {
        let p = fixture();
        assert_eq!(p.complete("ta", 3), [] as [&str; 0]);
        assert_eq!(p.complete("tas", 3), [] as [&str; 0]);
        assert!(!p.complete("tast", 3).is_empty());
    }

    /// Learned words are ranked, so they answer from [`MIN_PREFIX`].
    #[test]
    fn learned_words_answer_a_short_prefix() {
        let mut p = fixture();
        p.learn("tastatur");
        assert_eq!(p.complete("ta", 3), ["tastatur"]);
    }

    /// A completion saving one keystroke is not worth a tap.
    #[test]
    fn a_dictionary_completion_has_to_earn_the_tap() {
        let mut p = Predict::default();
        p.add_dic(b"2\nhuset\nhusholdning\n");
        assert_eq!(p.complete("huse", 3), [] as [&str; 0], "saves one char");
        assert_eq!(p.complete("hush", 3), ["husholdning"]);
    }

    #[test]
    fn never_offers_what_is_already_typed() {
        let mut p = fixture();
        p.learn("taste");
        assert!(!p.complete("taste", 3).contains(&"taste"));
        assert!(!p.complete("tast", 3).contains(&"tast"));
    }

    #[test]
    fn one_letter_is_too_little_to_go_on() {
        let mut p = fixture();
        p.learn("tastatur");
        assert_eq!(p.complete("t", 3), [] as [&str; 0]);
    }

    #[test]
    fn affix_flags_and_non_words_are_dropped() {
        let p = fixture();
        // "tastatur/dBCEGz" is the stem, not the flags.
        assert!(p.complete("tastat", 3).contains(&"tastatur"));
        assert!(!p.iter().any(|w| w.contains('/')));
        assert!(!p.iter().any(|w| w.contains('.')), "3.5 is not a word");
    }

    #[test]
    fn capitalisation_goes_both_ways() {
        let mut p = Predict::default();
        p.add_dic(b"1\nOslofjord\n");
        assert_eq!(p.complete("oslo", 3), ["Oslofjord"], "no shift reached for");
        assert_eq!(p.complete("Oslo", 3), ["Oslofjord"]);
    }

    #[test]
    fn learned_words_outrank_the_dictionary_by_use() {
        let mut p = fixture();
        for _ in 0..3 {
            p.learn("tastaturprogram");
        }
        p.learn("tastatur");
        assert_eq!(
            p.complete("tast", 3),
            ["tastaturprogram", "tastatur"],
            "most-used first, then the dictionary"
        );
    }

    /// The default: no dictionary at all, just what you have typed.
    #[test]
    fn works_with_no_dictionary_at_all() {
        let mut p = Predict::default();
        assert!(p.is_empty());
        p.learn("springchick");
        p.learn("springchick");
        p.learn("kompositor");
        assert_eq!(p.complete("spring", 3), ["springchick"]);
        assert_eq!(p.complete("ko", 3), ["kompositor"]);
    }

    #[test]
    fn learning_ignores_what_is_not_a_word() {
        let mut p = fixture();
        p.learn("ok");
        p.learn("a1b2");
        p.learn("");
        assert!(p.complete("ok", 3).is_empty() && p.complete("a1", 3).is_empty());
    }

    #[test]
    fn latin1_dictionaries_decode() {
        // "lørdag" as ISO-8859-1: 0xF8 for ø.
        let mut bytes = b"1\nl".to_vec();
        bytes.extend([0xF8]);
        bytes.extend(b"rdag\n");
        let mut p = Predict::default();
        p.add_dic(&bytes);
        assert_eq!(p.complete("lørd", 3), ["lørdag"]);
    }

    #[test]
    fn several_dictionaries_complete_either_language() {
        let mut p = Predict::default();
        p.add_dic(b"1\ntastatur\n");
        p.add_dic(b"1\nkeyboard\n");
        assert_eq!(p.complete("tast", 3), ["tastatur"]);
        assert_eq!(p.complete("keyb", 3), ["keyboard"]);
        assert_eq!(p.len(), 2, "both dictionaries, one index");
    }

    #[test]
    fn learned_words_round_trip() {
        let dir = std::env::temp_dir().join(format!("sc-predict-{}", std::process::id()));
        let path = dir.join("learned");
        let mut p = fixture();
        p.learn("springchick");
        p.learn("springchick");
        p.save(&path);

        let mut q = fixture();
        q.load_learned(&path);
        assert_eq!(q.complete("spring", 3), ["springchick"]);
        // Nothing new learned, so nothing to write.
        q.save(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
