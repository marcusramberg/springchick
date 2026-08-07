#!/usr/bin/env nix-shell
#!nix-shell -i bash -p wf-recorder
#
# Record the springchick screen on-device.
#
# springchick implements BOTH capture protocols: ext-image-copy-capture-v1 (the
# modern one, dmabuf + shm) and wlr-screencopy-unstable-v1 (shm; see
# `crates/sc-compositor/src/wlr_screencopy.rs`). Both are gated by the
# `vm-capture` nix check.
#
# Recorder: wf-recorder, over wlr-screencopy. It is the only recorder that can
# reach this SoC's hardware encoder. The reasoning, which cost a device
# investigation to establish:
#
#   - This Adreno (freedreno) has NO VAAPI. It does have a Venus H.264 encoder
#     at /dev/video1, reachable through ffmpeg's h264_v4l2m2m.
#   - wl-screenrec speaks ext-image-copy-capture but its pipeline is VAAPI-only:
#     it maps capture buffers into VAAPI frames no matter what, `--no-hw` only
#     swaps the *encoder*, and forcing --ffmpeg-encoder re-enables the VAAPI
#     device init (which then fails). So it can only ever do software x264 here.
#   - wf-recorder copies to CPU and hands the frame to any ffmpeg encoder, so
#     h264_v4l2m2m is reachable. It only speaks wlr-screencopy — which is why
#     springchick implements that protocol as well.
#
# Hardware encode is the default. If the v4l2 encoder is unavailable or unhappy,
# rerun with SOFTWARE=1 for CPU x264 (heavy at native 1224x2700 — downscale with
# RESOLUTION, e.g. RESOLUTION=612x1350, for a smooth demo).
#
# Ctrl-C stops and finalizes the file.
#
# Usage:
#   scripts/record.sh [output.mp4]
# Env overrides:
#   SPRINGCHICK_OUTPUT  wl_output name to capture    (default: springchick-0)
#   FPS                 constant framerate           (default: 30)
#   RESOLUTION          downscale, e.g. 612x1350     (default: native)
#                       both dimensions must be EVEN — H.264 rejects odd sizes
#   SOFTWARE=1          force CPU x264 instead of the v4l2 hardware encoder
set -euo pipefail

OUTPUT="${SPRINGCHICK_OUTPUT:-springchick-0}"
FPS="${FPS:-30}"
DEST="${1:-$HOME/springchick-$(date +%Y%m%d-%H%M%S).mp4}"

if ! command -v wf-recorder >/dev/null 2>&1; then
  echo "wf-recorder not found. Try: nix shell nixpkgs#wf-recorder" >&2
  exit 1
fi

args=(
  # `-o` has no long form in wf-recorder 0.6.0.
  -o "$OUTPUT"
  --file "$DEST"
  --framerate "$FPS"
  --overwrite
  # yuv420p keeps the result playable everywhere (and is what the v4l2 encoder
  # wants); springchick hands out XRGB8888, so a conversion happens regardless.
  --pixel-format yuv420p
)

if [ -n "${SOFTWARE:-}" ]; then
  args+=(--codec libx264)
  ENCODER="software x264"
else
  # Venus (v4l2 m2m) hardware H.264. B-frames off: the v4l2 encoders on this
  # SoC do not do them, and asking makes the encoder refuse the stream.
  args+=(--codec h264_v4l2m2m --bframes 0)
  ENCODER="hardware h264_v4l2m2m"
fi

# Scale in the filter graph rather than asking the compositor for a smaller
# capture — wlr-screencopy's region request crops, it does not scale.
[ -n "${RESOLUTION:-}" ] && args+=(--filter "scale=${RESOLUTION/x/:}")

echo "Recording $OUTPUT -> $DEST (${FPS}fps, $ENCODER). Ctrl-C to stop." >&2
if [ -z "${SOFTWARE:-}" ]; then
  echo "If the encoder fails to open, rerun with SOFTWARE=1." >&2
fi
exec wf-recorder "${args[@]}"
