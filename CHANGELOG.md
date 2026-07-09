# Changelog

All notable changes to VZT Flow are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses [semantic
versioning](https://semver.org/). Numbers quoted below were measured on this
repo's dev hardware (M5 MacBook Air) unless noted — see `README.md` /
`docs/PRD.md` for the full methodology.

## [0.2.0] — 2026-07-09

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
