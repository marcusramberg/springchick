# Springchick

## A Wayland compositor for Linux phones, written in Rust with Smithay and Skia

_The Authorative home for this repo is <https://code.bas.es/marcus/springchick/> -
Github is a mirror to allow easier contributions and visibility._

_IMPORTANT_: This project is made using LLM assist, however, it is not a low
effort project, and I don't consider it to be slop. It is in early development.
I'm daily driving it on my fairphone 5 running nixos. See
[dmsmobile](https://code.bas.es/marcus/dms-mobile) or
[my flake](https://code.bas.es/marcus/nix-config/src/branch/main/machines/dmsmobile/configuration.nix).

<img align="right" src="./assets/springchick.svg" alt="Right aligned icon">

- Springchick gets a lot of inspiration from iOS' springboard shell, and will be
  quite familiar to iPhone users.
- Implements a single finger user interface with smooth animation and
  live updates of all cards across transitions.
- Home manager with paging and reordering/hiding.
- Pull down to search
- dbus based rotation in fullscreen apps like media players and games.
- Keyboard shortcuts with hardware key mapping including long press.
- Automatic keyboard pop up integration with wvkbd
- External display support (mirroring only)
- Supports most required wayland protocols including ones for screen
  recording/shots/clipboard++
- Currently mostly tested on Nixos, I recommend using the provided flake on
  nixos phones. Nightly aarch64 packages are also published for
  postmarketOS/Alpine, Mobian/Debian and Arch — see [Installation](#installation).

## Screenshots

![app switcher](./assets/springchick-20260815-142937.png) ![nvim in foot with automatic keyboard popup](./assets/springchick-20260815-142952.png) ![card in drag mode](./assets/springchick-20260815-143026.png) ![editing the homescreen](./assets/springchick-20260815-143131.png)

## Installation

Nightly packages are built from `main` and published to the Forgejo registries
on code.bas.es. **aarch64 only** so far, and they are nightlies — expect
breakage. Versions look like `0.1.0.20260913`.

On NixOS, use the flake instead: add this repo as an input and enable
`programs.springchick`.

### postmarketOS

Needs a **systemd** postmarketOS install (`pmbootstrap init` → systemd). The
session is a `Type=notify` user unit, and that unit is the only thing that
raises `graphical-session.target` — without it portals, the on-screen keyboard
and everything else gating on an active graphical session stay down.

```sh
sudo curl -JO --output-dir /etc/apk/keys \
  https://code.bas.es/api/packages/marcus/alpine/key
echo 'https://code.bas.es/api/packages/marcus/alpine/edge/nightly' \
  | sudo tee -a /etc/apk/repositories
sudo apk update && sudo apk add springchick
```

The units land in `/usr/lib/systemd/{system,user}`, which is what
`postmarketos-base-systemd`'s apk trigger watches — no manual
`systemctl daemon-reload` needed.

### Mobian / Debian

```sh
sudo curl -fsSL https://code.bas.es/api/packages/marcus/debian/repository.key \
  -o /etc/apt/keyrings/forgejo-marcus.asc
echo 'deb [signed-by=/etc/apt/keyrings/forgejo-marcus.asc] https://code.bas.es/api/packages/marcus/debian trixie nightly' \
  | sudo tee /etc/apt/sources.list.d/springchick.list
sudo apt update && sudo apt install springchick
```

### Arch

```sh
curl -fsSL https://code.bas.es/api/packages/marcus/arch/repository.key \
  | sudo pacman-key --add -
sudo pacman-key --lsign-key marcus@noreply.code.bas.es
```

The `lsign-key` step is not optional — without it `pacman -Sy` rejects the
repo database as "unknown trust". Then append to `/etc/pacman.conf`:

```ini
[springchick-nightly]
Server = https://code.bas.es/api/packages/marcus/arch/springchick-nightly/$arch
```

```sh
sudo pacman -Sy springchick
```

### Starting it

All three ship a wayland session (`/usr/share/wayland-sessions/springchick.desktop`),
so greetd/GDM will list **springchick**. `/usr/share/springchick/config.example.toml`
documents every option at its default — copy it to
`/etc/springchick/config.toml` or `~/.config/springchick/config.toml` to change
anything.

Building the packages yourself: [packaging/README.md](packaging/README.md).

## On the roadmap

- Even smoother animations.
- More actions supported for hardware key mapping
- Startup apps on boot (currently best implemented through systemd user
  services)
- Optional rotation for UI in addition to the current full screen rotation.
