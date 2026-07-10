# VZT Flow — Windows Usage Guide

> [!WARNING]
> **Status: EXPERIMENTAL, but now proven on real hardware (2026-07-10).**
> The Windows build is compiled and tested by CI on every push
> (`.github/workflows/build.yml`'s `windows` job), and has now been
> exercised end to end on a real Windows 11 machine — install, model
> download, transcription, the hotkey, the daemon, the MCP server, and a
> real human dictating by voice all work. See [First real-hardware test
> results](#first-real-hardware-test-results-2026-07-10) for the full
> matrix. One release-blocking bug was found in that run: **v0.3.1's
> auto-paste silently does nothing** (your words still land on the
> clipboard — press Ctrl+V). That is fixed on `main` — and the fix was
> verified the same day on the same machine (a CI build of `main`
> auto-pasted a dictation into a focused Notepad) — and will ship in the
> next release. Everything below is either verified on real hardware
> (marked as such) or verified directly against the code. If you try it,
> please report back — see [Help us test](#help-us-test) at the bottom.

## Quick start

What you need: **64-bit Windows 10 (version 1803+) or Windows 11**, a
microphone, about **1.2 GB of disk** (app + speech model), and PowerShell.
No admin rights — everything installs per-user.

**1. Install.** Paste this into PowerShell:

```powershell
iwr https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.ps1 -UseBasicParsing | iex
```

It downloads the latest release and installs the desktop app, the `flow`
command-line tool, the MCP voice-input server for
[Claude Code](https://claude.com/claude-code), and the Parakeet speech
model (456 MB) — then starts the app. Details and alternatives:
[Install](#install) below. The installer is unsigned, so if Windows shows
"Windows protected your PC," click **More info → Run anyway**.

**2. Open a new terminal** (the just-installed `flow` command only appears
on the PATH of terminals started after the install).

**3. Hold Ctrl+Shift+Space, say a sentence, let go.** The app lives in the
system tray (bottom-right, near the clock) — there is no main window. On
v0.3.1 you then press **Ctrl+V** to paste what you said (see the warning
above); from the next release onward the text lands at your cursor by
itself.

There is no account, no license key, and nothing that talks to the network
after the model download. Sanity-check any time with:

```powershell
flow doctor
```

## Dictating on Windows: the day-to-day guide

Two gestures, one shortcut:

| Gesture | Mode | Stops when |
|---|---|---|
| **Hold** Ctrl+Shift+Space, talk, release | Push-to-talk | You release the keys |
| **Tap** Ctrl+Shift+Space (<300 ms), talk | Hands-free | ~2.5 s of silence, or a second tap |

Both verified working on real hardware, including by an actual human
dictating (2026-07-10).

Things that behave differently from the macOS guide — worth reading once:

- **The hotkey cannot be changed on Windows.** The hotkey picker in the
  Settings window configures the macOS `hotkey_keycode` only; on Windows
  the app always registers the fixed **Ctrl+Shift+Space** global shortcut,
  and changing the setting does nothing (a real user hit exactly this on
  2026-07-10 — set it to "Fn", then pressed Fn and nothing happened;
  Ctrl+Shift+Space still worked the whole time).
- **No Escape-to-cancel.** Use the tray icon's **Start/Stop dictation**
  item to end or abandon a recording early, or `flow cancel` from a
  terminal. (Registering a global Escape shortcut would swallow Escape
  system-wide for every app — not worth it.)
- **Your transcript is always on the clipboard**, whether or not the
  auto-paste lands. If text ever fails to appear (elevated/admin windows
  can silently block synthetic input — see
  [Differences vs. macOS](#differences-vs-macos)), just press Ctrl+V.
- **`clean` and `polish` modes fall back to raw on Windows** — the local
  cleanup LLM is macOS-only today, so what you get is the raw Parakeet
  transcript plus your personal-dictionary corrections. `code` mode (the
  deterministic spoken-form → `camelCase`/symbols transform) works fully.
- **One mode/tone for everything.** Per-app profiles need macOS's frontmost
  -app API; on Windows the `[default]` rule in `%APPDATA%\vzt-flow\profiles.toml`
  always applies. Edit it to change your Windows-wide mode (e.g. `mode =
  "code"` if you mostly dictate into a terminal).
- **Your dictation history** is in `flow history` (or
  `%APPDATA%\vzt-flow\history.jsonl`) — useful when you dictated into the
  void and want the text back.
- **Short clips mishear more.** A 2-second fragment over a laptop mic array
  is the hardest ASR case; full sentences at a normal pace transcribe
  dramatically better (a 14 s test clip came back verbatim on the same
  machine that fumbled 2-second snippets).

The personal dictionary (`dictionary.json`) and snippets (`snippets.json`)
work exactly as on macOS — same files, same behavior, in
`%APPDATA%\vzt-flow\`.

## Troubleshooting

Ordered by how often each one is the answer:

**1. `flow` is not recognized.** The installer adds
`%LOCALAPPDATA%\Programs\vzt-flow\bin` to your *User* PATH, which only
applies to terminals opened afterwards. Open a new terminal, or call it by
full path: `& "$env:LOCALAPPDATA\Programs\vzt-flow\bin\flow.exe" doctor`.

**2. I press Ctrl+Shift+Space and nothing happens.**
- Is the app running? It has no window — look for the tray icon
  (bottom-right, possibly behind the `^` overflow chevron), or run
  `flow status`: if it says "daemon not running," start the app from the
  Start menu (search "VZT Flow").
- Hold the hotkey while watching `flow status` in a terminal — if `state`
  never leaves `idle`, another app probably owns Ctrl+Shift+Space (the app
  logs `failed to register global hotkey` and keeps running without it).
  The tray's Start/Stop dictation item still works regardless.
- If you changed the hotkey in Settings: that setting is macOS-only — the
  Windows shortcut is always Ctrl+Shift+Space (see above).

**3. It records and transcribes, but no text appears.**
- On **v0.3.1** that's the known auto-paste bug — your words are on the
  clipboard, press **Ctrl+V**. Fixed on `main`/next release.
- On newer builds: is the target window running elevated (as
  administrator)? Windows silently drops synthetic input into elevated
  windows (UIPI) — the transcript is still on the clipboard.
- `flow history` shows what was transcribed either way.

**4. The overlay says the speech model is missing.** Run
`flow models download parakeet-v3` (456 MB). Don't bother with
`flow models download cleanup` on Windows — the cleanup LLM never loads
here (macOS-only).

**5. SmartScreen / "Windows protected your PC" on install.** The installers
are unsigned (no code signing in CI yet). Click **More info → Run anyway**,
or install via the PowerShell one-liner, which downloads without the
mark-of-the-web browser downloads get.

**6. Still stuck?** `flow doctor` prints the model, mic, daemon, and MCP
registration state in one shot — include its output in a bug report. (On
v0.3.1, ignore its "Right Option" hotkey line and "Daemon socket: not
present" line — both were display bugs on Windows, fixed on `main`.)

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
| **Auto-paste (enigo Ctrl+V)** | **BROKEN in v0.3.1 — silently does nothing.** `paste_text` reports `Pasted`, but the focused app (Notepad, verified foreground, non-elevated) receives no input, while an identical raw `SendInput` VK_V Ctrl+V from another process pastes fine. Root cause: enigo's `Key::Unicode('v')` is delivered as a `KEYEVENTF_UNICODE`/`VK_PACKET` character event, which apps don't map onto the Ctrl+V accelerator. **Fixed on `main`** (`insert.rs` now sends `Key::Other(0x56)` — VK_V — on Windows) **and the fix verified on the same machine the same day**: a CI build of `main` auto-pasted a full dictation into a focused Notepad. v0.3.1 users get the transcript on the clipboard and must paste manually. |

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
  NSIS/MSI installer bootstraps it. Verified on Windows 11 (2026-07-10);
  Windows 10 remains untested.
- **Memory/disk**: same models as macOS (Parakeet ~456MB download, ~640MB
  on disk; skip the cleanup LLM — it never loads on Windows). Measured on
  real hardware (Ryzen 7 7735HS, 2026-07-10): an 8.09s clip transcribed in
  1.06s wall (**RTF 0.131x**, ~7.6x realtime) on plain CPU ONNX, with a
  3.02s one-time model load. See [USAGE-macOS.md's Hardware requirements
  section](USAGE-macOS.md#hardware-requirements) for the general memory
  shape — the same lazy-load/idle-unload discipline applies.

## Install

### Option A: one-line installer (recommended)

```powershell
iwr https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.ps1 -UseBasicParsing | iex
```

This is the only path that leaves you with a fully working setup in one
step. It downloads the latest **tagged** GitHub Release and installs, in
order:

| Step | Where it lands |
|---|---|
| The desktop app (NSIS `-setup.exe`) | `%LOCALAPPDATA%\VZT Flow\` (per-user, no admin) |
| The `flow` CLI | `%LOCALAPPDATA%\Programs\vzt-flow\bin\flow.exe`, added to your **User PATH** — open a *new* terminal to use bare `flow` |
| The MCP server (voice input for Claude Code) | `%APPDATA%\vzt-flow\mcp`, registered via `claude mcp add` if the `claude` CLI is on PATH |
| The Parakeet speech model (456 MB) | `%APPDATA%\vzt-flow\models\parakeet-v3` |

Flags: `-InstallModels none` skips the model download; `-InstallYes` skips
the confirmation prompt; `-Silent` (or `$env:INSTALL_SILENT=1`) runs the
NSIS installer with `/S` instead of the interactive wizard — required for
unattended/agent-driven installs, since nothing can click through the
wizard in a scripted run. The silent per-user install path was verified
working on real Windows 11 (2026-07-10). Windows on Arm is skipped for the
CLI/MCP step (that tarball is x86_64-only today — see release.yml) and
falls back to the build-from-source path below.

`scripts/install.ps1` itself is not exercised by CI (there is no Windows
runner running this script); its individual steps (silent NSIS install, CLI
tarball layout, MCP `claude mcp add` registration, model download) were each
exercised once on real Windows hardware on 2026-07-10 — see the test-results
table at the top of this doc.

### Option B: download the CI-built installer (bleeding edge)

Use this when you want a fix that's on `main` but not yet in a tagged
release (as of this writing: the auto-paste fix). Every push/PR to `main`
uploads a Windows artifact named `vzt-flow-windows-x64-installers`,
containing the `.msi` (WiX) and `-setup.exe` (NSIS) installers Tauri's
`x86_64-pc-windows-msvc` bundle target produced.

```bash
gh run list --workflow=build.yml --branch=main --status=success --limit 1
gh run download <run-id> --name vzt-flow-windows-x64-installers --dir ./vzt-flow-win
```

Or via the Actions UI: repo → **Actions** → **build** → a green run →
**Artifacts** → `vzt-flow-windows-x64-installers`.

Run either the `.msi` or the `-setup.exe` to install (the NSIS `-setup.exe`
supports silent install with `/S`). Since the CI build is **unsigned** (no
code signing in CI), expect SmartScreen/Defender to flag a
browser-downloaded copy as an unrecognized publisher — click **More info →
Run anyway**. Note this artifact contains the **app only**: get the `flow`
CLI + MCP server by running the Option A one-liner too, or from a release's
`vzt-flow-cli-windows-x86_64.tar.gz`.

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

**Verified on real hardware (2026-07-10):** both gestures work — a
synthetic tap armed hands-free recording (daemon state went `recording`),
and a real human dictated by hold-to-talk repeatedly with correct hold/tap
discrimination. Auto-repeat and focus-loss edge cases haven't been
systematically probed, but ordinary use behaves exactly like the shared
state machine says it should.

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
  **Verified end to end on real hardware (2026-07-10):** with the desktop
  app running, `flow status`/`toggle`/`cancel`/`listen` and the MCP server
  all reached the daemon over the pipe, and a full record → silence
  auto-stop → transcribe cycle ran through it. One behavior to know:
  `set_recv_timeout` on named-pipe client streams fails ("named pipes do
  not support I/O timeouts") on real Windows just as it does on CI runners,
  so every daemon call logs a one-line
  `[vzt-flow] named pipe recv timeout unsupported, using blocking read`
  notice to stderr and proceeds with a blocking read — harmless, by design.
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
  clipboard only — paste manually with Ctrl+V). Fixed on `main` and
  re-verified on real hardware; see the test-results table above.
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

The 2026-07-10 real-hardware pass answered most of the original open
questions (hold/tap feels right: **yes**; daemon pipe works end to end:
**yes**; paste into a normal app: **broken in v0.3.1, fixed on `main` and
re-verified**). Still open — if you're running this on real Windows
hardware, we'd like to know:

- Does the unsigned installer's SmartScreen/Defender prompt block a
  **browser-downloaded** install outright, or just warn? (The tested
  install used `gh`/PowerShell downloads, which don't carry the
  mark-of-the-web, so SmartScreen never fired.)
- Hold/tap behavior under key auto-repeat and focus-loss edge cases
  (alt-tabbing away mid-hold, etc.).
- Behavior on Windows 10 (the tested machine was Windows 11), on Intel
  CPUs, and on machines with multiple audio input devices.
- Long-term stability: does the tray app stay healthy across days of use,
  sleep/resume, and monitor changes?
- Anything that crashes, hangs, or silently does nothing.

Open an issue (or, if you have write access, a note in the repo) with what
you found — this guide will be updated as real-hardware reports come in.
