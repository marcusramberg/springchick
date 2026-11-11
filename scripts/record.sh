#!/usr/bin/env nix-shell
#!nix-shell -i bash -p wf-recorder mkvtoolnix-cli ffmpeg
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
# NATIVE=1 skips all of that for springchick's own recorder (`ipc record`,
# crates/sc-compositor/src/record.rs): every rendered frame, GPU-converted
# straight into the hardware encoder's buffers. It writes a raw H.264 stream
# plus FILE.ts (`TIME_NS DROPPED_BEFORE` per frame), which mkvmerge muxes with
# the real frame times; ffmpeg cannot take timestamps for a raw stream from a
# file. `mux` does only that step, e.g. on another machine.
#
# Ctrl-C stops and finalizes the file.
#
# Usage:
#   scripts/record.sh [output.mp4]
#   NATIVE=1 [SCALE=0.5] scripts/record.sh [output.mkv|output.mp4]
#   scripts/record.sh mux recording.h264 output.mkv|output.mp4
# Env overrides:
#   SPRINGCHICK_OUTPUT  wl_output name to capture    (default: springchick-0)
#   FPS                 constant framerate           (default: 30)
#   RESOLUTION          downscale, e.g. 612x1350     (default: native)
#                       both dimensions must be EVEN — H.264 rejects odd sizes
#   SOFTWARE=1          force CPU x264 instead of the v4l2 hardware encoder
set -euo pipefail

# Raw stream + its .ts -> a variable-frame-rate file.
mux() {
  local raw=$1 dest=$2 mkv=$2
  [[ $dest == *.mkv ]] || mkv=$(mktemp --suffix=.mkv)
  { echo "# timestamp format v2"
    awk 'NR == 1 { t0 = $1 } { printf "%.6f\n", ($1 - t0) / 1e6 }' "$raw.ts"; } > "$raw.tsv2"
  # The encoder writes no colour description; the frames are BT.709 limited.
  ffmpeg -nostdin -loglevel error -y -i "$raw" -c copy -bsf:v \
    h264_metadata=colour_primaries=1:transfer_characteristics=1:matrix_coefficients=1:video_full_range_flag=0 \
    -f h264 "$raw.tagged"
  # 0.1 ms ticks: the default 1 ms would round 120 Hz frame times.
  mkvmerge -q -o "$mkv" --timestamp-scale 100000 --timestamps "0:$raw.tsv2" "$raw.tagged"
  if [ "$mkv" != "$dest" ]; then
    ffmpeg -nostdin -loglevel error -y -i "$mkv" -c copy "$dest"
    rm -f "$mkv"
  fi
  rm -f "$raw.tsv2" "$raw.tagged"
  echo "$dest: $(wc -l < "$raw.ts") frames, $(awk 'END { print $2 }' "$raw.ts") dropped" >&2
}

if [ "${1:-}" = mux ]; then
  mux "$2" "$3"
  exit
fi

if [ -n "${NATIVE:-}" ]; then
  DEST="${1:-$HOME/springchick-$(date +%Y%m%d-%H%M%S).mkv}"
  RAW="${DEST%.*}.h264"
  springchick ipc record start "$RAW" "${SCALE:-1}"
  echo "Recording -> $DEST. Ctrl-C to stop." >&2
  trap : INT
  sleep infinity || true
  springchick ipc record stop
  mux "$RAW" "$DEST"
  rm -f "$RAW" "$RAW.ts"
  exit
fi

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
