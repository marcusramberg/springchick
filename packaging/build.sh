#!/usr/bin/env bash
# Build a distro package for springchick, locally.
#
#   packaging/build.sh <alpine|debian|arch> [amd64|arm64]
#
# Runs packaging/compile.sh in a container of the target distro (so the binary
# links against that distro's glibc/musl), then packages the result with nfpm
# on the host. Output lands in dist/. CI does the same two steps without the
# container wrapper — see .woodpecker/nightly.yaml.
#
# Env:
#   SKIA_BINARIES_URL  prebuilt skia blob to use instead of building it
#                      (see packaging/README.md — matters for musl)
#   ENGINE             podman (default) or docker
#   PKG_RELEASE        package release number, default 1
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
distro=${1:?usage: build.sh <alpine|debian|arch> [amd64|arm64]}

case ${2:-$(uname -m)} in
  amd64 | x86_64) pkgarch=amd64 platform=linux/amd64 ;;
  arm64 | aarch64) pkgarch=arm64 platform=linux/arm64 ;;
  *) echo "unknown arch: ${2:-$(uname -m)}" >&2; exit 1 ;;
esac

case $distro in
  # alpine:3.22 ships rustc 1.87; workspace deps need 1.88+.
  alpine) image=docker.io/library/alpine:edge ;;
  debian) image=docker.io/library/debian:trixie ;;
  arch)
    # There is no official archlinux image for arm64.
    if [ "$pkgarch" = arm64 ]; then
      image=docker.io/menci/archlinuxarm:base-devel
    else
      image=docker.io/library/archlinux:base-devel
    fi
    ;;
  *) echo "unknown distro: $distro" >&2; exit 1 ;;
esac

engine=${ENGINE:-podman}

# Foreign-arch builds need a binfmt handler registered with the F (fix-binary)
# flag, or the interpreter is invisible inside the container's mount namespace
# and every exec fails with "missing dynamic library". NixOS registers P only.
case $(uname -m) in x86_64) host=amd64 ;; aarch64) host=arm64 ;; *) host=$(uname -m) ;; esac
if [ "$pkgarch" != "$host" ] && ! grep -q 'flags:.*F' /proc/sys/fs/binfmt_misc/*-linux 2>/dev/null; then
  echo "no F-flag binfmt handler for $pkgarch; register one once with:" >&2
  echo "  $engine run --privileged --rm docker.io/tonistiigi/binfmt --install $pkgarch" >&2
  exit 1
fi

out=$repo/dist/$distro-$pkgarch
mkdir -p "$out"

# Named volumes keep the cargo registry and target dir across runs. The first
# musl build compiles skia from source (~40 min); the cache is what makes the
# second one bearable.
# ponytail: dependency install repeats every run; bake a prepared image per
# distro (podman build/commit) if the wait starts to hurt.
vol=springchick-$distro-$pkgarch

# Passing SKIA_BINARIES_URL through empty is not the same as not passing it:
# skia-bindings would curl a blank URL, fail, and fall back to a source build.
skia_env=()
if [ -n "${SKIA_BINARIES_URL:-}" ]; then
  skia_env=(-e "SKIA_BINARIES_URL=$SKIA_BINARIES_URL")
fi

"$engine" run --rm --platform "$platform" \
  "${skia_env[@]}" \
  -v "$repo":/src:ro \
  -v "$vol-cargo":/cargo \
  -v "$vol-target":/target \
  -v "$out":/out \
  -w /src \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -e RUSTUP_HOME=/cargo/rustup \
  -e DISTRO="$distro" -e OUT=/out \
  "$image" sh /src/packaging/compile.sh

cd "$repo"
packaging/package.sh "$distro" "$pkgarch"
