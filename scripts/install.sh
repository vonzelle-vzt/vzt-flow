#!/usr/bin/env bash
# VZT Flow macOS installer.
#
#   curl -fsSL https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.sh | bash
#
# Downloads the latest GitHub Release (.dmg + CLI tarball), installs
# "VZT Flow.app" to /Applications, the `flow` CLI to a PATH dir, and the
# MCP server to ~/.vzt-flow/mcp (registering it with `claude mcp add` when
# the `claude` CLI is present).
#
# Env flags:
#   INSTALL_YES=1   never prompt (assume yes to overwrite prompts)
#   NO_LAUNCH=1     skip `open -a "VZT Flow"` at the end
#   GITHUB_TOKEN    used for asset download when `gh` isn't available
#                   (required while the repo is private)
set -euo pipefail

REPO="vonzelle-vzt/vzt-flow"
APP_NAME="VZT Flow.app"
INSTALL_YES="${INSTALL_YES:-0}"
NO_LAUNCH="${NO_LAUNCH:-0}"

log() { printf '==> %s\n' "$1"; }
warn() { printf 'warning: %s\n' "$1" >&2; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

# --- platform check -----------------------------------------------------

if [ "$(uname -s)" != "Darwin" ]; then
  die "this installer is for macOS only (Windows: scripts/install.ps1)"
fi

ARCH="$(uname -m)"
if [ "$ARCH" != "arm64" ]; then
  die "VZT Flow currently ships Apple Silicon (arm64) builds only — detected: $ARCH"
fi

WORKDIR="$(mktemp -d)"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

# --- download helpers -----------------------------------------------------
# Two download paths: `gh release download` (works out of the box for the
# repo owner / anyone with `gh auth login`, and transparently handles the
# private-repo case), or plain curl against the GitHub REST API using a
# GITHUB_TOKEN (needed for private-repo asset downloads without `gh`).
# Both branches leave files in $WORKDIR; the script works unchanged once
# the repo goes public since `gh` needs no auth for public repos and the
# curl path degrades to unauthenticated requests.

HAVE_GH=0
if command -v gh >/dev/null 2>&1; then
  HAVE_GH=1
fi

download_with_gh() {
  local pattern="$1"
  gh release download --repo "$REPO" --pattern "$pattern" --dir "$WORKDIR" --clobber
}

download_with_token() {
  local pattern="$1"
  local token="${GITHUB_TOKEN:-}"
  [ -n "$token" ] || die "repo is private and 'gh' is not installed — set GITHUB_TOKEN (a PAT with 'repo' scope) or install gh: https://cli.github.com"

  local api="https://api.github.com/repos/${REPO}/releases/latest"
  local assets_json
  assets_json="$(curl -fsSL -H "Authorization: Bearer ${token}" -H "Accept: application/vnd.github+json" "$api")" \
    || die "failed to query GitHub releases API for ${REPO} — check GITHUB_TOKEN and repo access"

  # Portable enough without jq: pull "browser_download_url" / "id" / "name"
  # lines and match by simple glob translated to a regex.
  local regex
  regex="$(printf '%s' "$pattern" | sed -e 's/\./\\./g' -e 's/\*/.*/g')"

  local name url
  while IFS=$'\t' read -r name url; do
    if [[ "$name" =~ ^${regex}$ ]]; then
      log "downloading $name"
      curl -fsSL -H "Authorization: Bearer ${token}" \
        -H "Accept: application/octet-stream" \
        -o "$WORKDIR/$name" "$url"
      return 0
    fi
  done < <(printf '%s' "$assets_json" | python3 -c '
import json, sys
data = json.load(sys.stdin)
for a in data.get("assets", []):
    print(f"{a[\"name\"]}\t{a[\"url\"]}")
' 2>/dev/null || printf '')

  die "no release asset matching '$pattern' found for ${REPO}"
}

fetch_asset() {
  local pattern="$1"
  if [ "$HAVE_GH" = "1" ]; then
    download_with_gh "$pattern"
  else
    download_with_token "$pattern"
  fi
}

# --- download release assets ----------------------------------------------

log "fetching latest release assets for ${REPO}"
fetch_asset "*.dmg"
fetch_asset "vzt-flow-cli-macos-aarch64.tar.gz"

DMG_PATH="$(find "$WORKDIR" -maxdepth 1 -name '*.dmg' | head -n1)"
[ -n "$DMG_PATH" ] || die "download succeeded but no .dmg found in $WORKDIR"
TARBALL_PATH="$WORKDIR/vzt-flow-cli-macos-aarch64.tar.gz"
[ -f "$TARBALL_PATH" ] || die "download succeeded but CLI tarball not found in $WORKDIR"

# --- install the .app -----------------------------------------------------

log "mounting $(basename "$DMG_PATH")"
hdiutil attach "$DMG_PATH" -nobrowse -quiet
# Scan /Volumes for the freshly-mounted app volume rather than parsing
# `hdiutil attach -plist` output, which is brittle across macOS versions.
MOUNT_POINT="$(find /Volumes -maxdepth 1 -iname '*vzt*flow*' 2>/dev/null | head -n1 || true)"
[ -n "$MOUNT_POINT" ] || die "could not locate mounted dmg volume under /Volumes"

unmount_dmg() { hdiutil detach "$MOUNT_POINT" -quiet 2>/dev/null || true; }
trap 'unmount_dmg; cleanup' EXIT

SRC_APP="$MOUNT_POINT/$APP_NAME"
[ -d "$SRC_APP" ] || SRC_APP="$(find "$MOUNT_POINT" -maxdepth 1 -iname '*.app' | head -n1)"
[ -d "$SRC_APP" ] || die "no .app bundle found on mounted dmg ($MOUNT_POINT)"

DEST_APP="/Applications/$(basename "$SRC_APP")"
if [ -d "$DEST_APP" ]; then
  if [ "$INSTALL_YES" != "1" ]; then
    read -r -p "‘$DEST_APP’ already exists — overwrite? [y/N] " reply
    case "$reply" in
      [yY]|[yY][eE][sS]) ;;
      *) die "install aborted — existing app left in place" ;;
    esac
  fi
  log "removing existing $DEST_APP"
  rm -rf "$DEST_APP"
fi

log "installing $(basename "$SRC_APP") to /Applications"
cp -R "$SRC_APP" "$DEST_APP"

unmount_dmg
trap cleanup EXIT

# --- install the flow CLI --------------------------------------------------

log "extracting CLI tarball"
tar -xzf "$TARBALL_PATH" -C "$WORKDIR"
CLI_SRC_DIR="$(find "$WORKDIR" -maxdepth 1 -type d -name 'vzt-flow-cli-*' | head -n1)"
[ -n "$CLI_SRC_DIR" ] || die "CLI tarball did not contain the expected vzt-flow-cli-* directory"

# Prefer /usr/local/bin (already on PATH for most users); fall back to
# ~/.local/bin without sudo if it's not writable.
CLI_DEST_DIR="/usr/local/bin"
if [ ! -w "$CLI_DEST_DIR" ] 2>/dev/null; then
  CLI_DEST_DIR="$HOME/.local/bin"
  mkdir -p "$CLI_DEST_DIR"
fi

log "installing flow CLI to $CLI_DEST_DIR"
install -m 0755 "$CLI_SRC_DIR/bin/flow" "$CLI_DEST_DIR/flow"

if [[ ":$PATH:" != *":$CLI_DEST_DIR:"* ]]; then
  warn "$CLI_DEST_DIR is not on your PATH — add this to your shell profile:"
  warn "  export PATH=\"$CLI_DEST_DIR:\$PATH\""
fi

# --- install the MCP server -------------------------------------------------

MCP_DEST="$HOME/.vzt-flow/mcp"
log "installing MCP server to $MCP_DEST"
mkdir -p "$HOME/.vzt-flow"
rm -rf "$MCP_DEST"
cp -R "$CLI_SRC_DIR/mcp/dist" "$MCP_DEST"
cp "$CLI_SRC_DIR/mcp/README-snippet.md" "$HOME/.vzt-flow/mcp-README.md" 2>/dev/null || true

MCP_ENTRY="$MCP_DEST/index.js"
if [ -f "$MCP_ENTRY" ] && command -v claude >/dev/null 2>&1; then
  log "registering vzt-flow with claude mcp"
  claude mcp remove vzt-flow --scope user >/dev/null 2>&1 || true
  claude mcp add vzt-flow --scope user -- node "$MCP_ENTRY" \
    || warn "failed to register MCP server via 'claude mcp add' — you can retry manually:\n  claude mcp add vzt-flow --scope user -- node \"$MCP_ENTRY\""
else
  [ -f "$MCP_ENTRY" ] || warn "MCP entry file not found at $MCP_ENTRY — skipping claude mcp registration"
  command -v claude >/dev/null 2>&1 || log "claude CLI not found — skipping MCP registration (install later with: claude mcp add vzt-flow --scope user -- node \"$MCP_ENTRY\")"
fi

# --- launch + next steps -----------------------------------------------------

if [ "$NO_LAUNCH" != "1" ]; then
  log "launching VZT Flow"
  open -a "$DEST_APP" || warn "failed to launch $DEST_APP — open it manually from /Applications"
fi

cat <<EOF

VZT Flow is installed.

Grant these 3 permissions when macOS prompts (or in System Settings ->
Privacy & Security):
  1. Microphone       — required to record dictation audio
  2. Accessibility     — required for the hold-to-talk hotkey (Right Option)
  3. Input Monitoring   — required for the CGEventTap hotkey listener

CLI:  flow --help
MCP:  claude mcp list   (should show "vzt-flow")
EOF
