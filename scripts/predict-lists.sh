#!/usr/bin/env bash
# Builds sc-osk's word lists from AOSP LatinIME into DIR, for packaging on
# any distro: install DIR as /usr/share/springchick/predict. nix/package.nix
# does the same with pinned hashes.
#
#   scripts/predict-lists.sh DIR [LANG...]
set -euo pipefail

dir=${1:?usage: predict-lists.sh DIR [LANG...]}
shift
rev=127336e9f29d69607eab55982324b210279ae8c5
langs=("$@")
[ ${#langs[@]} -gt 0 ] || langs=(cs da de el en en_GB en_US es fi fr hr it iw lt lv nb nl pl pt_BR pt_PT ro ru sl sr sv tr)

here=$(dirname "$(readlink -f "$0")")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$dir"
for l in "${langs[@]}"; do
    # googlesource serves raw files base64-encoded.
    curl -fsSL "https://android.googlesource.com/platform/packages/inputmethods/LatinIME/+/$rev/dictionaries/${l}_wordlist.combined.gz?format=TEXT" \
        | base64 -d > "$tmp/$l.gz"
    python3 "$here/aosp-wordlist.py" "$tmp/$l.gz" > "$dir/$l.txt"
    echo "$l: $(wc -l < "$dir/$l.txt") words"
done
