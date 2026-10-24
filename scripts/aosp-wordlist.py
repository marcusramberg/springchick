#!/usr/bin/env python3
"""Turn an AOSP LatinIME `*.combined.gz` into sc-predict's `word\tf` list.

`f` is AOSP's 0-255 log-scale frequency. Profanity (f=0, offensive) is
dropped. `--extra FILE` merges a `word count` file on any scale (a corpus
frequency list such as hermitdave/FrequencyWords, or score-words.py output):
words AOSP lacks get an `f` by mapping their count, by log2 bucket, to the
median `f` of the words both lists know.

  scripts/aosp-wordlist.py nb_wordlist.combined.gz [--extra no_full.txt] > nb.txt
"""

import argparse
import gzip
import math
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
        f = line.split()
        if len(f) == 2 and f[1].isdigit() and int(f[1]) > 0:
            out[f[0]] = int(f[1])
    return out


def bucket(count):
    return int(2 * math.log2(count))


def calibrate(words, scores):
    """Count bucket -> f, by median over the overlap, made monotonic."""
    buckets = {}
    for w, s in scores.items():
        if w in words:
            buckets.setdefault(bucket(s), []).append(words[w])
    medians = {b: statistics.median(v) for b, v in buckets.items() if len(v) >= 5}
    table, best = {}, 0
    for b in range(max(buckets, default=0) + 1):
        best = max(best, medians.get(b, best))
        table[b] = int(best)
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
            if w not in words and table[bucket(s)] > 0:
                words[w] = table[bucket(s)]
                added += 1
        print(f"{added} words added from {a.extra}", file=sys.stderr)
    for w in sorted(words):
        print(f"{w}\t{words[w]}")


if __name__ == "__main__":
    t = calibrate({f"w{i}": i for i in range(1, 300)}, {f"w{i}": i for i in range(1, 300)})
    assert t[bucket(1)] == 0 and 0 < t[bucket(32)] < t[bucket(64)] < t[bucket(256)], t
    main()
