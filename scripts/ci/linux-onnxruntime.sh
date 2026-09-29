#!/usr/bin/env bash
# Fetch Microsoft's official onnxruntime shared library and point ort-sys at it.
#
# Why: the prebuilt static libonnxruntime that ort-sys downloads from pyke's CDN
# is built against glibc 2.38 (__isoc23_strtol*), so it cannot link on Ubuntu
# 22.04 (glibc 2.35). Microsoft's official release is built on an older
# baseline. ort-sys links it dynamically when ORT_LIB_PATH + ORT_PREFER_DYNAMIC_LINK
# are set (ort-sys 2.0.0-rc.12 build/main.rs), which also skips its own download.
#
# Exports (via $GITHUB_ENV when set) ORT_LIB_PATH, ORT_PREFER_DYNAMIC_LINK,
# LD_LIBRARY_PATH, and stages a resolved copy of the library at
# apps/desktop/src-tauri/linux-libs/ for the .deb bundle (tauri.linux.conf.json).
set -euo pipefail

ORT_VERSION="1.24.2"
ORT_SHA256="43725474ba5663642e17684717946693850e2005efbd724ac72da278fead25e6"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DEST="${RUNNER_TEMP:-/tmp}/onnxruntime"
TGZ="${RUNNER_TEMP:-/tmp}/onnxruntime-linux-x64-${ORT_VERSION}.tgz"

mkdir -p "$DEST"
curl -fsSL --retry 3 -o "$TGZ" \
  "https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-x64-${ORT_VERSION}.tgz"
echo "${ORT_SHA256}  ${TGZ}" | sha256sum -c -
tar -xzf "$TGZ" -C "$DEST" --strip-components=1

LIB="$DEST/lib"
REAL="$(readlink -f "$LIB/libonnxruntime.so")"
echo "onnxruntime ${ORT_VERSION}: $REAL"
echo "SONAME: $(objdump -p "$REAL" | awk '/SONAME/ {print $2}')"
echo "Highest GLIBC needed by onnxruntime: $(objdump -T "$REAL" | awk '/\*UND\*/' | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)"
echo "Highest GLIBCXX needed by onnxruntime: $(objdump -T "$REAL" | awk '/\*UND\*/' | grep -o 'GLIBCXX_[0-9.]*' | sort -Vu | tail -1)"

# Resolved (non-symlink) copy named after the SONAME, for the .deb.
STAGE="$ROOT/apps/desktop/src-tauri/linux-libs"
mkdir -p "$STAGE"
cp -L "$REAL" "$STAGE/libonnxruntime.so.1"

if [ -n "${GITHUB_ENV:-}" ]; then
  {
    echo "ORT_LIB_PATH=$LIB"
    echo "ORT_PREFER_DYNAMIC_LINK=1"
    echo "LD_LIBRARY_PATH=$LIB${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  } >> "$GITHUB_ENV"
fi
