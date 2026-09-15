#!/bin/sh
# Turn the binaries staged by compile.sh into a distro package.
#
#   packaging/package.sh <alpine|debian|arch> <amd64|arm64>
#
# Env:
#   PKG_VERSION  default: sc-compositor's Cargo.toml version
#   PKG_RELEASE  default: 1
set -eu

distro=${1:?usage: package.sh <alpine|debian|arch> <amd64|arm64>}
pkgarch=${2:?usage: package.sh <alpine|debian|arch> <amd64|arm64>}

case $distro in
  alpine) packager=apk ;;
  debian) packager=deb ;;
  arch) packager=archlinux ;;
  *) echo "unknown distro: $distro" >&2; exit 1 ;;
esac

version=${PKG_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' crates/sc-compositor/Cargo.toml | head -1)}

# nfpm does not expand env vars in `contents.src`, so fill the manifest in here.
render() {
  sed -e "s|\${PKG_ARCH}|$pkgarch|" \
      -e "s|\${PKG_VERSION}|$version|" \
      -e "s|\${PKG_RELEASE}|${PKG_RELEASE:-1}|" \
      -e "s|\${PKG_BIN}|dist/$distro-$pkgarch|" \
      "$1" > "$2"
}

render packaging/nfpm.yaml "dist/nfpm-$distro-$pkgarch.yaml"
nfpm package -f "dist/nfpm-$distro-$pkgarch.yaml" -p "$packager" -t dist/

# The session-wiring package: units and dependencies only, no staged binaries,
# so it exists for Alpine/postmarketOS alone.
if [ "$distro" = alpine ]; then
  render packaging/nfpm-ui.yaml "dist/nfpm-ui-$pkgarch.yaml"
  nfpm package -f "dist/nfpm-ui-$pkgarch.yaml" -p apk -t dist/
fi
