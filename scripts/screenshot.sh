#!/usr/bin/env nix-shell
#!nix-shell -i bash -p grim
#
# Single-frame screenshot — the simplest smoke test that springchick's
# ext-image-copy-capture-v1 support works, with no encoder/muxer in the way.
# Run this FIRST after deploying screencopy: if grim writes a correct PNG, the
# protocol + dmabuf blit are good and any remaining trouble is encoder-side.
#
# grim 1.5.0 (this channel) speaks ext-image-copy-capture.
#
# Usage: scripts/screenshot.sh [out.png]
set -euo pipefail

OUTPUT="${SPRINGCHICK_OUTPUT:-springchick-0}"
DEST="${1:-$HOME/springchick-$(date +%Y%m%d-%H%M%S).png}"

if ! command -v grim >/dev/null 2>&1; then
  echo "grim not found. Try: nix shell nixpkgs#grim" >&2
  exit 1
fi

grim -o "$OUTPUT" "$DEST"
echo "Wrote $DEST" >&2
