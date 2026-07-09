# VZT Flow — Linux Usage Guide

> [!WARNING]
> **Status: EXPERIMENTAL — compiles and is CI-tested, never run on real Linux
> hardware.** The Linux build is compiled *and* unit-tested by CI
> (`.github/workflows/build.yml`'s `linux` job runs `cargo test --release
> --workspace` and `cargo tauri build` on `ubuntu-latest` for every push/PR to
> `main`, producing a `.deb` and an `.AppImage`), but all development happens
> on a macOS machine. Everything below is either verified directly against the
> code / a crate's source (marked as such) or an honest "this is what the code
> does, unvalidated on a real Linux desktop" statement. The single biggest
> unknown is the **display server**: X11 is well-supported by the underlying
> crates; Wayland is degraded. If you run it, please report back — see
> [Help us test](#help-us-test).

## Support matrix: X11 vs. Wayland

VZT Flow's OS-touching surfaces are the global hotkey, the synthetic paste,
the menu-bar tray, and the always-on-top overlay. Their behavior differs by
display server:

| Capability | X11 | Wayland |
|---|---|---|
| **Hold-to-talk hotkey** (Ctrl+Shift+Space) | ✅ Full — global grab, clean press/release | ⚠️ Degraded — only fires while an XWayland-backed window is focused; no global grab across native Wayland apps. If no X server is reachable at all, the hotkey is inactive |
| **Tray Start/Stop** (fallback trigger) | ✅ Works | ✅ Works — the display-server-independent way to dictate |
| **Auto-paste** (synthetic Ctrl+V) | ✅ Pastes into the focused app | ⚠️ Not reliable — transcript is left on the clipboard + a "press Ctrl+V" desktop notification. XTEST-into-Wayland injection is honored by GNOME/Mutter and KDE but not all compositors |
| **Tray icon** | ✅ Needs `libayatana-appindicator` (see below) | ✅ Same |
| **Overlay** (recording pill, always-on-top) | ✅ Standard override-redirect top-level | ⚠️ Wayland gives clients no say over stacking/position; the pill shows but "always on top / centered at bottom" is best-effort, compositor-dependent |
| **Meeting mode** (system-audio capture) | ❌ Not yet on Linux — see [Meeting mode](#meeting-mode-not-yet-on-linux) | ❌ Same |

**The honest summary:** on **X11** VZT Flow behaves like the Windows build —
hold-to-talk, auto-paste, tray, overlay all work as designed. On **Wayland**,
dictation still works end-to-end via the tray (or an XWayland-focused hotkey)
and the clipboard, but the "hold a key anywhere, transcript lands at the
cursor" magic is not fully available because Wayland deliberately denies
clients global input grabs and cross-app synthetic input. This is a Wayland
security-model constraint, not a bug we can paper over.

### Why Wayland is degraded (and what would fix it)

- **Hotkey.** The hold-to-talk monitor uses `tauri-plugin-global-shortcut`,
  which wraps the `global-hotkey` crate. As of `global-hotkey` v0.8, its only
  Linux backend is X11 (`XGrabKey` on the root window). Verified against the
  crate source (`src/platform_impl/x11/mod.rs`): it even enables xkb
  *detectable auto-repeat* and latches a `pressed` flag, so a held key yields
  exactly one press and one release — perfect for hold-to-talk — but only on
  X11. Under Wayland it connects to the X server exposed by **XWayland**, so
  the grab only sees events routed to X11/XWayland windows.
  The Wayland-native path would be the
  `org.freedesktop.portal.GlobalShortcuts` XDG desktop portal, which
  `global-hotkey` does **not** implement in this version. Wiring that portal
  up (or a compositor-specific protocol) is the roadmap item that would make
  the hotkey global on Wayland.
- **Paste.** `enigo` (our input-simulation crate) defaults to its `x11rb`
  backend on Linux — synthetic input via the X11 **XTEST** extension. That
  reaches native Wayland clients only if the compositor forwards XTEST fake
  input (GNOME/KDE do; some wlroots compositors do not). The code detects a
  no-X-server session (`DISPLAY` unset) and, rather than firing a keystroke
  into the void, leaves the transcript on the clipboard and posts a desktop
  notification telling you to press Ctrl+V. The transcript is *always* placed
  on the clipboard first, on every platform, so a manual paste is the worst
  case even when auto-paste silently fails.

## Runtime dependencies

The desktop app is a Tauri 2 (WebKitGTK) menu-bar app. On a fresh desktop
install you need:

**Debian / Ubuntu:**

```bash
sudo apt-get install -y \
  libwebkit2gtk-4.1-0 \
  libayatana-appindicator3-1 \
  libgtk-3-0 \
  libasound2 \
  libfuse2            # only if you run the .AppImage (FUSE mount)
```

**Fedora:**

```bash
sudo dnf install -y \
  webkit2gtk4.1 \
  libappindicator-gtk3 \
  gtk3 \
  alsa-lib \
  fuse-libs           # only for the .AppImage
```

- **`libayatana-appindicator3`** is what makes the tray icon appear. Tauri 2's
  Linux tray uses the AppIndicator/StatusNotifierItem protocol; without this
  package (and, on some desktops, a StatusNotifier host — GNOME needs the
  *AppIndicator Support* extension) there is no tray icon and the app is
  effectively headless. The `.deb` declares this as a dependency so `apt`
  pulls it automatically; the `.AppImage` does **not**, so install it yourself.
- **ALSA** (`libasound2`) backs `cpal`'s microphone capture. PulseAudio /
  PipeWire both expose an ALSA-compatible device, so this works under all
  three; no PulseAudio-specific package is required.

## Install

### Option A: one-liner (Release build)

```bash
curl -fsSL https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.sh | bash
```

`scripts/install.sh` detects Linux, grabs the latest Release's `.deb` (on
Debian/Ubuntu, installed via `apt`) or `.AppImage` (everywhere else, dropped
in `~/.local/bin/vzt-flow.AppImage`), installs the `flow` CLI to a PATH
directory, and registers the MCP server with `claude mcp add` if the `claude`
CLI is present. It never touches `/Applications` (that's the macOS path) and
leaves the macOS install flow unchanged.

### Option B: download the CI-built bundle

Every push/PR to `main` uploads a Linux artifact named
`vzt-flow-linux-x64-bundles` containing the `.deb` and `.AppImage`:

```bash
gh run list --workflow=build.yml --branch=main --status=success --limit 1
gh run download <run-id> --name vzt-flow-linux-x64-bundles --dir ./vzt-flow-linux
```

Then `sudo apt-get install -y ./vzt-flow-linux/*.deb`, or `chmod +x
./vzt-flow-linux/*.AppImage && ./vzt-flow-linux/*.AppImage`.

Tagged releases (`v*`) additionally ship a standalone CLI tarball
(`vzt-flow-cli-linux-x86_64.tar.gz`: the `flow` binary + the MCP server),
mirroring the macOS tarball layout.

### Option C: build from source

```bash
# system deps (Debian/Ubuntu) — same as CI's linux job
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
  libasound2-dev build-essential curl wget file libxdo-dev libssl-dev libgtk-3-dev

cargo build --release -p flow-cli
./target/release/flow doctor
./target/release/flow models download parakeet-v3

cd apps/desktop && npm install && cargo install tauri-cli --version "^2"
cargo tauri build --target x86_64-unknown-linux-gnu
# bundles land under ../../target/x86_64-unknown-linux-gnu/release/bundle/{deb,appimage}/
```

## Hotkey

Default binding is **Ctrl+Shift+Space** (hold to talk), the same as the
Windows build — not macOS's Right Option. The plugin-based backend can't grab
a bare modifier key, so a normal combo is used. On X11 this is a global grab
that works regardless of which app is focused. On Wayland it only fires while
an XWayland-backed window is focused (see the [support matrix](#support-matrix-x11-vs-wayland)).

The tray's **Start / Stop dictation** item is the universal fallback and works
under any display server — if the hotkey doesn't register (pure Wayland, no X
server), that's your dictation trigger.

**Escape-to-cancel** is not wired to a global key on Linux (a globally
registered Escape would be swallowed for every app). Because the Unix daemon
control socket **is** available on Linux (unlike Windows), you can end a
recording early from a terminal:

```bash
flow cancel     # stop + discard the current recording
flow toggle     # start/stop like the tray item
flow status     # daemon + model state
```

## Differences vs. macOS

- **No cleanup LLM (`clean` / `polish` modes fall back to raw).** The embedded
  llama.cpp cleanup provider (`LlamaCleanupProvider`) is `#[cfg(target_os =
  "macos")]`-gated, exactly as on Windows. On Linux, `clean`/`polish` return
  the dictionary-corrected transcript without an LLM rewrite. `raw` and `code`
  modes are fully functional (they never use the LLM).
- **No per-app profiles.** Frontmost-app bundle-ID resolution is macOS-only,
  so `profiles.toml`'s `[default]` always applies.
- **No secure-field paste protection.** macOS's `IsSecureEventInputEnabled`
  check has no cross-platform equivalent; there's no permission gate to grant.
- **Paste on Wayland** is clipboard-only with a notification (see above).
- **Meeting mode** is unavailable — see below.

## Meeting mode (not yet on Linux)

`flow meeting` and the tray's meeting-transcription items return a clear error
on Linux:

> meeting mode is not yet available on Linux (needs a PipeWire system-audio
> capture backend — on the roadmap; macOS uses ScreenCaptureKit today)

Meeting mode captures the *other participants'* audio by tapping system/app
output, which on macOS uses ScreenCaptureKit. The Linux equivalent is a
**PipeWire** capture backend (a `pw-stream` on a monitor node, or the
`org.freedesktop.portal.ScreenCast` portal for audio). That backend isn't
built yet; the pure sub-modules (dedup, streaming chunker, transcript writer)
already compile cross-platform, so only the capture source is missing.

## Known limitations (Linux, as shipped)

- Wayland: no global hotkey across native apps; no reliable auto-paste;
  best-effort overlay stacking. Use the tray + clipboard.
- No cleanup LLM, no per-app profiles, no meeting mode (all as above).
- The tray requires `libayatana-appindicator3` and, on GNOME, the
  *AppIndicator Support* extension to be visible.
- `.AppImage` requires FUSE (`libfuse2`) and does not auto-pull the tray
  dependency — prefer the `.deb` on Debian/Ubuntu.
- **None of this has run on real Linux hardware.** Hold/tap timing, the
  XWayland hotkey edge cases, XTEST-into-Wayland paste behavior per compositor,
  tray rendering across desktops (GNOME/KDE/XFCE), and overlay stacking are all
  "verified in code / crate source, untested in practice."

## Help us test

If you run VZT Flow on Linux, the most useful things to report:

1. **Display server + desktop** (`echo $XDG_SESSION_TYPE`, and GNOME/KDE/XFCE/etc.).
2. **Hotkey**: does Ctrl+Shift+Space start/stop dictation globally? Only in
   XWayland windows? Not at all?
3. **Paste**: does the transcript land at the cursor, or only reach the
   clipboard? Which compositor?
4. **Tray**: does the menu-bar icon appear? Did you need an extension?
5. **`flow doctor`** output and any stderr from launching the app in a terminal.

File an issue at
<https://github.com/vonzelle-vzt/vzt-flow/issues> with the above.
