#!/usr/bin/env python3
"""Turn an AOSP LatinIME `*.combined.gz` into sc-predict's `word\tf` list.

`f` is AOSP's 0-255 log-scale frequency. Profanity (f=0, offensive) is
dropped. `--extra FILE` merges a `word\tscore` file from score-words.py:
words AOSP lacks get an `f` by mapping each LLM score to the median `f` of
the words both lists know.

  scripts/aosp-wordlist.py nb_wordlist.combined.gz [--extra nb_NO.scores] > nb.txt
"""

import argparse
import gzip
import re
import statistics
import sys


def read_aosp(path):
    words = {}
    with gzip.open(path, "rt", encoding="utf-8") as f:
        for line in f:
            m = re.match(r"\s*word=([^,]+),f=(\d+)(.*)", line)
            if not m:
                continue
            w, f, rest = m.group(1), int(m.group(2)), m.group(3)
            if f == 0 or "offensive" in rest or "not_a_word" in rest:
                continue
            words[w] = max(f, words.get(w, 0))
    return words


def read_scores(path):
    out = {}
    for line in open(path, encoding="utf-8"):
        w, _, s = line.rstrip("\n").partition("\t")
        if s.isdigit():
            out[w] = int(s)
    return out


def calibrate(words, scores):
    """LLM score -> f, by median over the overlap, made monotonic."""
    buckets = {}
    for w, s in scores.items():
        if w in words:
            buckets.setdefault(s, []).append(words[w])
    medians = {s: statistics.median(v) for s, v in buckets.items() if len(v) >= 5}
    table, best = {}, 0
    for s in range(101):
        best = max(best, medians.get(s, best))
        table[s] = int(best)
    return table


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("aosp")
    ap.add_argument("--extra")
    a = ap.parse_args()

    words = read_aosp(a.aosp)
    if a.extra:
        scores = read_scores(a.extra)
        table = calibrate(words, scores)
        added = 0
        for w, s in scores.items():
            if w not in words and s > 0 and table[s] > 0:
                words[w] = table[s]
                added += 1
        print(f"{added} words added from {a.extra}", file=sys.stderr)
    for w in sorted(words):
        print(f"{w}\t{words[w]}")


if __name__ == "__main__":
    t = calibrate({f"w{i}": i for i in range(500)}, {f"w{i}": i // 10 for i in range(500)})
    assert t[0] == 4 and t[10] == 104 and t[49] == 494 and t[100] == 494, t
    main()
