//! Types a text through the predictor and reports how often the word shows in
//! the strip, by how many characters were typed first, and the keystrokes
//! saved by taking it then.
//!
//!   cargo run -p sc-predict --example bench -- [--learn] LIST.txt... CORPUS.txt
//!
//! `--learn` learns each word after it is typed, as the keyboard does.

use sc_predict::Predict;

const SLOTS: usize = 3;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let learn = args.iter().any(|a| a == "--learn");
    let files: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let Some((corpus, lists)) = files.split_last() else {
        eprintln!("usage: bench [--learn] LIST.txt... CORPUS.txt");
        std::process::exit(2);
    };
    let mut p = Predict::default();
    for l in lists {
        p.add_list(&std::fs::read_to_string(l).expect(l));
    }
    let text = std::fs::read_to_string(corpus).expect(corpus);
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphabetic() || c == '\'' || c == '-'))
        .filter(|w| w.chars().count() >= 2)
        .collect();

    // hit[k]: words found after k characters (and not before).
    let mut hit = [0usize; 8];
    let mut typed = 0usize;
    let mut saved = 0usize;
    let mut prev = None;
    for &w in &words {
        let n = w.chars().count();
        typed += n + 1;
        for k in 0..n {
            let prefix: String = w.chars().take(k).collect();
            let found = p
                .complete(prev, &prefix, SLOTS)
                .iter()
                .any(|s| s.eq_ignore_ascii_case(w) || s == w);
            if found {
                let slot = k.min(hit.len() - 1);
                hit[slot] += 1;
                // The word and its space for one tap.
                saved += n - k;
                break;
            }
        }
        if learn {
            p.learn(prev, w);
        }
        prev = Some(w);
    }

    let total = words.len();
    println!(
        "{total} words, {} list entries{}",
        p.len(),
        if learn { ", learning" } else { "" }
    );
    let mut cum = 0;
    for (k, &h) in hit.iter().enumerate() {
        cum += h;
        println!(
            "after {k}{} chars: {:5.1}%  (cumulative {:5.1}%)",
            if k == hit.len() - 1 { "+" } else { " " },
            100.0 * h as f64 / total as f64,
            100.0 * cum as f64 / total as f64
        );
    }
    println!(
        "never: {:5.1}%",
        100.0 * (total - cum) as f64 / total as f64
    );
    println!(
        "keystrokes saved: {:5.1}%",
        100.0 * saved as f64 / typed as f64
    );
}
