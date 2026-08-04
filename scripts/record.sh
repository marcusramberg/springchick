#!/usr/bin/env nix-shell
#!nix-shell -i bash -p wl-screenrec
#
# Record the springchick screen on-device.
#
# springchick implements ext-image-copy-capture-v1 (the successor to
# wlr-screencopy). Capture is zero-copy: the compositor blits the composited
# frame straight into the recorder's dmabuf, so the GPU→CPU download and the
# H.264 encode happen in the recorder process, off springchick's render thread.
#
# Recorder: wl-screenrec. Verified (by inspecting the binary) that even 0.2.0 —
# the version in this nixpkgs channel — already speaks ext-image-copy-capture +
# ext-image-capture-source. wf-recorder is NOT usable here: it only speaks
# wlr-screencopy, which springchick does not implement.
#
# Encoding: SOFTWARE x264 (--no-hw). There is no VAAPI on this Adreno
# (freedreno), and wl-screenrec's hardware path is VAAPI-only — even forcing
# --ffmpeg-encoder h264_v4l2m2m makes it init a VAAPI device first and fail. It
# also ignores --no-hw whenever an encoder is forced, so there is NO route
# through wl-screenrec to the SoC's Venus (h264_v4l2m2m) hardware encoder. Hence
# CPU x264. It's heavy at native 1224x2700; downscale with RESOLUTION for smooth
# demos (e.g. RESOLUTION=612x1350).
#
# Ctrl-C stops and finalizes the file.
#
# Usage:
#   scripts/record.sh [output.mp4]
# Env overrides:
#   SPRINGCHICK_OUTPUT  wl_output name to capture   (default: springchick-0)
#   FPS                 max framerate into encoder   (default: 30)
#   RESOLUTION          downscale, e.g. 612x1350     (default: native)
set -euo pipefail

OUTPUT="${SPRINGCHICK_OUTPUT:-springchick-0}"
FPS="${FPS:-30}"
DEST="${1:-$HOME/springchick-$(date +%Y%m%d-%H%M%S).mp4}"

if ! command -v wl-screenrec >/dev/null 2>&1; then
  echo "wl-screenrec not found. Try: nix shell nixpkgs#wl-screenrec" >&2
  exit 1
fi

args=(
  --output "$OUTPUT"
  --filename "$DEST"
  --max-fps "$FPS"
  # springchick only offers ext-image-copy-capture; be explicit so wl-screenrec
  # never tries the wlr-screencopy path.
  --capture-backend ext-image-copy-capture
  # Download to CPU + software x264. Do NOT add --ffmpeg-encoder: it forces the
  # VAAPI frame path (absent here) and disables --no-hw.
  --no-hw
)
[ -n "${RESOLUTION:-}" ] && args+=(--encode-resolution "$RESOLUTION")

echo "Recording $OUTPUT -> $DEST (${FPS}fps, software x264). Ctrl-C to stop." >&2
exec wl-screenrec "${args[@]}"
