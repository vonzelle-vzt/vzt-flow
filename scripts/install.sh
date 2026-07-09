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
# The repo is public, so no authentication is needed for the default path.
#
# Env flags:
#   INSTALL_YES=1   never prompt (assume yes to overwrite prompts)
#   NO_LAUNCH=1     skip `open -a "VZT Flow"` at the end
#   GITHUB_TOKEN    optional; only needed as a fallback if `gh` isn't
#                   installed and unauthenticated GitHub API requests are
#                   rate-limited (60/hr per IP), or for a private fork
set -euo pipefail

REPO="vonzelle-vzt/vzt-flow"
APP_NAME="VZT Flow.app"
INSTALL_YES="${INSTALL_YES:-0}"
NO_LAUNCH="${NO_LAUNCH:-0}"

log() { printf '==> %s\n' "$1"; }
warn() { printf 'warning: %s\n' "$1" >&2; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

# --- platform check -----------------------------------------------------

OS_KIND="$(uname -s)"
ARCH="$(uname -m)"

case "$OS_KIND" in
  Darwin)
    case "$ARCH" in
      arm64)
        DMG_PATTERN="*aarch64*.dmg"
        CLI_PATTERN="vzt-flow-cli-macos-aarch64.tar.gz"
        ;;
      x86_64)
        # Intel Mac: CPU-only inference (no Metal/CoreML) — see the README
        # hardware compat matrix. Built and packaged in CI, cross-compiled from
        # an arm64 runner; not verified on real Intel hardware.
        warn "Intel Mac detected — CPU-only inference (no Metal/CoreML), slower than Apple Silicon. See README for details."
        # Tauri names the x86_64 dmg with "x64", not "x86_64" (see
        # tauri-bundler's dmg/mod.rs: Arch::X86_64 => "x64") — e.g.
        # "VZT Flow_0.1.0_x64.dmg". The CLI tarball name is ours, not Tauri's.
        DMG_PATTERN="*_x64.dmg"
        CLI_PATTERN="vzt-flow-cli-macos-x86_64.tar.gz"
        ;;
      *)
        die "unsupported macOS architecture: $ARCH (VZT Flow ships arm64 and x86_64 builds only)"
        ;;
    esac
    ;;
  Linux)
    # Linux x86_64 only. Built and tested in CI but never run on real Linux
    # hardware — see the README hardware compat matrix and docs/USAGE-Linux.md
    # (X11 full, Wayland degraded). Install path handled by install_linux()
    # below, which exits before the macOS dmg/hdiutil code.
    case "$ARCH" in
      x86_64)
        CLI_PATTERN="vzt-flow-cli-linux-x86_64.tar.gz"
        ;;
      *)
        die "unsupported Linux architecture: $ARCH (VZT Flow ships an x86_64 Linux build only)"
        ;;
    esac
    ;;
  *)
    die "unsupported OS: $OS_KIND (macOS and Linux x86_64 only; Windows: scripts/install.ps1)"
    ;;
esac

WORKDIR="$(mktemp -d)"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

# --- download helpers -----------------------------------------------------
# Two download paths: `gh release download` (no auth needed for this public
# repo — works out of the box whether or not you've run `gh auth login`),
# or plain curl against the GitHub REST API, authenticated with
# GITHUB_TOKEN if set, otherwise falling back to an unauthenticated request
# (fine for occasional installs; GitHub rate-limits unauthenticated API
# calls to 60/hr per IP, which GITHUB_TOKEN lifts). Both branches leave
# files in $WORKDIR.

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
  # Repo is public — GITHUB_TOKEN is optional. Set it to lift GitHub's
  # 60/hr-per-IP unauthenticated API rate limit, or to fetch from a
  # private fork.
  local auth_header=()
  [ -n "$token" ] && auth_header=(-H "Authorization: Bearer ${token}")

  local api="https://api.github.com/repos/${REPO}/releases/latest"
  local assets_json
  assets_json="$(curl -fsSL "${auth_header[@]}" -H "Accept: application/vnd.github+json" "$api")" \
    || die "failed to query GitHub releases API for ${REPO} (rate-limited? set GITHUB_TOKEN) or install gh: https://cli.github.com"

  # Portable enough without jq: pull "browser_download_url" / "id" / "name"
  # lines and match by simple glob translated to a regex.
  local regex
  regex="$(printf '%s' "$pattern" | sed -e 's/\./\\./g' -e 's/\*/.*/g')"

  local name url
  while IFS=$'\t' read -r name url; do
    if [[ "$name" =~ ^${regex}$ ]]; then
      log "downloading $name"
      curl -fsSL "${auth_header[@]}" \
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

# --- Linux install path (deb / AppImage) ----------------------------------
# Debian/Ubuntu (dpkg + apt present) get the .deb; everything else falls back
# to the portable .AppImage dropped in ~/.local/bin. Then the shared flow CLI
# + MCP steps. Kept in a function invoked from the dispatch just below so the
# macOS dmg/hdiutil path further down stays byte-for-byte unchanged.
install_linux() {
  local kind bundle_pattern
  if command -v apt-get >/dev/null 2>&1 && command -v dpkg >/dev/null 2>&1; then
    kind="deb"; bundle_pattern="*.deb"
  else
    kind="appimage"; bundle_pattern="*.AppImage"
  fi

  log "fetching latest release assets for ${REPO} (Linux x86_64, ${kind})"
  fetch_asset "$bundle_pattern"
  fetch_asset "$CLI_PATTERN"

  local bundle tarball
  bundle="$(find "$WORKDIR" -maxdepth 1 \( -name '*.deb' -o -name '*.AppImage' \) | head -n1)"
  [ -n "$bundle" ] || die "download succeeded but no .deb/.AppImage found in $WORKDIR"
  tarball="$WORKDIR/$CLI_PATTERN"
  [ -f "$tarball" ] || die "download succeeded but CLI tarball not found in $WORKDIR"

  # --- install the desktop app ---
  if [ "$kind" = "deb" ]; then
    log "installing $(basename "$bundle") (apt, needs sudo)"
    if command -v sudo >/dev/null 2>&1; then
      # apt-get resolves the runtime deps (webkit2gtk, libayatana-appindicator,
      # alsa) from the package's control file; dpkg -i + apt-get -f is the
      # fallback if the direct-file install form isn't supported.
      sudo apt-get install -y "$bundle" \
        || { sudo dpkg -i "$bundle" || true; sudo apt-get -f install -y; }
    else
      warn "sudo not found — install the .deb manually: apt-get install -y '$bundle'"
    fi
  else
    local appdir="$HOME/.local/bin"
    mkdir -p "$appdir"
    install -m 0755 "$bundle" "$appdir/vzt-flow.AppImage"
    log "installed AppImage to $appdir/vzt-flow.AppImage"
    warn "AppImage needs FUSE at runtime (apt: libfuse2) and the tray needs libayatana-appindicator3 — see docs/USAGE-Linux.md."
  fi

  # --- install the flow CLI ---
  log "extracting CLI tarball"
  tar -xzf "$tarball" -C "$WORKDIR"
  local cli_src
  cli_src="$(find "$WORKDIR" -maxdepth 1 -type d -name 'vzt-flow-cli-*' | head -n1)"
  [ -n "$cli_src" ] || die "CLI tarball did not contain the expected vzt-flow-cli-* directory"

  local cli_dest="/usr/local/bin"
  if [ ! -w "$cli_dest" ] 2>/dev/null; then
    cli_dest="$HOME/.local/bin"
    mkdir -p "$cli_dest"
  fi
  log "installing flow CLI to $cli_dest"
  install -m 0755 "$cli_src/bin/flow" "$cli_dest/flow"
  if [[ ":$PATH:" != *":$cli_dest:"* ]]; then
    warn "$cli_dest is not on your PATH — add this to your shell profile:"
    warn "  export PATH=\"$cli_dest:\$PATH\""
  fi

  # --- install the MCP server (same layout as the macOS path) ---
  local mcp_dest="$HOME/.vzt-flow/mcp"
  log "installing MCP server to $mcp_dest"
  mkdir -p "$HOME/.vzt-flow"
  rm -rf "$mcp_dest"
  cp -R "$cli_src/mcp/dist" "$mcp_dest"
  [ -d "$cli_src/mcp/node_modules" ] && cp -R "$cli_src/mcp/node_modules" "$mcp_dest/node_modules"
  [ -f "$cli_src/mcp/package.json" ] && cp "$cli_src/mcp/package.json" "$mcp_dest/package.json"
  cp "$cli_src/mcp/README-snippet.md" "$HOME/.vzt-flow/mcp-README.md" 2>/dev/null || true

  local mcp_entry="$mcp_dest/index.js"
  if [ -f "$mcp_entry" ] && command -v claude >/dev/null 2>&1; then
    log "registering vzt-flow with claude mcp"
    claude mcp remove vzt-flow --scope user >/dev/null 2>&1 || true
    claude mcp add vzt-flow --scope user -- node "$mcp_entry" \
      || warn "failed to register MCP server via 'claude mcp add' — retry:\n  claude mcp add vzt-flow --scope user -- node \"$mcp_entry\""
  else
    [ -f "$mcp_entry" ] || warn "MCP entry file not found at $mcp_entry — skipping claude mcp registration"
    command -v claude >/dev/null 2>&1 || log "claude CLI not found — skipping MCP registration (install later with: claude mcp add vzt-flow --scope user -- node \"$mcp_entry\")"
  fi

  cat <<'EOF'

VZT Flow is installed (Linux — built/tested in CI, not yet validated on real
Linux hardware; see docs/USAGE-Linux.md).

Runtime notes (full X11-vs-Wayland support matrix in docs/USAGE-Linux.md):
  - Hold-to-talk hotkey (Ctrl+Shift+Space) works on X11. On Wayland it only
    fires while an XWayland-backed window is focused — the tray's Start/Stop
    item works on both.
  - The tray icon needs libayatana-appindicator3 installed
    (apt: libayatana-appindicator3-1).
  - Paste: on Wayland the transcript is left on the clipboard and you press
    Ctrl+V (synthetic paste can't reach native Wayland apps). On X11 it pastes
    automatically.
  - Grant microphone access; there is no accessibility grant to make on Linux.
  - Meeting mode is not yet available on Linux (needs a PipeWire capture
    backend — on the roadmap).

CLI:  flow --help    (run 'flow doctor' first)
MCP:  claude mcp list   (should show "vzt-flow")
EOF
}

# Dispatch the Linux path here; the macOS dmg/hdiutil code below is never
# reached on Linux.
if [ "$OS_KIND" = "Linux" ]; then
  install_linux
  exit 0
fi

# --- download release assets ----------------------------------------------

log "fetching latest release assets for ${REPO} (arch: $ARCH)"
# Two .dmg assets ship per release (aarch64 + x86_64) — fetch the one
# matching this machine's arch specifically, not a bare "*.dmg" glob, or
# we'd risk grabbing the wrong one when both are present in $WORKDIR.
fetch_asset "$DMG_PATTERN"
fetch_asset "$CLI_PATTERN"

DMG_PATH="$(find "$WORKDIR" -maxdepth 1 -name '*.dmg' | head -n1)"
[ -n "$DMG_PATH" ] || die "download succeeded but no .dmg found in $WORKDIR"
TARBALL_PATH="$WORKDIR/$CLI_PATTERN"
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
# Flatten dist/'s contents directly into $MCP_DEST so the entry point is
# ~/.vzt-flow/mcp/index.js (not .../mcp/dist/index.js) — that's the path
# `claude mcp add` below registers.
cp -R "$CLI_SRC_DIR/mcp/dist" "$MCP_DEST"
# node_modules is bundled alongside dist/ in the release tarball — the
# compiled JS imports @modelcontextprotocol/sdk and zod at runtime, so it
# won't run standalone without them. Place it as a sibling of index.js so
# Node's module resolution finds it immediately.
[ -d "$CLI_SRC_DIR/mcp/node_modules" ] && cp -R "$CLI_SRC_DIR/mcp/node_modules" "$MCP_DEST/node_modules"
[ -f "$CLI_SRC_DIR/mcp/package.json" ] && cp "$CLI_SRC_DIR/mcp/package.json" "$MCP_DEST/package.json"
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
