# VZT Flow — Windows Usage Guide

> [!WARNING]
> **Status: EXPERIMENTAL, first real-hardware run 2026-07-10.** The Windows
> build compiles and is built by CI (`.github/workflows/build.yml`'s
> `windows` job runs `cargo test --release --workspace` and `cargo tauri
> build` on `windows-latest` for every push/PR to `main`), and the v0.3.1
> release has now been exercised once on real Windows hardware (Windows 11
> Pro, x64) — see [First real-hardware test
> results](#first-real-hardware-test-results-2026-07-10) below for exactly
> what worked and what didn't (headline: everything up to and including the
> daemon worked; the **auto-paste silently did nothing** — fixed on `main`
> after that run). Everything else below is either verified directly against
> the code (marked as such) or an honest "this is what the code does,
> untested in practice" statement. If you try it, please report back — see
> [Help us test](#help-us-test) at the bottom.

## First real-hardware test results (2026-07-10)

One full pass of the v0.3.1 release artifacts on a real Windows 11 Pro x64
machine (Ryzen 7 7735HS). What was observed:

| Area | Result |
|---|---|
| NSIS installer (`VZT.Flow_0.3.1_x64-setup.exe`, silent `/S`) | **Works.** Per-user install to `%LOCALAPPDATA%\VZT Flow\`, exit 0. (Downloaded via `gh`, so no mark-of-the-web — the SmartScreen question is still open for browser downloads.) |
| CLI tarball layout | **Matches `install.ps1`'s expectations** (`bin\flow.exe`, `mcp\dist`, `mcp\node_modules`, `mcp\package.json`). |
| `flow doctor` | Runs; found the mic (AMD Audio Device), ffmpeg, `%APPDATA%\vzt-flow` config root. Two display bugs found and fixed on `main`: it printed the macOS "Right Option (keycode 61)" hotkey and gated the daemon check on a `daemon.sock` file that never exists on Windows (reporting "not present" against a live daemon). |
| `flow models download parakeet-v3` | **Works** (456 MB → `%APPDATA%\vzt-flow\models\parakeet-v3`). |
| `flow transcribe` (8.09 s TTS wav) | **Works, verbatim transcript.** 1.06 s wall, **RTF 0.131x** on plain CPU ONNX, model load 3.02 s. |
| Desktop app + named-pipe daemon | **Works end to end** — first time observed outside CI. App runs from the tray, `\\.\pipe\vzt-flow-daemon` exists, `flow status`/`toggle`/`cancel`/`listen` all reach it. The `set_recv_timeout` blocking-read degradation (CLAUDE.md gotcha g) fires on real hardware too — it is the *normal* Windows path, not a CI quirk. |
| Ctrl+Shift+Space global hotkey | **Registers and fires** (verified with a synthetic tap → daemon state went `recording`; also verified by a human hold-to-talk). |
| Hands-free record → silence auto-stop → transcribe | **Works** through the daemon (13.9 s speaker-played clip picked up by the laptop mic, verbatim transcript). |
| MCP server (`transcribe_file` over stdio JSON-RPC) | **Works** against the daemon. |
| Clipboard save → set → restore | **Works** (original clipboard restored after dictation). |
| **Auto-paste (enigo Ctrl+V)** | **BROKEN in v0.3.1 — silently does nothing.** `paste_text` reports `Pasted`, but the focused app (Notepad, verified foreground, non-elevated) receives no input, while an identical raw `SendInput` VK_V Ctrl+V from another process pastes fine. Root cause: enigo's `Key::Unicode('v')` is delivered as a `KEYEVENTF_UNICODE`/`VK_PACKET` character event, which apps don't map onto the Ctrl+V accelerator. **Fixed on `main`** (`insert.rs` now sends `Key::Other(0x56)` — VK_V — on Windows); v0.3.1 users get the transcript on the clipboard and must paste manually. |

## Hardware requirements

- **CPU**: x86_64 always works (built + CI-tested). arm64 (Windows on Arm)
  is attempted (`windows-arm64` job in `.github/workflows/build.yml`, marked
  `continue-on-error: true`) — whether it actually produces a working build
  depends on whether `ort` (ONNX Runtime) and Tauri's WebView2 host have
  usable Windows-arm64 support this week; check the latest `build` workflow
  run for current status rather than trusting this doc to stay perfectly in
  sync. The cleanup LLM (`llama-cpp-2`) is entirely macOS-only already (see
  [Differences vs. macOS](#differences-vs-macos) below), so arm64 Windows
  isn't blocked by that dependency the way Intel Mac is.
- **OS floor**: Windows 10 version 1803 (April 2018 Update) or newer, or
  Windows 11 — this is WebView2's own minimum, not something VZT Flow adds
  on top ([Tauri's WebView2 docs](https://v2.tauri.app/reference/webview-versions/)).
  WebView2 ships as part of the OS from 1803 onward; on anything older, the
  NSIS/MSI installer bootstraps it. Unverified on real hardware either way —
  see the warning banner above.
- **Memory/disk**: same models as macOS (Parakeet ~456MB, no cleanup LLM
  applicable here) — see [USAGE-macOS.md's Hardware requirements
  section](USAGE-macOS.md#hardware-requirements) for the general shape of
  the numbers; Windows-specific measurements don't exist since this hasn't
  run on real hardware.

## Install

### Option A: download the CI-built installer

Every push/PR to `main` uploads a Windows artifact named
`vzt-flow-windows-x64-installers`, containing whatever `.msi`/`.exe`
installers Tauri's `x86_64-pc-windows-msvc` bundle target produced — by
Tauri's own `{productName}_{version}_{arch}[_{lang}]` naming convention,
that's expected to be named `VZT Flow_0.2.0_x64_en-US.msi` (WiX/MSI
installer) and/or `VZT Flow_0.2.0_x64-setup.exe` (NSIS installer).

```bash
gh run list --workflow=build.yml --branch=main --status=success --limit 1
gh run download <run-id> --name vzt-flow-windows-x64-installers --dir ./vzt-flow-win
```

Or via the Actions UI: repo → **Actions** → **build** → a green run →
**Artifacts** → `vzt-flow-windows-x64-installers`.

Run either the `.msi` or the `.exe` to install. Since the CI build is
**unsigned** (no code signing in CI — see the workflow comment `"tauri build
(x64, unsigned — no code signing in CI)"`), expect SmartScreen/Defender to
flag it as an unrecognized publisher; you'll need to click through an
"install anyway" prompt.

### Option B: one-line installer (tagged releases only)

```powershell
iwr https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.ps1 -UseBasicParsing | iex
```

Downloads the latest **tagged** GitHub Release's `.msi`/`.exe` (see
`.github/workflows/release.yml`'s `windows` job — distinct from the
per-push `build.yml` artifact in Option A above) and runs it, then also
downloads and installs the `flow` CLI + MCP server from that same release's
`vzt-flow-cli-windows-x86_64.tar.gz` asset: `flow.exe` lands in
`%LOCALAPPDATA%\Programs\vzt-flow\bin` (added to your User PATH — restart
your terminal to pick it up) and the MCP server in
`%APPDATA%\vzt-flow\mcp`, registered with `claude mcp add` if the `claude`
CLI is on PATH. By default it also downloads the Parakeet ASR model
(`-InstallModels asr`) so the install is usable end to end; pass
`-InstallModels none` to skip. Windows on Arm is skipped for the CLI/MCP
step (that tarball is x86_64-only today — see release.yml) and falls back
to the manual build-from-source path below.

For unattended/agent-driven installs, pass `-Silent` (or set
`$env:INSTALL_SILENT=1`) to run the NSIS installer with `/S` instead of the
interactive wizard — nothing can click through the wizard in a scripted run.
The silent per-user install path was verified working on real Windows 11
(2026-07-10).

`scripts/install.ps1` itself is not exercised by CI (there is no Windows
runner running this script); its individual steps (silent NSIS install, CLI
tarball layout, MCP `claude mcp add` registration, model download) were each
exercised once on real Windows hardware on 2026-07-10 — see the test-results
table at the top of this doc.

### Option C: build from source

```powershell
git clone https://github.com/vonzelle-vzt/vzt-flow.git
cd vzt-flow

cargo build --release -p flow-cli
.\target\release\flow.exe doctor
.\target\release\flow.exe models download parakeet-v3
.\target\release\flow.exe models download cleanup   # optional — see below, likely doesn't work

cd apps\desktop
npm install
cargo install tauri-cli --version "^2"
cargo tauri build --target x86_64-pc-windows-msvc
```

As on macOS, the Cargo-workspace bundle output lands under the
**workspace-root** `target\x86_64-pc-windows-msvc\release\bundle\`, not
`apps\desktop\src-tauri\target\...`, even though the build is invoked from
`apps\desktop`.

## Hotkey

**Verified in code** (`apps/desktop/src-tauri/src/coordinator.rs`,
`spawn_hotkey_monitor` under `#[cfg(target_os = "windows")]`): the Windows
binding is **Ctrl+Shift+Space**, registered via
`tauri_plugin_global_shortcut::Shortcut::new(Some(Modifiers::CONTROL |
Modifiers::SHIFT), Code::Space)`, plugged in through
`tauri-plugin-global-shortcut` (only added to the Tauri builder on Windows —
`apps/desktop/src-tauri/src/lib.rs`).

This is **not** a modifier-only binding like macOS's Right Option, because
`tauri-plugin-global-shortcut` doesn't support registering a bare modifier
key as a shortcut on Windows. Ctrl+Shift+Space is a normal key-combo
shortcut instead, which the plugin supports fine.

**Hold/tap semantics: the same press/release state machine as macOS applies**
— confirmed in code, not assumed. The plugin's `on_shortcut` callback maps
`ShortcutState::Pressed`/`Released` straight onto the same
`HotkeyEvent::HoldKeyPressed`/`HoldKeyReleased` events macOS's `CGEventTap`
emits, and both platforms feed the *same* `run_coordinator` state machine in
`coordinator.rs`. So in principle:
- **Hold Ctrl+Shift+Space** to record, release to transcribe+paste.
- **Tap** (press+release faster than `hold_threshold_ms`, 300ms default) to
  toggle a hands-free recording, which auto-stops after
  `handsfree_silence_secs` (2.5s default) of silence.
- The same `max_hold_secs` (600s) / `max_handsfree_secs` (600s) caps apply.

Untested caveat: `tauri-plugin-global-shortcut`'s Windows implementation may
deliver press/release differently than macOS's raw `CGEventTap` under key
auto-repeat or focus-loss edge cases — this has not been exercised on real
hardware, so treat the hold/tap timing as "should work per the shared state
machine" rather than "verified working."

**No Escape-to-cancel on Windows** — verified in code and in the
coordinator's own doc comment: a *registered* global Escape shortcut would
swallow Escape system-wide (unlike macOS's `ListenOnly` tap, which never
consumes it), which is an unacceptable UX tradeoff. Use the tray's
**Start/Stop dictation** item to end a recording early instead — that always
works, on both platforms, since it doesn't go through the hotkey path at all.

If the shortcut fails to register (e.g. another app already owns
Ctrl+Shift+Space), the app logs `failed to register global hotkey
Ctrl+Shift+Space: ...` and continues running with the hotkey inactive — the
tray toggle is unaffected.

## Differences vs. macOS

All verified directly against the code (not carried over from the macOS
guide unchanged):

- **Daemon control socket: supported, over a named pipe.** `flow_core::ipc`
  now has a Windows transport (`ipc::windows`, `#[cfg(windows)]`) alongside
  the Unix domain socket one — it uses the `interprocess` crate's
  `local_socket` abstraction to open a native named pipe at
  `\\.\pipe\vzt-flow-daemon`, ACL'd to the current user session by default
  (equivalent in spirit to the Unix socket's `0600` chmod). The desktop
  app's `daemon::spawn` binds this pipe and runs the same accept-loop
  contract as the Unix `serve` (one connection at a time), so `flow
  status`/`toggle`/`cancel`/`listen` and the MCP server's daemon path
  (`mcp/src/daemon.ts`, which connects to the same `\\.\pipe\...` path via
  Node's `net.createConnection({ path })`) are all daemon-first on Windows
  the same way they are on macOS.
  **What's proven vs. not:** CI (`.github/workflows/build.yml`'s `windows`
  job) compiles this and runs `cargo test --release --workspace` on
  `windows-latest`, including windows-gated unit tests
  (`crates/flow-core/src/ipc.rs`'s `windows_tests` module) that bind/serve/
  connect/round-trip a request over a real named pipe on that runner. What
  CI does **not** exercise: the full desktop-app-as-daemon path end to end
  (spawn the `.exe`, run `flow status` against it, actually record+paste
  through the daemon) — that's still unverified on real Windows hardware,
  same caveat as the rest of this doc.
- **`flow` CLI verbs are daemon-first, same as macOS.** `flow listen`
  connects to the daemon pipe first and only falls back to the
  `run_standalone` path (`AudioRecorder::record_until_enter`, waiting for
  Enter) if nothing answers; `flow status`/`toggle`/`cancel` talk to the
  daemon pipe and report "daemon not running (or unreachable)" only if
  there's genuinely no listener; `flow history` still works standalone by
  reading `history.jsonl` directly either way.
- **No frontmost-app profiles — the default profile always applies.**
  `flow_core::permissions::frontmost_bundle_id()` is `#[cfg(not(target_os =
  "macos"))]`-gated to always return `None` on Windows (the macOS
  implementation uses `NSWorkspace`, which doesn't exist on Windows).
  `Profiles::resolve(None)` always returns `profiles.toml`'s `[default]`
  rule (`clean`/`neutral` unless you edit it), so the per-app `code`/`clean`
  overrides in the seeded `profiles.toml` (Terminal/iTerm/Warp/Mail/Slack —
  all macOS bundle IDs anyway) never match on Windows. Edit `[default]`
  directly if you want a different mode/tone as your one Windows-wide
  setting.
- **Paste is Ctrl+V via enigo, no secure-field detection.**
  `insert.rs`'s `paste_modifier()` returns `Key::Control` on any non-macOS
  target (vs. `Key::Meta`/Cmd on macOS), and the `secure_input_enabled()`
  check that skips pasting into password fields on macOS
  (`IsSecureEventInputEnabled`) is `#[cfg(not(target_os = "macos"))]`-gated
  to always return `false` on Windows — there's no equivalent OS API being
  queried, so VZT Flow will always *attempt* the Ctrl+V paste on Windows,
  including into fields a human would consider "secure." The transcript is
  still saved to the clipboard first either way (as on macOS), so worst case
  a failed/blocked synthetic paste just leaves it there for a manual paste.
- **Elevated/admin windows may silently block the paste.** Also from
  `insert.rs`'s own doc comment: Windows has UIPI (User Interface Privilege
  Isolation), which silently drops a lower-privilege process's synthetic
  input when the target window runs elevated — and enigo has no way to
  detect or report that this happened. VZT Flow does not attempt to work
  around this; the transcript is left on the clipboard as the fallback in
  every case, so the practical effect is "sometimes you'll need to paste
  manually into an elevated app, with no on-screen indication of why the
  automatic paste didn't happen."
- **No TCC-equivalent permission gate.** `accessibility_trusted()` is
  hardcoded to always return `true` on non-macOS — Windows has no
  Accessibility-style one-time grant to check, so paste is never skipped
  for "permission not granted" the way it can be on macOS. (It can still
  silently fail for the UIPI reason above — that's a different failure
  mode with no corresponding permission to grant.)
- **Config path is `%APPDATA%\vzt-flow`,** not `~/.config/vzt-flow`.
  Verified in `crates/flow-core/src/config.rs::config_dir()`: the macOS
  branch hardcodes the literal `~/.config/vzt-flow` path (deliberately, to
  match existing installs and doc comments, rather than macOS's usual
  `~/Library/Application Support`); every other target — Windows included —
  uses `dirs::config_dir()`, which resolves to `%APPDATA%` on Windows, joined
  with `vzt-flow`. So on Windows, `config.toml`, `dictionary.json`,
  `profiles.toml`, `snippets.json`, `history.jsonl`, and `models\` all live
  under `%APPDATA%\vzt-flow\` (typically
  `C:\Users\<you>\AppData\Roaming\vzt-flow\`).
- **Cleanup (`clean`/`polish`) LLM is macOS-only for now.** Verified in
  `crates/flow-core/src/cleanup.rs`: the entire `LlamaCleanupProvider`
  (embedded llama.cpp) implementation lives behind
  `#[cfg(target_os = "macos")]`. Both call sites that would use it on other
  platforms are explicitly gated to fail gracefully instead of trying to
  compile a non-existent provider:
  - The desktop app's daemon-driven pipeline
    (`crates/flow-core/src/cleanup_manager.rs::load_provider`) has a
    `#[cfg(not(target_os = "macos"))]` fallback that returns
    `Err("embedded llama.cpp cleanup provider is only implemented for
    macOS")` — meaning on Windows, `clean`/`polish` mode always falls back
    to the dictionary-corrected raw transcript; the cleanup model never
    loads at all, downloaded or not. `code` and `raw` mode are unaffected
    (code mode is a pure deterministic transform; raw mode never touches
    the LLM on any platform).
  - `flow-cli`'s standalone pipeline
    (`crates/flow-cli/src/commands/listen.rs::apply_standalone_pipeline`)
    has the identical `#[cfg(not(target_os = "macos"))]` branch, which just
    returns the dictionary-corrected text unchanged for any mode other than
    `code`/`raw`.
  - Practical effect: `flow models download cleanup` will still download the
    GGUF file on Windows (the download logic itself isn't platform-gated),
    but it's inert — nothing on Windows ever loads it. Don't bother
    downloading it there today.
- **CPU-only ONNX inference (no CoreML/Metal).** Parakeet transcription runs
  through `transcribe-rs`'s ONNX runtime the same way on every platform;
  only macOS gets the CoreML execution provider (added via a
  `target_os = "macos"`-scoped Cargo dependency section — see
  `crates/flow-core/Cargo.toml`). Windows transcription is plain CPU ONNX,
  so expect a slower realtime factor than on Apple Silicon.

## Known limitations

- ~~Daemon control socket (named pipe) not yet exercised end to end on real
  Windows hardware~~ — **verified working 2026-07-10** (see [First
  real-hardware test results](#first-real-hardware-test-results-2026-07-10)):
  desktop-app-as-daemon + `flow status`/`toggle`/`cancel`/`listen` + the MCP
  server's daemon path all round-trip through a really-running app.
- **v0.3.1's auto-paste is broken** (silently leaves the transcript on the
  clipboard only — paste manually with Ctrl+V). Fixed on `main`; see the
  test-results table above.
- No per-app profiles → one mode/tone for everything (edit `profiles.toml`'s
  `[default]`).
- No `clean`/`polish` LLM rewrite — only `raw` and `code` modes do anything
  beyond dictionary correction.
- No Escape-to-cancel; use the tray's Start/Stop item.
- No secure-field paste protection.
- Elevated-window paste can silently fail (transcript still lands on the
  clipboard).
- Tested on real Windows hardware exactly once (2026-07-10, one machine) —
  the SmartScreen/Defender prompt on a browser-downloaded unsigned installer
  and long-term stability remain unverified.

## Help us test

If you're running this on real Windows hardware, we'd like to know:
- Does the Ctrl+Shift+Space hold/tap distinction actually feel right, or
  does `tauri-plugin-global-shortcut` deliver press/release differently than
  macOS's `CGEventTap` in practice?
- Does the unsigned installer's SmartScreen/Defender prompt block install
  outright, or just warn?
- Does paste into a normal (non-elevated) app work reliably via enigo's
  Ctrl+V simulation?
- Does the daemon named pipe actually work end to end: launch the desktop
  app, then run `flow status` / `flow toggle` / `flow listen` from a
  separate terminal — do they reach the running app, or fail to connect?
  Same question for the MCP server's daemon path.
- Anything that crashes, hangs, or silently does nothing.

Open an issue (or, if you have write access, a note in the repo) with what
you found — this guide will be updated as real-hardware reports come in.
