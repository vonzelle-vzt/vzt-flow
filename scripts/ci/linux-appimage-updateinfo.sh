#!/usr/bin/env bash
# Embed AppImageUpdate information into the Tauri-built AppImage and produce
# the matching .zsync, so `appimageupdatetool` / AppImageUpdate can update it.
#
# Tauri has no knob for this, so we repack what `cargo tauri build` produced:
#   1. rename to the dot form GitHub uses for release assets
#      ("VZT Flow_0.3.7_amd64.AppImage" -> "VZT.Flow_0.3.7_amd64.AppImage"),
#      so the name in the zsync header == the published asset name;
#   2. unsquashfs as root (keeps the stored modes exactly -- the permission
#      gate in the workflow depends on them);
#   3. appimagetool -u "<update info>" repacks + writes <AppImage>.zsync.
#
# Usage: linux-appimage-updateinfo.sh <bundle/appimage dir>
# Replaces the original AppImage in that dir with the repacked one and leaves
# the .zsync beside it. Last line of stdout is APPIMAGE=<path>.
set -euo pipefail

DIR="${1:?usage: $0 <dir containing the built .AppImage>}"

# Pinned: never track "continuous". Bump both together.
AT_VERSION="1.9.1"
AT_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
AT_URL="https://github.com/AppImage/appimagetool/releases/download/${AT_VERSION}/appimagetool-x86_64.AppImage"

REPO_OWNER="vonzelle-vzt"
REPO_NAME="vzt-flow"

DIR="$(cd "$DIR" && pwd)"
SRC="$(find "$DIR" -maxdepth 1 -name '*.AppImage' | head -n1)"
[ -n "$SRC" ] || { echo "::error::no .AppImage in $DIR" >&2; exit 1; }

BASE="$(basename "$SRC")"
NAME="${BASE// /.}"                       # dot form, matches the GitHub asset
case "$NAME" in
  VZT.Flow_*_amd64.AppImage) ;;
  *) echo "::error::unexpected AppImage name '$BASE' (want 'VZT Flow_<ver>_amd64.AppImage')" >&2; exit 1 ;;
esac
UPDATE_INFO="gh-releases-zsync|${REPO_OWNER}|${REPO_NAME}|latest|VZT.Flow_*_amd64.AppImage.zsync"

WORK="$(mktemp -d)"
trap 'sudo rm -rf "$WORK"' EXIT

echo "== fetching appimagetool ${AT_VERSION} =="
curl -fsSL --retry 3 -o "$WORK/appimagetool" "$AT_URL"
echo "${AT_SHA256}  $WORK/appimagetool" | sha256sum -c -
chmod +x "$WORK/appimagetool"

command -v unsquashfs >/dev/null || sudo apt-get install -y squashfs-tools

chmod +x "$SRC"
echo "== extracting (as root, stored modes preserved) =="
sudo unsquashfs -q -o "$("$SRC" --appimage-offset)" -d "$WORK/squashfs-root" "$SRC" >/dev/null

echo "== repacking with update information =="
mkdir "$WORK/out"
# --appimage-extract-and-run: no FUSE needed on the runner.
( cd "$WORK/out" && ARCH=x86_64 "$WORK/appimagetool" --appimage-extract-and-run \
    -u "$UPDATE_INFO" "$WORK/squashfs-root" "$WORK/out/$NAME" )

[ -f "$WORK/out/$NAME" ] || { echo "::error::appimagetool produced no $NAME" >&2; exit 1; }
ZSYNC="$(find "$WORK/out" -maxdepth 1 -name '*.zsync' | head -n1)"
[ -n "$ZSYNC" ] || { echo "::error::appimagetool produced no .zsync" >&2; exit 1; }
[ "$(basename "$ZSYNC")" = "$NAME.zsync" ] \
  || { echo "::error::zsync is '$(basename "$ZSYNC")', expected '$NAME.zsync'" >&2; exit 1; }

chmod +x "$WORK/out/$NAME"
echo "== verifying =="
GOT="$("$WORK/out/$NAME" --appimage-updateinformation)"
echo "update information: $GOT"
[ "$GOT" = "$UPDATE_INFO" ] || { echo "::error::embedded update info mismatch" >&2; exit 1; }
grep -a -m1 '^Filename: ' "$ZSYNC"
grep -a -m1 '^URL: ' "$ZSYNC"
grep -aq "^Filename: $NAME\$" "$ZSYNC" || { echo "::error::zsync Filename != $NAME" >&2; exit 1; }
grep -aq "^URL: $NAME\$" "$ZSYNC" || { echo "::error::zsync URL != $NAME" >&2; exit 1; }

rm -f "$SRC"
cp "$WORK/out/$NAME" "$DIR/$NAME"
cp "$ZSYNC" "$DIR/$NAME.zsync"
chmod 0755 "$DIR/$NAME"
ls -l "$DIR"
echo "APPIMAGE=$DIR/$NAME"
