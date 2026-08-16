# Running springchick on the Fairphone 5 (M4 device backend)

Device: `ssh dmsmobile` — Mobile NixOS, logind/`seat0`, greeter + default session are now
`dms-greeter` + niri (this doc's older Phosh/greetd references still describe the same
greetd-holds-DRM-master situation). The repo is checked out at `~/Source/springchick` and
builds happen on-device.

## Installing springchick as a login session

The flake exposes `packages.springchick` and `nixosModules.springchick`. The package ships
`bin/springchick`, `bin/springchick-session` (same binary with `SPRINGCHICK_BACKEND=drm` +
`XDG_SESSION_TYPE=wayland`), and `share/wayland-sessions/springchick.desktop`; the module
adds it to `environment.systemPackages` and `services.displayManager.sessionPackages`, so
the greeter lists "springchick" as a session and hands it a real logind seat — no
seatd-over-SSH hack.

In the device's `/etc/nixos/flake.nix`:

```nix
inputs.springchick.url = "git+https://code.bas.es/marcus/springchick.git?ref=marcus/fanstack";
inputs.springchick.inputs.nixpkgs.follows = "nixpkgs";
# … in nixosConfigurations.dmsMobile.modules:
inputs.springchick.nixosModules.springchick
{ programs.springchick.enable = true; }
```

Then `sudo nixos-rebuild switch --flake /etc/nixos#dmsMobile`, log out, and pick springchick
in the greeter. `defaultSession` stays niri, so a bad build is one logout away from recovery.

Skia in the Nix build: `skia-bindings` would fetch prebuilt Skia over the network, which the
sandbox forbids, so `nix/package.nix` pins that archive with `fetchurl` and passes it via
`SKIA_BINARIES_URL=file://…`. Bumping `skia-safe` means updating the version, rust-skia repo
hash, feature string, and both target hashes there.

## Config

Config file: `$XDG_CONFIG_HOME/springchick/config.toml` (→ `~/.config/springchick/…`),
then `/etc/springchick/config.toml`, overridable with `SPRINGCHICK_CONFIG=<path>` (strict
override — no fallthrough to the other tiers if that file is missing). A missing or unreadable
file falls through to the next tier (or to compiled-in defaults if none remain); a file that
exists and is readable but fails to parse as TOML uses the compiled-in defaults immediately,
without trying further tiers. Nothing is written to disk. Loaded once at startup — edits need
a restart.

On NixOS, `programs.springchick.config` writes `/etc/springchick/config.toml` from a raw TOML
string (`null` by default — leaves it unmanaged).

### Main

```toml
[main]
dpi = 3               # optional; output scale advertised to clients. Defaults to 3. Fractional (e.g. 2.5) allowed.
idle_blank_secs = 600 # optional; blank the panel after this many seconds of no
                      # input. Defaults to 600 (10 min). Set 0 to disable. The
                      # power button still blanks/wakes on demand.
```

The FP5 panel (1224×2700) is dense enough that 1:1 client rendering looks tiny, so the
default is `3` rather than smithay's usual `1`. It is advertised via `wp_fractional_scale`,
so fractional values (e.g. `2.5`) work too — clients render at that scale and the
compositor's own UI is independently sized for the panel.

### Keybinds

```toml
[keybinds]
long_press_ms = 800          # optional, global

[[keybinds.binding]]
key = "XF86AudioRaiseVolume" # xkb keysym name
press = "short"              # "short" | "long"
action = "volume-up"         # internal action; mutually exclusive with `command`

[[keybinds.binding]]
key = "XF86AudioRaiseVolume"
press = "long"
action = "close-app"

[[keybinds.binding]]
key = "Return"
mods = ["Super"]             # optional; exact match on Ctrl/Alt/Shift/Super
press = "short"
command = "foot"
```

`command` runs through `sh -c`, so pipes and quoting work as written. Actions:

| action | effect |
|---|---|
| `close-app` | close the front toplevel |
| `home` | return to the home screen |
| `toggle-display` | blank/unblank the panel via DPMS — DRM only, no-op under winit |
| `volume-up` / `volume-down` | `wpctl` step the default sink ±5% and show the OSD |
| `volume-mute` | `wpctl` toggle mute and show the OSD |
| `toggle-fullscreen` | flip the foreground app between immersive fullscreen (rotates with the device) and maximized |
| `search` | open the search app — the same UI the Home pull-down opens |
| `switcher-next` / `switcher-prev` | step the switcher deck one card toward older / more-recent apps, opening it first |

The volume actions run `wpctl` (PipeWire), read the level back, and show a vertical OSD bar
on the right edge (top third, next to the rockers): white fill for the level, amber past
100%, greyed with a red slash when muted. It fades after ~1.5s.

Defaults, mirroring the niri bindings this replaced:

| key | short | long |
|---|---|---|
| `XF86AudioRaiseVolume` | action `volume-up` | action `close-app` |
| `XF86AudioLowerVolume` | action `volume-down` | `pkill -SIGRTMIN -f wvkbd-mobintl` |
| `XF86PowerOff` | action `toggle-display` | `systemctl poweroff` |
| `Super`+`h` | action `home` | — |
| `Super`+`f` | action `toggle-fullscreen` | — |
| `Super`+`s` | action `search` | — |
| `Super`+`Tab` | action `switcher-next` | — |
| `Super`+`Shift`+`Tab` (`ISO_Left_Tab`) | action `switcher-prev` | — |

`Super`+`Tab` is a held-modifier switch: the deck opens (the app shrinks into the front card
slot, or the deck rises from Home), each further `Tab` springs the carousel one card on and
wraps at the ends, and **releasing Super opens the focused card** — zooming from wherever
that card is on screen, even mid-flight. Releasing Super with no session running does
nothing. Home lives on `Super`+`h` rather than `Escape` so a terminal keeps its own `Escape`.

Timing — a long press fires while the key is **still held**, and suppresses the short one.
The default threshold is 800ms so a volume nudge does not slip into the long action:

```
press ───┬─────────────── 800ms ───┬──────────── release
         │                         │
    short armed              long FIRES here
                             short suppressed
```

Two behaviors worth knowing before you debug something surprising:

- **A key with any binding never reaches clients**, in either direction. A key that only has
  a long binding still swallows its short press; give it an explicit short binding if an app
  should ever see it.
- **Config errors are skipped, not fatal.** An unknown keysym name, a bad `press` value or a
  binding with both `command` and `action` logs a warning and is dropped; the compositor
  still starts. Check the log for `skipping keybinding` if a button does nothing.

Bind power-long to `logger -t springchick poweroff-would-fire` first and watch
`journalctl -f -t springchick` to confirm the timing before trusting it with the real
`systemctl poweroff` — an 800ms slip otherwise powers the phone off mid-test.

Headless testing without hardware: set `SPRINGCHICK_DEBUG_SOCK=<short path>` and send
`key <keysym-name> [hold_ms]` (e.g. `key XF86PowerOff 700`). Injected keys travel the real
path — xkb lookup, binding match, long-press timing, client forwarding. Keep the socket path
short; `sun_path` is ~100 bytes and a long temp dir fails to bind. `tests/integration.sh`
test 7 does exactly this.

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
- **App HiDPI (resolved):** the output advertises `Scale::Fractional(3.0)` by default (see
  [Config § Main](#main)), configurable via `[main].dpi` in `config.toml`.
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
