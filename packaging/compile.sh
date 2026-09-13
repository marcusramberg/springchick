#!/bin/sh
# Build the springchick binaries. Runs *inside* a container of the target
# distro — packaging/build.sh mounts the repo and calls this, Woodpecker runs
# it directly in the distro image.
#
# Env:
#   DISTRO             alpine | debian | arch (required)
#   OUT                staging dir for the binaries, default dist/$DISTRO-$PKG_ARCH
#   PKG_ARCH           only used to name the default OUT
#   SKIA_BINARIES_URL  prebuilt skia blob (optional; matters for musl)
set -eu

: "${DISTRO:?set DISTRO=alpine|debian|arch}"
OUT=${OUT:-dist/$DISTRO-${PKG_ARCH:-$(uname -m)}}

case "$DISTRO" in
  alpine)
    apk add --no-cache build-base cargo rust clang-dev llvm-dev pkgconf \
      curl ca-certificates \
      python3 git samurai gn bash \
      wayland-dev wayland-protocols libxkbcommon-dev libinput-dev mesa-dev \
      eudev-dev libseat-dev dbus-dev fontconfig-dev freetype-dev expat-dev zlib-dev
    # No prebuilt skia exists for musl, so skia-bindings full-builds it (it
    # fetches the skia source itself); gn and ninja must be ours, the bundled
    # ones are glibc binaries.
    export SKIA_GN_COMMAND=/usr/bin/gn SKIA_NINJA_COMMAND=/usr/bin/samu
    # skia-bindings only adds Alpine libstdc++ include paths for
    # *-unknown-linux-musl; Alpine rustc is <arch>-alpine-linux-musl, so it
    # falls through to generic linux and clang cannot find bits/c++config.h
    # (it lives in a triple-named subdir). Add it here.
    cxx=/usr/include/c++/$(ls /usr/include/c++ | sort -V | tail -1)
    cxxinc="-I$cxx -I$cxx/$(cc -dumpmachine)"
    export SKIA_GN_ARGS="extra_cflags_cc=[\"-I$cxx\",\"-I$cxx/$(cc -dumpmachine)\"]"
    export BINDGEN_EXTRA_CLANG_ARGS="$cxxinc"
    ;;
  debian)
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends \
      build-essential pkg-config curl ca-certificates git python3 \
      clang libclang-dev rustup \
      libwayland-dev wayland-protocols libxkbcommon-dev libinput-dev \
      libgbm-dev libegl-dev libgles-dev libudev-dev libseat-dev \
      libdbus-1-dev libfontconfig-dev libfreetype-dev libexpat1-dev zlib1g-dev
    rustup default stable
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
    ;;
  arch)
    # Landlock sandbox cannot be applied inside a container.
    pacman -Syu --noconfirm --needed --disable-sandbox \
      base-devel pkgconf clang python git rustup \
      wayland wayland-protocols libxkbcommon libinput mesa seatd \
      systemd-libs dbus fontconfig freetype2 expat zlib
    rustup default stable
    export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
    ;;
  *)
    echo "unknown distro: $DISTRO" >&2
    exit 1
    ;;
esac

cargo build --release --locked -p sc-compositor -p sc-search

mkdir -p "$OUT"
bin=${CARGO_TARGET_DIR:-target}/release
install -m755 "$bin/springchick" "$bin/sc-search" "$OUT/"
sed 's|@springchick@|/usr/bin/springchick|' nix/springchick-session > "$OUT/springchick-session"
chmod 755 "$OUT/springchick-session"
