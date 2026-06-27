# Running springchick on the Fairphone 5 (M4 device backend)

Device: `ssh dmsmobile` — Mobile NixOS, Phosh on VT1, logind/`seat0`. The repo is
checked out at `~/Source/springchick` and builds happen on-device.

## Seat access: seatd over SSH (no physical login)

The DRM node `/dev/dri/card0` is `root:video` with an ACL logind grants to the **active VT
session's** user. An SSH session owns no seat, so `chvt` alone does NOT help — your SSH
process still isn't the seat owner (`session.open` → `EPERM`). Two ways to get DRM-master:

- **Physical VT login** (proper, but impractical on a phone with no console keyboard), or
- **`seatd` + `LIBSEAT_BACKEND=seatd`** (works entirely over SSH) — the method below.

Mobile NixOS runs greetd → phosh, which holds DRM-master, so stop greetd first. `seatd`
ships in the dev shell (`pkgs.seatd`). You are already in the `video` group.

## Run (seatd over SSH)

`ssh dmsmobile`, `cd ~/Source/springchick && git pull`, then run this — it stops Phosh,
runs springchick until you Ctrl-C, and always restores Phosh:

```bash
cat > /tmp/sc-run.sh <<'EOF'
#!/usr/bin/env bash
cd ~/Source/springchick
SEATD=$(nix develop -c sh -c 'command -v seatd')
cleanup(){ sudo pkill -x seatd 2>/dev/null; sudo systemctl start greetd; echo "[restored greetd]"; }
trap cleanup EXIT
echo "[stopping greetd]"; sudo systemctl stop greetd; sleep 3
echo "[starting seatd]"; sudo "$SEATD" -g video >/tmp/seatd.log 2>&1 & sleep 1
echo "[launching springchick — Ctrl-C to stop]"
LIBSEAT_BACKEND=seatd SPRINGCHICK_BACKEND=drm SPRINGCHICK_PERF=1 \
  nix develop -c cargo run -p sc-compositor 2>&1 | tee /tmp/springchick-perf.log
EOF
chmod +x /tmp/sc-run.sh && /tmp/sc-run.sh
```

Drive by touch: tap an icon (zoom-open) → grab the bottom bar, drag up, release (home) →
horizontal flick on the bar (quick-switch). Read the per-second perf line
(`fps=.. p50=..ms p99=..ms dropped=.. n=..`). `Ctrl-C` stops springchick and restores Phosh.

> The `sleep 3` after stopping greetd lets it release DRM-master; without it springchick may
> log "Unable to become drm master, assuming unprivileged mode".

## Known results / quirks (FP5, confirmed 2026-06-27)

- **Orientation (resolved):** the GBM scanout buffer is vertically flipped vs winit's
  framebuffer. Skia home/bar uses `flip_y` (`skia_flip_y=true` in the DRM `DrawCtx`); the
  wayland app-window composites correct with `Transform::Normal`. Both are set in
  `drm_backend.rs`. (A naive `Transform::_180` leaves the home upside-down and the app
  rotated — the two layers need different corrections because Skia draws via raw GL,
  bypassing the smithay output transform.)
- **Tearing (resolved):** `queue_buffer(sync=None)` presented buffers before the GPU
  finished → top-row + whole-screen flicker. Fixed with `skia.finish_gpu()` (glFinish)
  before the page-flip.
- **App HiDPI (open / M5):** the output advertises `Scale::Integer(1)`, so clients render
  1:1 on the dense 1224×2700 panel and look tiny. Display-scaling is out of M4 scope.
- **Perf:** steady render cost ~`p50 4.9ms / p99 5.4ms / dropped 0` against the 11.1ms
  90Hz budget. NB: the perf line's `fps` is render throughput (render duration), not the
  present rate.

## Troubleshooting

- **libseat refuses the seat / "DRM master" errors:** confirm `seatd` is running and you
  passed `LIBSEAT_BACKEND=seatd`, and that greetd was stopped (it holds DRM-master).
- **`next_buffer` / GBM allocation failures:** likely a format/modifier mismatch on Adreno.
  The color formats requested are `Argb8888`/`Xrgb8888`; if allocation fails, the renderer's
  dmabuf formats may not intersect scanout-capable modifiers — try linear, or narrow the
  `color_formats` list.
- **Black screen but perf line ticking:** the EGL context must be current when Skia draws.
  If home/bar don't appear, the Skia GL overlay isn't seeing the renderer's context — check
  ordering around `renderer.bind` in `App::render`.

## Done criteria (M4)

springchick takes DRM-master, renders the home screen right-side-up, touch input works
(tap launches; full home → open → grab → home + quick-switch loop), and the `FrameStats`
line prints sustained numbers. The numbers are the deliverable — no pass/fail threshold.

Status 2026-06-27: runs on FP5 over DRM via seatd-over-SSH; correct orientation; no
tearing; touch tap confirmed launching apps; render cost ~5ms/frame. Remaining to fully
close: drive the complete gesture loop and record the perf line.
