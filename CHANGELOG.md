# Changelog

All notable changes to VZT Flow are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses [semantic
versioning](https://semver.org/). Numbers quoted below were measured on this
repo's dev hardware (M5 MacBook Air) unless noted — see `README.md` /
`docs/PRD.md` for the full methodology.

## [0.3.4] — 2026-08-02

**A cancel that arrived at the wrong moment was thrown away, and the microphone
stayed on.** `flow status` would pin at `state: recording` with the mic light
lit, and nothing could talk it down — `flow cancel` returned `ok` and changed
nothing, `flow toggle` did the same. Quitting and reopening the app was the only
way out. It never affected hold-to-talk, only the tray toggle, the daemon socket
and the MCP `listen` path, which is why it went unnoticed for so long: the
gesture almost everyone uses is the one gesture that couldn't trigger it.

The app sets its state to "recording" at the moment it *asks* the audio worker
to start, rather than when the worker confirms it has — so the two are never
exactly in step. That window is normally harmless. What made it harmful is that
the worker answered a stop or cancel that arrived outside a recording by doing
nothing at all, silently: with no reply and no state change, a disagreement
between the two had nothing that could ever resolve it.

The fix is that the acknowledgement is no longer optional. A stop or cancel with
no recording in progress now answers `NotRecording`, and the app reconciles to
idle — but only if it still believes it is recording, so a late acknowledgement
can never tear down a transcription that is already underway.

### Fixed

- **A stuck recording can no longer strand the microphone.** The audio worker's
  outer command loop replied to `AudioCommand::Stop`/`Cancel` with `{}`; it now
  sends `AudioReply::NotRecording`, and the coordinator resets to Idle when it
  still holds `DictationState::Recording` (Transcribing and Idle are left
  alone). A cancel now always either ends a live recording or clears a stuck
  one. Pinned by `cancel_with_no_recording_is_acknowledged_not_swallowed`,
  which fails by timeout against the previous no-op arm.
- **Two `.lock().unwrap()` calls on `AppState` are gone** from the coordinator's
  audio-level and hands-free-cap paths, replaced with `lock_or_recover()`. A
  panic holding either mutex would poison it and make every later hotkey press
  panic on the same line — the exact failure the 0.3.3 supervisor exists to
  survive. No production `.lock().unwrap()` on `AppState` remains.

### Known

**This fix is not proven against the original symptom.** The stall was observed
directly and twice — state pinned at `recording` across minutes, multiple
`flow cancel`/`flow toggle` calls and two process samples, CoreAudio threads
live, both the coordinator and audio-worker threads idle in `recv`, cleared only
by restarting. The swallowed-command path above is a real defect and a
*sufficient* explanation for a stall that nothing can clear, which is why it was
fixed. It has not been demonstrated to be *the* cause, and the original stall has
not been reproducible on demand on any version.

An earlier draft of this entry cited "3 wedges in 8 toggle-then-cancel cycles".
**That figure is withdrawn**: it sampled state immediately after `flow cancel`
returned, and a start and cancel issued back-to-back leave state legitimately
`recording` for under a second while CoreAudio opens the input device. It was
measuring device-open latency, not a stuck state. Inserting a 1s delay before the
cancel gives 0/8, and on 0.3.4 a long-window measurement resolves to idle within
0–1s in 6 of 6 trials. Treat sub-second `recording` after a cancel as normal.

## [0.3.3] — 2026-07-29

**The app could die in the middle of a dictation, and you'd never be told.**
It quit silently — no dialog, no error, the menu-bar icon simply gone — so the
next time you held the hotkey, nothing happened. Every visible sign pointed at
a broken hotkey or a permissions problem. The actual cause was the auto-paste,
and it had been latent since the first macOS build.

There is a pleasing symmetry with 0.3.2: that release fixed `Key::Unicode('v')`
on Windows because it pasted *nothing*. This one fixes the same call on macOS
because it brought down the *process*. Same line of code, opposite failure,
found the same way — by running it on real hardware until it broke.

### Fixed

- **macOS auto-paste could abort the entire app.** `enigo`'s
  `Key::Unicode('v')` resolves the character against the live keyboard layout,
  sweeping keycodes 0–127 across two modifier states — **256
  `TISCopyCurrentKeyboardInputSource` / `TISGetInputSourceProperty` calls per
  paste**. Those HIToolbox APIs are not safe off the main thread: internally
  `islGetInputSourceListWithAdditions` asserts the main queue and kills the
  process. `simulate_paste` runs on the coordinator's background thread, so a
  dictation that had already been transcribed successfully could take the whole
  app down at the final step. Now sends the raw keycode `kVK_ANSI_V` (`0x09`),
  which enigo posts directly as a `CGEvent` with no layout lookup.

  It is a **race** — a single background caller usually wins and survives,
  which is exactly why this looked random and survived months of daily use. It
  reproduces deterministically with concurrency: one thread passes, eight
  abort. The regression test uses eight.

  This failure is a process abort, not a Rust panic, so no amount of error
  handling could have contained it. The only fix is not making the call.

  *Trade-off, stated plainly:* a raw keycode is layout-blind. `0x09` is the key
  that types `v` on QWERTY; on Dvorak or Colemak it is not, so those layouts
  would get the wrong shortcut. This is the same trade-off the Windows `VK_V`
  fix already accepts. If it affects you, please open an issue — the fix is to
  resolve the keycode once on the main thread at startup and cache it, not to
  go back to the crashing call.

- **A panic in the dictation loop left the hotkey permanently dead, silently.**
  Rust unwinds rather than aborts here, so a panic on the coordinator thread
  killed only that thread — the process, tray icon and Settings window all kept
  running while the hotkey quietly stopped responding. No crash report, nothing
  to diagnose from, and the only cure was quitting and relaunching. It also
  latched: a panic while holding a lock poisoned it, so every later press hit
  the same poisoned `unwrap`. The loop is now supervised — it catches the
  unwind, resets the recorder to Idle, tells you via the overlay, and restarts,
  keeping the same message channel so queued work isn't lost. Locks recover
  from poisoning instead of propagating it.

- **Panics were never logged.** There was no panic hook at all, so a panic on
  any background thread vanished without a trace — a GUI app launched from
  Finder has nowhere useful to write stderr. Panics now log thread name and
  source location before the default handler runs.

### Notes

- Audited the rest of the coordinator thread for the same main-thread hazard.
  The overlay's `show`/`hide` are safe: Tauri marshals them to the event loop
  via `send_user_message`. The permission checks (`IsSecureEventInputEnabled`,
  `AXIsProcessTrustedWithOptions`, `IOHIDCheckAccess`) and the Accessibility
  paste-verification read are not main-thread-restricted. No changes needed.
- Four regression tests added, including one that pins the receiver as
  *borrowed* — if it ever becomes owned again, the restart silently loses the
  channel and the supervision is worthless.

## [0.3.2] — 2026-07-10

**Windows works, for real.** The Windows build had never been run on Windows.
Now it has — install, model download, transcription (RTF 0.131x on CPU ONNX),
the Ctrl+Shift+Space hotkey, hands-free auto-stop, the desktop-app-as-daemon
round trip over the named pipe, the MCP server, and a human dictating by voice.
That run found one bug, and it was a bad one.

### Fixed

- **Windows auto-paste silently did nothing.** `enigo`'s `Key::Unicode('v')` is
  delivered as a `KEYEVENTF_UNICODE` / `VK_PACKET` character event, which target
  applications do not map onto the Ctrl+V paste accelerator. enigo reported
  success; Notepad received nothing. Your transcript still reached the clipboard,
  so the failure looked like "the app pastes nothing" rather than an error. Send
  the real virtual key (`VK_V`, `0x56`) instead. Verified on the same hardware
  that found it.
- **The Settings window offered a hotkey picker on Windows that did nothing.**
  The hotkey is hardcoded to Ctrl+Shift+Space there and `hotkey_keycode` is
  ignored, so the picker was a dead control. Settings is now platform-aware.

### Documentation

- The Windows guide is rewritten around what a user does, now that the build is
  verified rather than hypothetical, and the README says plainly that the
  Windows installers are **unsigned** — SmartScreen will warn, and the Apple
  Developer ID signing covers the macOS `.dmg` only.
- `README` explains how to change the hotkey where the gestures are described,
  including why only modifier keys are valid, why Caps Lock is excluded, and why
  the left-side keys are labelled as conflicting.
- `set_recv_timeout` on a Windows named pipe fails on **real hardware**, not
  just CI runners. The blocking-read degradation is the normal Windows path.

## [0.3.1] — 2026-07-10

### Fixed

- **The grant reset would have misfired once the app is signed.** A Developer ID
  signature gives TCC a stable requirement (team + bundle id) rather than a
  cdhash, so Accessibility and Input Monitoring survive an upgrade on their own.
  The installer now skips the reset when the app is Developer-ID-signed on both
  sides of the upgrade, and still performs it for ad-hoc builds, for the release
  where signing first lands (whose old grants are cdhash-pinned and genuinely
  stale), and for a downgrade back to ad-hoc.

- **An upgrade left the hotkey silently dead.** macOS pins the Accessibility and
  Input Monitoring grants to a code requirement containing the exact binary
  hash, which changes on every ad-hoc signed release. After an upgrade the old
  grant rows survived, still showing a ticked checkbox in System Settings, while
  macOS matched them against the new binary, saw a different hash, and denied.
  The hotkey did nothing — no dialog, no error, no log line — and un-ticking and
  re-ticking the box only toggled the stale row. `scripts/install.sh` now
  compares the code hash before and after replacing the app and, when it
  changed, clears exactly those two grants (`tccutil reset`, no `sudo`) so macOS
  prompts again, and prints a banner explaining what to re-grant. The microphone
  grant is left alone.
- **`INSTALL_MODELS=none` claimed the models were missing when they weren't.**
  The closing message was hardcoded to the flag rather than reading the disk, so
  a re-run or upgrade told users to re-download 456 MB they already had.

### Added

- **Signed and notarized with a Developer ID certificate.** Every previous
  release was ad-hoc signed, which cost users twice: a browser-downloaded `.dmg`
  or a Homebrew cask needed a right-click → Open to get past Gatekeeper, and
  because TCC pins Accessibility and Input Monitoring to the exact binary hash,
  every upgrade silently killed the hotkey. Both are gone. Notarization uses an
  App Store Connect API key rather than an Apple ID and an app-specific
  password.
- **New app icon.** The menu-bar tray icon is unchanged — it is a macOS template
  image, which macOS renders as a flat silhouette, so the full-colour artwork
  cannot be used there.

- **`scripts/setup-signing.sh`** turns the Developer ID setup into one command.
  It reads the signing identity and Team ID out of your exported `.p12`,
  base64-encodes the certificate, and writes the six secrets `release.yml`
  reads. It refuses a certificate that is not a `Developer ID Application`, and
  refuses a normal Apple ID password where an app-specific one is required —
  both of which otherwise fail deep inside a CI run with an opaque error.

- **CI for the installer** (`.github/workflows/installer.yml`). `scripts/**` was
  excluded from `build.yml` via `paths-ignore`, so `scripts/install.sh` — which
  every one-liner user fetches straight from `main`, with no release gate in
  front of it — shipped with no syntax check at all. A typo would have broken
  every new install the moment it was pushed. The new workflow lints
  (`shellcheck`, `bash -n`, and a PowerShell parse of `install.ps1`) and then
  actually installs the latest release on clean macOS and Linux runners,
  asserting that a fresh install leaves a working app, CLI and MCP server; that
  reinstalling the same build does **not** reset the user's permissions; that a
  changed code identity **does**; and that `NO_APP=1` installs the CLI without
  touching `/Applications`. The two grant tests check each other for vacuity.
- **`NO_APP=1`** installs only the `flow` CLI, MCP server and models, skipping
  the `.dmg` download and leaving `/Applications` untouched. This is now the
  supported way to add the CLI alongside a Homebrew cask install. Previously the
  installer would `rm -rf` the cask-managed app and replace it, leaving `brew`
  with a receipt for a bundle it no longer wrote — while the README, the cask
  caveats, and `AGENT-INSTALL.md` all claimed it "detects the brew-installed app
  and won't overwrite it." It never did. All three have been corrected.

### Documentation

- README gains an install-path comparison table (only the one-liner leaves a
  working system), the installer's environment variables, what each of the three
  macOS permissions buys and how it fails without it, the upgrade grant-reset
  problem and its fix, a Gatekeeper/code-signing section, and a troubleshooting
  ladder for "I hold the hotkey and nothing happens."

## [0.3.0] — 2026-07-09

**The release where a stranger can actually install this.** Every prior version
worked reliably for exactly one person — the maintainer — whose machine already
had the models, the permissions, and a dev build launched from a terminal. This
one closes the gap.

### The app sets itself up

- **The app can download its own models.** It never could. `brew install --cask`
  and a `.dmg` download both ship *no CLI*, and only the CLI could fetch models —
  so those users could never transcribe a word. Settings now has a Download button
  with a live percentage.
- **Settings opens on first launch.** VZT Flow is a menu-bar app with no Dock
  icon, so a fresh install previously looked like nothing had happened at all.
- **Holding the key before the model exists tells you so.** Recording wasn't gated
  on the model being present: you talked for thirty seconds, released, and got a
  hard-coded "Transcription failed" while the real reason went to a stderr no GUI
  user ever sees.
- **Granting Input Monitoring now arms the hotkey within ~2 seconds.** Previously
  the event tap was created once at startup, so granting the permission the app
  had just asked for did nothing until you quit and relaunched — and only stderr
  ever said so.
- **The `curl | bash` one-liner installs the model.** It defaulted to skipping it,
  so the documented install command produced a system that could not transcribe.

### Windows

- **`flow.exe` and the MCP server now ship.** Windows previously received the app
  alone. Since only the CLI could download the speech model, there was no path to
  a working install short of building from source. `install.ps1` now installs the
  CLI, adds it to your PATH, registers the MCP server, and downloads the model.
- Still experimental: no cleanup LLM, no per-app profiles, no Esc-to-cancel, no
  secure-field detection, and it has never been run on real Windows hardware.

### Fixed

- **macOS 12 users installed an app that could never launch.** The bundle
  advertised `LSMinimumSystemVersion 12.0` while hard-linking ScreenCaptureKit,
  whose audio capture requires 13.0 — so it installed cleanly and then dyld-aborted
  with no error, just a bouncing icon. The floor is now 13.0, and 13.3 on Intel
  (the bundled ONNX Runtime dylib is `minos 13.3`).
- **A flaky download bricked the install permanently.** The cleanup model was
  written straight to its final path, a sha256 mismatch printed "proceeding
  anyway", and the next run's existence check then skipped re-downloading that
  corrupt 1.1GB file *forever*. Downloads now stage to a `.partial`, resume over
  HTTP Range, verify sha256 before an atomic rename, and never promote a bad file.
  A corrupt model already on disk is detected and re-downloaded.
- **Setting `VZT_FLOW_BIN` did nothing.** The MCP server read `FLOW_BIN`, while
  every script, README, and release note documented `VZT_FLOW_BIN`. Both work now;
  the installed locations are probed before any dev-tree path.
- Installers now check their dependencies instead of assuming them: Node (required
  by the MCP server, previously unchecked — its absence produced a registration
  that failed later with an opaque error) and free disk space before a download.

### Added

- **Rebindable hold-to-talk key** — nine modifiers instead of five. Left-side keys
  are offered but flagged: they collide with ordinary shortcuts. Caps Lock is
  excluded because its flag reflects the latched state, not the key being held.
- **`flow doctor` reports your hotkey binding** and names the two ways it can be
  wrong. An unsupported keycode in `config.toml` used to leave the hotkey silently
  dead with no diagnostic.
- **[`AGENT-INSTALL.md`](AGENT-INSTALL.md)** — hand it to Claude Code, Codex CLI,
  or Gemini CLI and it installs and verifies everything.

### Known limitations

- Still ad-hoc signed, not notarized. A browser-downloaded `.dmg` shows the
  bypassable "Apple could not verify" dialog (System Settings → Open Anyway), and
  because the ad-hoc signature changes each release, **macOS drops your
  Accessibility and Input Monitoring grants when you upgrade** — re-grant both.
  The CI signing path is written and dormant, waiting on a Developer ID certificate.
- Linux is **unsupported / community-maintained**. Artifacts still build, but
  Wayland has no global hotkey and nothing has been run on real Linux hardware.
- The three macOS permissions (Microphone, Accessibility, Input Monitoring) are
  required by the OS for any app that records you and types into other apps. No
  amount of signing removes them.

## [0.2.1] — 2026-07-09

**If you installed 0.2.0, upgrade.** Its macOS packaging was broken in three
ways that never showed up on the maintainer's machine, and between them they
made the app unusable for essentially every new user. No dictation behavior
changed in this release — it is packaging only.

### Fixed

- **The app force-quit the moment you talked** (Apple Silicon). The bundle
  carried no `NSMicrophoneUsageDescription`, and macOS terminates a process
  that opens a microphone stream without one — no dialog, no error, just a
  kill. It never bit the maintainer because a binary launched from a Terminal
  inherits Terminal as its TCC *responsible process*, and Terminal already
  holds a microphone grant. Double-clicking the `.app` makes it its own
  responsible process, and it died. You will now see a normal macOS
  microphone prompt on first dictation.
- **The app wouldn't open at all if you downloaded the `.dmg` in a browser**
  (Apple Silicon). Only the Intel CI job ever ran `codesign`, so the arm64
  bundle shipped with its resources unsealed. A quarantined copy — any
  browser download, and Homebrew casks, which quarantine by default — failed
  Gatekeeper with *"VZT Flow is damaged and can't be opened"*, the variant
  with no "Open Anyway" escape hatch. The bundle is now ad-hoc signed during
  bundling. (Installs via the `curl | bash` one-liner were unaffected: curl
  never sets the quarantine attribute.)
- **Every Intel Mac crashed at launch, on every launch.** `cargo tauri build`
  cuts the `.app` and the `.dmg` in one invocation; the job then patched the
  `.app` to bundle `libonnxruntime.dylib` — but the `.dmg` had already been
  packaged from the unpatched one. The shipped Intel app was unsigned, had no
  `Contents/Frameworks`, and needed a dylib via an rpath pointing nowhere, so
  it aborted at dyld load. The dylib is now placed and signed by the bundler
  before the `.dmg` is cut.

### Added

- **Rebindable hold-to-talk key.** Settings → Hotkey now offers all nine
  hold-capable modifiers — Right Option (default), Right Shift, Right
  Control, Right Command, Fn, and the four left-side modifiers — instead of
  five. Left-side keys are flagged in the UI: they collide with ordinary
  shortcuts (Cmd+C, Shift+click), which is why Right Option is the default.
  Changes apply live, with no restart. Caps Lock is deliberately excluded —
  its flag reflects the *latched* state, not physical key-down, so binding it
  would toggle rather than hold.
- **`flow doctor` now reports your hotkey binding** and names the two ways it
  can be wrong: a keycode outside the supported set (the hotkey silently
  never fires), or Caps Lock (toggle semantics). Previously an unsupported
  `hotkey_keycode` in `config.toml` produced a dead hotkey with no diagnostic.
- **[`AGENT-INSTALL.md`](AGENT-INSTALL.md)** — point Claude Code at it and it
  installs the app, CLI, MCP server, and models, then verifies the result.
  `scripts/install.sh` and `install.ps1` gained `INSTALL_MODELS=none|asr|all`
  for unattended model downloads; the default `none` leaves the public
  one-liner's behavior unchanged.

### Known limitations

- Still ad-hoc signed, not notarized. A browser-downloaded `.dmg` now shows
  the *bypassable* "Apple could not verify…" dialog (System Settings →
  Privacy & Security → Open Anyway) instead of the un-bypassable "damaged"
  one. Removing it entirely requires a Developer ID certificate.
- Because the signature is ad-hoc, its cdhash changes every release, so macOS
  drops your Accessibility and Input Monitoring grants on upgrade. Re-grant
  both after installing. Only Developer ID signing gives a stable identity
  across updates.
- The Intel build is still CI-built and has never been run on real Intel
  hardware.

## [0.2.0] — 2026-07-09

> [!WARNING]
> Broken on macOS — see 0.2.1. The app force-quits on first dictation
> (Apple Silicon), refuses to open from a browser-downloaded `.dmg`, and
> never launches at all on Intel. Upgrade rather than installing this.

The dictation core grows up (long holds no longer OOM and paste the instant
you release), and the app leaves macOS: a full Linux build and a Windows
daemon that finally has a working control socket.

### Dictation core

- **Rolling transcription** (`crates/flow-core/src/rolling.rs`, `f3618e8`) —
  on a long hold, each silence-completed ~30s chunk is transcribed *while you
  keep talking*, so at release only the final <35s tail remains. On a real
  ~7¾-minute (465s) clip the transcript now pastes **0.53s after release
  instead of 25.15s** — identical 7745-char output, 15 chunks transcribed
  during recording, peak RSS unchanged (still ONNX-inference-bound, ~11GB). A
  subdued live-preview line in the overlay pill shows the raw tail as it
  accrues. On by default (`rolling_transcription`); recordings under ~35s are
  unaffected. Hidden `flow rolling-test <file>` exercises it.
- **Long-form dictation** (`759cf9c`) — recording caps raised to **600s
  (10min)** for both held and hands-free (`max_hold_secs` /
  `max_handsfree_secs`); the cleanup-LLM deadline now scales with transcript
  length (`cleanup_timeout_ms` base + `cleanup_timeout_per_char_ms`/char,
  capped at `cleanup_timeout_max_ms`); the pill shows a running mm:ss timer
  with an amber warning in the last 30s before the cap.
- **Accidental-press guard** (`crates/flow-core/src/hotkey.rs`, `f3618e8`) —
  Right Option is macOS's special-character modifier, so holding it and
  pressing any other key is now treated as typing, not push-to-talk: the
  hands-free toggle is suppressed and any already-started recording is
  discarded. The tap is `ListenOnly`, so the typed character still reaches
  the app.
- **Paste verification** (`crates/flow-core/src/insert.rs`, `f3618e8`) —
  ~150ms after the synthetic Cmd+V, the focused field is read via the
  Accessibility API; if the transcript tail is missing, the paste is retried
  once, then left on the clipboard with a "paste may have failed" overlay.
  Unreadable fields (most web/Electron/secure inputs) are assumed successful.
  Bounded under 400ms.
- **Code mode** gained deterministic quote and comma symbols (`1d0c69d`).

### Meeting mode

- **Meeting auto-detect** (`crates/flow-core/src/meeting/detect.rs`,
  `308b899`) — a background detector combines a frontmost-app rule table
  (Zoom/Meet/Teams) with a mic-live CoreAudio signal via a debounced state
  machine to offer or auto-start `flow meeting` when a call begins. 100%
  local, titles-only, no screenshots/OCR. New tray submenu **Meeting
  auto-detect ▸ Ask/Auto/Off**, persisted as `meeting_auto` in `config.toml`.

### Cross-platform

- **Linux port** (`264d140`) — CI-built `.deb` + `.AppImage` for
  `x86_64-unknown-linux-gnu`. On **X11** it behaves like the Windows build
  (hold-to-talk, auto-paste, tray, overlay). On **Wayland** it degrades
  gracefully: no global hotkey across native apps, clipboard-only paste (with
  a "press Ctrl+V" notification), best-effort overlay — Wayland denies
  clients global input grabs by design. Meeting mode is not yet available on
  Linux (needs a PipeWire capture backend). Never run on real Linux hardware;
  see `docs/USAGE-Linux.md`.
- **Windows named-pipe daemon** (`11493ce`, `519004f`, `3ca31c8`) — the
  daemon control socket now works on Windows over a native named pipe
  (`\\.\pipe\vzt-flow-daemon`), so `flow status`/`toggle`/`cancel`/`listen`
  and the MCP server are daemon-first there too, matching macOS. CI-unit-
  tested (a real pipe connect + status round trip); the full desktop-app-as-
  daemon path is still unverified on real Windows hardware.
- **Custom app icon** (`3a2e60f`) — replaces the Tauri default placeholder.

### Install

- **Homebrew cask** — `brew install --cask vonzelle-vzt/vzt/vzt-flow` from
  the [vonzelle-vzt/homebrew-vzt](https://github.com/vonzelle-vzt/homebrew-vzt)
  tap (Apple Silicon / Intel `.dmg` auto-selected). Some Homebrew versions
  require `brew trust --cask vonzelle-vzt/vzt/vzt-flow` first.
- `scripts/install.sh` now also handles Linux (`.deb` via `apt`, or the
  portable `.AppImage`) alongside the existing macOS path.

### Fixes

- **Silent-input glossary bug** (`1d0c69d`) — an empty/whitespace-only
  transcript no longer echoes a stray glossary/dictionary term; empty input
  is passed through untouched.
- **Long-audio OOM** (`4757636`) — the bundled `transcribe-rs` Parakeet
  engine has no internal chunking and grows memory faster than linearly
  (measured: ~15GB peak for 49s of audio, ~37GB for 93s, OOM kill at ~146s).
  Recordings longer than ~35s are now transparently split into ~30s
  silence-cut chunks (`crates/flow-core/src/chunking.rs`) and transcribed
  sequentially, bounding peak memory to a single chunk. A real ~7min (438s)
  clip now measures **~32.5s wall time, RTF 0.074, ~8.9GB peak RSS**.
- **Windows named-pipe test flakiness** (`519004f`, `3ca31c8`) — the
  in-process pipe tests in `ipc.rs` no longer hang or race on the Windows
  runner; GitHub's windows-2025 runners reject `set_recv_timeout` on
  named-pipe client streams, so the client degrades to a blocking read (safe
  — callers gate on `is_alive` first).

## [0.1.0] — 2026-07-09

Initial release. The full local dictation pipeline: hold-to-talk (or tap for
hands-free) → on-device Parakeet TDT 0.6B v3 ASR (int8 ONNX, CoreML on Apple
Silicon; measured 0.83s / RTF 0.097x on an 8.6s clip) → optional local
Qwen3-1.7B `clean`/`polish` cleanup (deadline-bound, ~0.3s warm) or
deterministic `code` mode → dictionary correction, per-app profiles,
snippets, history → paste at the cursor. Ships the `flow` CLI (daemon-first
with standalone fallback), an MCP server for Claude Code (`listen`,
`transcribe_file`, `dictation_history`), and `flow meeting` — dual-stream,
speaker-labelled, fully-local Zoom/Meet/Teams transcription with a Qwen3
summary on stop. macOS Apple Silicon (primary) + Intel (CI-built); Windows
x64 experimental. Everything runs on-device — the only network traffic is the
one-time model download from Hugging Face.

[0.2.0]: https://github.com/vonzelle-vzt/vzt-flow/releases/tag/v0.2.0
[0.1.0]: https://github.com/vonzelle-vzt/vzt-flow/releases/tag/v0.1.0
