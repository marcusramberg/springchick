#![forbid(unsafe_code)]
//! Word completion, by prefix, and the next word after a finished one.
//!
//! Only completion. There is no spell check and no correction, so a word is
//! never replaced by one you did not pick.
//!
//! Ranking is a word list with a frequency per word (`word\tf`, `f` 0..255
//! on AOSP's log scale; `scripts/aosp-wordlist.py` makes one), plus the words
//! and word pairs you have typed, which carry the most weight. A hunspell
//! `.dic` has no frequencies and ranked badly however it was sorted, which is
//! why the list format is not hunspell.

use std::collections::HashMap;
use std::path::Path;

/// Each time a word is typed is worth this much frequency.
const LEARNED: u32 = 40;
/// A word never in the list, but typed, starts from here.
const UNLISTED: u32 = 100;
/// Each time `prev word` was typed together.
const PAIR: u32 = 60;

#[derive(Default)]
pub struct Predict {
    /// Every word, sorted and `\n`-separated: one allocation, not 170k.
    arena: String,
    /// Where each word starts in `arena`, in sort order, and its frequency.
    index: Vec<u32>,
    freq: Vec<u8>,
    /// Words typed before, and how often.
    learned: HashMap<String, u32>,
    /// `prev -> next -> count`, from typing.
    pairs: HashMap<String, HashMap<String, u32>>,
    unsaved: bool,
}

impl Predict {
    /// Adds a `word\tf` list. Several lists share one `Predict`, which is
    /// how a bilingual keyboard completes either language.
    pub fn add_list(&mut self, text: &str) {
        let mut words: Vec<(&str, u8)> = text
            .lines()
            .filter_map(|l| {
                let (w, f) = l.split_once('\t')?;
                let w = w.trim();
                let f = f.trim().parse().ok()?;
                (!w.is_empty() && w.chars().all(is_word_char)).then_some((w, f))
            })
            .collect();
        words.extend((0..self.index.len()).map(|i| (self.at(i), self.freq[i])));
        words.sort_unstable_by(|a, b| a.0.cmp(b.0).then(b.1.cmp(&a.1)));
        words.dedup_by(|a, b| a.0 == b.0);

        let mut arena = String::with_capacity(words.iter().map(|w| w.0.len() + 1).sum());
        let mut index = Vec::with_capacity(words.len());
        let mut freq = Vec::with_capacity(words.len());
        for (w, f) in words {
            index.push(arena.len() as u32);
            freq.push(f);
            arena.push_str(w);
            arena.push('\n');
        }
        self.arena = arena;
        self.index = index;
        self.freq = freq;
    }

    fn at(&self, i: usize) -> &str {
        let rest = &self.arena[self.index[i] as usize..];
        &rest[..rest.find('\n').unwrap_or(rest.len())]
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    fn listed(&self, word: &str) -> Option<u8> {
        let i = self.index.partition_point(|&o| {
            let rest = &self.arena[o as usize..];
            &rest[..rest.find('\n').unwrap_or(rest.len())] < word
        });
        (i < self.index.len() && self.at(i) == word).then(|| self.freq[i])
    }

    /// Every listed word starting with `prefix`, with its frequency.
    fn range<'a>(&'a self, prefix: &str, out: &mut Vec<(u32, &'a str)>) {
        let start = self.index.partition_point(|&o| {
            let rest = &self.arena[o as usize..];
            &rest[..rest.find('\n').unwrap_or(rest.len())] < prefix
        });
        out.extend(
            (start..self.index.len())
                .map(|i| (u32::from(self.freq[i]), self.at(i)))
                .take_while(|(_, w)| w.starts_with(prefix)),
        );
    }

    /// Up to `n` completions of `prefix` after `prev`, best first; with an
    /// empty prefix, the words that have followed `prev` before. Never the
    /// prefix itself, and capitalised like the prefix.
    pub fn complete(&self, prev: Option<&str>, prefix: &str, n: usize) -> Vec<String> {
        let pairs = prev.and_then(|p| self.pairs.get(&p.to_lowercase()));
        let mut found: Vec<(u32, &str)> = Vec::new();
        if prefix.is_empty() {
            let Some(pairs) = pairs else {
                return Vec::new();
            };
            found.extend(pairs.iter().map(|(w, &c)| (c * PAIR, w.as_str())));
        } else {
            self.range(prefix, &mut found);
            if let Some(other) = case_variants(prefix) {
                self.range(&other, &mut found);
            }
            for (w, &c) in &self.learned {
                if starts_with_fold(w, prefix) {
                    let base = self.listed(w).map_or(UNLISTED, u32::from);
                    found.push((base + c * LEARNED, w));
                }
            }
            if let Some(pairs) = pairs {
                for (score, w) in &mut found {
                    if let Some(c) = pairs.get(&w.to_lowercase()) {
                        *score += c * PAIR;
                    }
                }
            }
        }
        found.sort_unstable_by(|a, b| {
            b.0.cmp(&a.0)
                .then(a.1.len().cmp(&b.1.len()))
                .then(a.1.cmp(b.1))
        });

        let capital = prefix.chars().next().is_some_and(char::is_uppercase);
        let mut out: Vec<String> = Vec::with_capacity(n);
        for (_, w) in found {
            if out.len() == n {
                break;
            }
            if eq_fold(w, prefix) || out.iter().any(|o| eq_fold(o, w)) {
                continue;
            }
            out.push(if capital {
                capitalise(w)
            } else {
                w.to_string()
            });
        }
        out
    }

    /// Remembers a word you finished, and that it followed `prev`.
    pub fn learn(&mut self, prev: Option<&str>, word: &str) {
        if word.chars().count() < 2 || !word.chars().all(is_word_char) {
            return;
        }
        *self.learned.entry(word.to_string()).or_insert(0) += 1;
        if let Some(prev) = prev.filter(|p| p.chars().all(is_word_char)) {
            *self
                .pairs
                .entry(prev.to_lowercase())
                .or_default()
                .entry(word.to_lowercase())
                .or_insert(0) += 1;
        }
        self.unsaved = true;
    }

    pub fn load_learned(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            let mut f = line.split(' ');
            let Some(count) = f.next().and_then(|c| c.parse().ok()) else {
                continue;
            };
            match (f.next(), f.next()) {
                (Some(w), None) => {
                    self.learned.insert(w.to_string(), count);
                }
                (Some(p), Some(w)) => {
                    self.pairs
                        .entry(p.to_string())
                        .or_default()
                        .insert(w.to_string(), count);
                }
                _ => {}
            }
        }
        self.unsaved = false;
    }

    pub fn save(&mut self, path: &Path) {
        if !self.unsaved {
            return;
        }
        self.unsaved = false;
        let mut lines: Vec<(u32, String)> = self
            .learned
            .iter()
            .map(|(w, &c)| (c, w.clone()))
            .chain(
                self.pairs
                    .iter()
                    .flat_map(|(p, n)| n.iter().map(move |(w, &c)| (c, format!("{p} {w}")))),
            )
            .collect();
        lines.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let body: String = lines.iter().map(|(c, w)| format!("{c} {w}\n")).collect();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, body);
    }
}

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

fn eq_fold(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

fn capitalise(w: &str) -> String {
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

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

    const LIST: &str =
        "tastatur\t120\ntastaturprogram\t60\ntast\t90\ntaste\t100\nOslo\t150\nhus\t180\n";

    fn fixture() -> Predict {
        let mut p = Predict::default();
        p.add_list(LIST);
        p
    }

    fn c(p: &Predict, prefix: &str) -> Vec<String> {
        p.complete(None, prefix, 3)
    }

    #[test]
    fn completes_most_frequent_first() {
        let p = fixture();
        assert_eq!(c(&p, "tas"), ["tastatur", "taste", "tast"]);
        assert_eq!(c(&p, "tastat"), ["tastatur", "tastaturprogram"]);
        assert_eq!(c(&p, "zzzz"), [] as [&str; 0]);
    }

    #[test]
    fn never_offers_what_is_already_typed() {
        let p = fixture();
        assert!(!c(&p, "taste").contains(&"taste".to_string()));
        assert!(!c(&p, "Taste").iter().any(|w| w == "Taste"));
    }

    #[test]
    fn capitalisation_follows_the_prefix() {
        let mut p = Predict::default();
        p.add_list("Oslofjord\t50\nhus\t180\n");
        assert_eq!(c(&p, "oslo"), ["Oslofjord"], "a name keeps its capital");
        assert_eq!(c(&p, "Oslo"), ["Oslofjord"]);
        assert_eq!(c(&p, "Hu"), ["Hus"], "sentence start");
        assert_eq!(c(&p, "hu"), ["hus"]);
    }

    #[test]
    fn learned_words_outrank_the_list_by_use() {
        let mut p = fixture();
        for _ in 0..3 {
            p.learn(None, "tastaturprogram");
        }
        p.learn(None, "tastatur");
        assert_eq!(c(&p, "tast"), ["tastaturprogram", "tastatur", "taste"]);
    }

    #[test]
    fn a_word_typed_once_does_not_beat_a_very_common_one() {
        let mut p = Predict::default();
        p.add_list("the\t222\nthanks\t120\n");
        p.learn(None, "thanks");
        assert_eq!(c(&p, "th"), ["the", "thanks"]);
    }

    #[test]
    fn the_next_word_comes_from_pairs_typed_before() {
        let mut p = fixture();
        p.learn(None, "god");
        p.learn(Some("god"), "morgen");
        p.learn(Some("god"), "morgen");
        p.learn(Some("god"), "natt");
        assert_eq!(p.complete(Some("god"), "", 3), ["morgen", "natt"]);
        assert_eq!(
            p.complete(Some("God"), "", 3),
            ["morgen", "natt"],
            "case folded"
        );
        assert_eq!(p.complete(None, "", 3), [] as [&str; 0]);
        assert_eq!(p.complete(Some("hus"), "", 3), [] as [&str; 0]);
    }

    #[test]
    fn a_pair_lifts_a_completion() {
        let mut p = fixture();
        p.learn(Some("mitt"), "tast");
        p.learn(None, "tastatur");
        assert_eq!(p.complete(Some("mitt"), "tas", 3)[0], "tast");
        assert_eq!(c(&p, "tas")[0], "tastatur", "only after that word");
    }

    /// The default: no list at all, just what you have typed.
    #[test]
    fn works_with_no_list_at_all() {
        let mut p = Predict::default();
        assert!(p.is_empty());
        p.learn(None, "springchick");
        p.learn(None, "springchick");
        p.learn(None, "kompositor");
        assert_eq!(c(&p, "spring"), ["springchick"]);
        assert_eq!(c(&p, "k"), ["kompositor"]);
    }

    #[test]
    fn learning_ignores_what_is_not_a_word() {
        let mut p = fixture();
        p.learn(None, "a1b2");
        p.learn(None, "");
        p.learn(None, "x");
        assert!(c(&p, "a1").is_empty() && c(&p, "x").is_empty());
    }

    #[test]
    fn several_lists_complete_either_language() {
        let mut p = Predict::default();
        p.add_list("tastatur\t100\n");
        p.add_list("keyboard\t100\ntastatur\t50\n");
        assert_eq!(c(&p, "tast"), ["tastatur"]);
        assert_eq!(c(&p, "keyb"), ["keyboard"]);
        assert_eq!(p.len(), 2, "both lists, one index, duplicates merged");
        assert_eq!(p.listed("tastatur"), Some(100), "the higher frequency wins");
    }

    #[test]
    fn learned_words_and_pairs_round_trip() {
        let dir = std::env::temp_dir().join(format!("sc-predict-{}", std::process::id()));
        let path = dir.join("learned");
        let mut p = fixture();
        p.learn(None, "springchick");
        p.learn(Some("springchick"), "kompositor");
        p.save(&path);

        let mut q = fixture();
        q.load_learned(&path);
        assert_eq!(c(&q, "spring"), ["springchick"]);
        assert_eq!(q.complete(Some("springchick"), "", 3), ["kompositor"]);
        // Nothing new learned, so nothing to write.
        q.save(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
