#!/bin/sh
# Upload built packages in dist/ to the Forgejo package registries.
#
#   FORGEJO_TOKEN=... packaging/publish.sh [files...]   # default: everything in dist/
#
# Env:
#   FORGEJO_URL    default https://code.bas.es
#   FORGEJO_OWNER  default marcus
#   FORGEJO_TOKEN  required
#   APK_BRANCH     Alpine registry branch, default edge
#   APK_REPO       Alpine registry repository, default main
#   DEB_DIST       Debian distribution, default trixie
#   DEB_COMPONENT  Debian component, default main
#   ARCH_GROUP     Arch registry group, default springchick
set -eu

url=${FORGEJO_URL:-https://code.bas.es}
owner=${FORGEJO_OWNER:-marcus}
: "${FORGEJO_TOKEN:?set FORGEJO_TOKEN}"

repo=$(cd "$(dirname "$0")/.." && pwd)
[ "$#" -gt 0 ] || set -- "$repo"/dist/*.apk "$repo"/dist/*.deb "$repo"/dist/*.pkg.tar.zst

published=0
for f in "$@"; do
  [ -f "$f" ] || continue
  case $f in
    *.apk) endpoint="alpine/${APK_BRANCH:-edge}/${APK_REPO:-main}" ;;
    *.deb) endpoint="debian/pool/${DEB_DIST:-trixie}/${DEB_COMPONENT:-main}/upload" ;;
    *.pkg.tar.zst) endpoint="arch/${ARCH_GROUP:-springchick}" ;;
    *) echo "skipping $f" >&2; continue ;;
  esac

  echo "-> $(basename "$f") to $endpoint"
  curl -fsSL -X PUT \
    -H "Authorization: token $FORGEJO_TOKEN" \
    --upload-file "$f" \
    "$url/api/packages/$owner/$endpoint"
  published=$((published + 1))
done

[ "$published" -gt 0 ] || { echo "nothing in dist/ to publish" >&2; exit 1; }
