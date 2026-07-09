# VZT Flow — Product Requirements Document

Status: living document, describes the app **as-built** plus near-term
roadmap. Where a claim needs a number, the number is pulled from the README
and `docs/USAGE-macOS.md`, both measured on this repo's own dev hardware (M5
MacBook Air) — re-verify before quoting elsewhere if hardware changes.

## Problem & positioning

Cloud dictation apps — [Wispr Flow](https://wisprflow.ai) chief among them —
cost **$12-15/month ($144-180/yr)** and send every recording (audio, and
sometimes screenshots of the active window for app-aware formatting) to a
third-party server for processing. For a hold-key-and-talk workflow on Apple
Silicon, none of that round-trip is actually necessary: a 0.6B-parameter ASR
model and a 1.7B cleanup LLM both run comfortably local and fast enough that
the cloud trip was never buying meaningful latency or quality headroom.

VZT Flow's position is **not** "beats cloud transcription quality" — Parakeet
TDT 0.6B v3 is very good, not the largest ASR model in existence. The
deliberate trade is: give up a little headroom at the top end in exchange for
**$0 cost, zero network dependency during use, and a privacy guarantee
verifiable by reading the source** (MIT-licensed, this repo).

**Privacy guarantee, precisely stated:** audio, transcripts, and screenshots
never leave the device during normal operation. The only network traffic VZT
Flow ever makes is a one-time download of the Parakeet/Qwen3 model files from
Hugging Face on first use of each feature.

## Users

**Primary: developers.** The two features with no analogue in any cloud
dictation product — `code` mode (spoken-form → real identifiers/punctuation)
and the MCP voice-input tool for Claude Code — only make sense for someone
writing code and prompting an agentic coding tool. Per-app profiles seed
Terminal/iTerm2/Warp to `code` mode out of the box for the same reason.

**Secondary: anyone who wants free, private, hold-key dictation** — the
`clean`/`polish`/`raw` pipeline, per-app profiles, dictionary, and snippets
are all general-purpose and not developer-specific.

## Core requirements — as shipped

Status legend: ✅ shipped and daily-driven · 🚧 shipped, actively being
extended (a concurrent workstream may be mid-merge)

| Requirement | Status | Notes |
|---|---|---|
| Push-to-talk (hold) + hands-free (tap) + cancel (Esc) | ✅ | Hold Right Option (macOS keycode 61) to record/release-to-paste; tap (<300ms, `hold_threshold_ms`) starts hands-free, auto-stops after 2.5s silence (`handsfree_silence_secs`) or a second tap; Esc discards outright. Hard caps: 600s (10min) for both hold and hands-free (`max_hold_secs`/`max_handsfree_secs`) so a stuck key can't record forever — hitting the cap transcribes what was captured. |
| On-device ASR (Parakeet TDT 0.6B v3) | ✅ | int8-quantized ONNX via `transcribe-rs`, CoreML execution provider on Apple Silicon. Measured: 0.83s wall time / **RTF 0.097x (~10x realtime)** on an 8.6s clip, M5 MacBook Air. Windows runs the same model on plain CPU ONNX (no CoreML) — expect a lower RTF, unverified on real hardware. |
| Cleanup modes (`raw`/`clean`/`polish`) with never-block deadline | ✅ | `clean`/`polish` run Qwen3-1.7B-Instruct (Q4_K_M GGUF) via embedded llama.cpp, Metal-offloaded on Apple Silicon. Deadline scales with input length: `cleanup_timeout_ms` (2500ms base) + `cleanup_timeout_per_char_ms` (6ms/char), capped at `cleanup_timeout_max_ms` (20000ms) — miss the deadline and the dictionary-corrected raw transcript pastes instead, worker thread cancelled+joined (never detached). Measured warm generation for a short sentence: ~0.3s, well inside the base deadline. |
| Code mode | ✅ | Deterministic, no-LLM transform (`crates/flow-core/src/codemode.rs`): case keywords (`camel case`/`snake case`/`pascal case`/`kebab case`), symbol words (`open paren`, `fat arrow`, `dollar sign`, ...), language-keyword boundaries, implicit call-name merge (`get user open paren close paren` → `getUser()`). Exact and instant — no model in the loop. |
| Per-app profiles | ✅ | `profiles.toml`, macOS bundle IDs (optional `*` prefix match) → mode/tone pair, resolved via `NSWorkspace` (bundle ID only — no screenshot, no window content). Seeded: Terminal/iTerm2/Warp → `code`; Mail → `clean`/`formal`; Slack → `clean`/`casual`; everything else → `[default]` (`clean`/`neutral`). |
| Personal dictionary | ✅ | `dictionary.json`, fuzzy (Levenshtein, budget `len/4`/word) for terms 4+ chars, exact-match only below that. Applied before cleanup/code-mode. Seeded with the project's own stack (Supabase, Whop, Vercel, Tauri, Parakeet, ...). |
| Snippets | ✅ | `snippets.json`, trigger phrase → fixed expansion, fires only when the trigger is the *entire* cleaned transcript (or `"insert <trigger>"`). Applied after cleanup. |
| History | ✅ | `history.jsonl`; `flow history -n <N>` and the tray's "Copy last transcript." |
| CLI + daemon socket | ✅ | `flow listen/transcribe/models/doctor/status/toggle/cancel/history` plus hidden diagnostics `paste-test`/`clean-test`/`code-test`. Daemon-first (drives the desktop app's overlay) with a fully standalone fallback (no daemon required) on macOS. Unix domain socket only — see Windows gap below. |
| MCP voice input for Claude Code | ✅ | `mcp/src/index.ts`: `listen`, `transcribe_file`, `dictation_history` tools, backed by the daemon socket when available, standalone `flow` CLI otherwise. The headline differentiator — no other dictation product exposes an MCP tool for coding agents. |
| Meeting transcription + summary | ✅ | `flow meeting` — dual-stream capture (ScreenCaptureKit for system/participant audio + mic), both transcribed by the same local Parakeet engine, speaker-labelled Markdown transcript with an echo filter (Jaccard similarity > 0.7 on time-overlapping lines) to dedupe your own mic picking up speaker audio. On stop, local Qwen3 appends a summary + action items. `meeting_transcript` MCP tool exposes transcripts to Claude Code. macOS-only (ScreenCaptureKit is a macOS 13+ framework). |
| Meeting auto-detect | 🚧 | Merging concurrently (tray/meeting/config workstream) — automatic start of `flow meeting` when a call app becomes frontmost, rather than requiring a manual `flow meeting --title ...` invocation. |
| Long-audio chunking | ✅ | Shipped `4757636`. `transcribe-rs`'s bundled Parakeet engine has **no internal audio chunking** (`supports_streaming: false`) and quadratic memory growth (see Non-functional/memory below) — recordings longer than ~35s are transparently split into ~30s silence-cut chunks (`crates/flow-core/src/chunking.rs`) and transcribed sequentially, bounding peak memory to a single chunk regardless of total length. Measured on a real ~7min (438s) clip: **~32.5s wall time, RTF 0.074, ~8.9GB peak RSS**. |
| Cross-platform builds | ✅ macOS · 🚧 Windows | CI (`build.yml`/`release.yml`) builds macOS Apple Silicon (`aarch64-apple-darwin`, primary/tested) and Intel (`x86_64-apple-darwin`, CI-built, never run on real Intel hardware — effective OS floor is macOS 13.3, not the 12.0 `tauri.conf.json` advertises). Windows x64 (`x86_64-pc-windows-msvc`) is CI-built and experimental (never run on real hardware); Windows Arm (`aarch64-pc-windows-msvc`) is attempted as an allowed-to-fail job. |

## Non-functional requirements

### Latency targets (measured, M5 MacBook Air)

- **ASR**: RTF ~0.097x (~10x realtime) — 0.83s wall time for an 8.6s clip.
- **Cleanup (warm)**: ~0.3s for a short sentence; deadline formula
  (2500ms base + 6ms/char, capped 20000ms) guarantees a bounded worst case
  regardless of input length.
- **First dictation of a session**: a few extra seconds for lazy Parakeet
  model load; cleanup LLM pre-warms in parallel with speech (model load + a
  throwaway generation to force Metal kernel JIT), so it's typically already
  warm by the time cleanup actually runs.

### Memory budget, including the quadratic-ASR lesson

Measured via `ps -o rss` on the dev machine:
- **Idle** (both models unloaded, the steady state after `idle_unload_secs`,
  300s default): **~30-40MB RSS**.
- **ASR loaded, mid-dictation**: ~2GB (engineering estimate from Parakeet's
  ~456MB on-disk footprint + ONNX Runtime session/execution-provider
  overhead).
- **Cleanup also loaded** (`clean`/`polish`): ~3.5GB (adds the ~1.1GB Qwen3
  GGUF + its 8192-token llama.cpp context allocation).

**The quadratic-ASR lesson (why long-audio chunking is a hard requirement,
not a nice-to-have):** the bundled `transcribe-rs` Parakeet engine has no
internal chunking and exhibits **faster-than-linear (measured: roughly
quadratic) memory growth** with audio length. Measured on this repo's M5:
**~15GB peak for 49s of audio, ~37GB for 93s, and an out-of-memory kill for
~146s (2m26s)**. A ~7min clip failed outright with a CoreML "dynamically
resizing for sequence length" error. This is why `max_hold_secs`/
`max_handsfree_secs` are capped at 600s, and why long-audio chunking
(shipped `4757636`, ✅ above) is what makes that cap safe: recordings longer
than ~35s are transparently split into ~30s chunks before ever reaching the
engine, bounding peak memory to a single chunk's footprint regardless of
total recording length. A real ~7min (438s) clip now measures **~32.5s wall
time, RTF 0.074, ~8.9GB peak RSS** — well within budget. The underlying
`transcribe-rs` quadratic growth is unchanged upstream; the chunker is the
fix, not a raised ceiling.

### Offline-only guarantee

No audio, transcript, or screenshot leaves the device during normal
operation. The only network calls are one-time model downloads (Parakeet
~456MB, Qwen3 ~1.1GB) from Hugging Face, explicitly triggered by
`flow models download`. This is a testable claim — the codebase is small
enough to audit end to end (see README's Contributing section).

### Permissions model (macOS)

Three System Settings grants: **Microphone** (auto-prompted on first
recording), **Accessibility** (paste simulation — without it, transcript
lands on the clipboard for manual paste), **Input Monitoring** (hold-to-talk
hotkey via `CGEventTap` — without it, the hotkey silently does nothing, tray
toggle still works). **Screen Recording** is additionally required for
meeting mode's system-audio capture, and macOS attributes that grant to the
*terminal app*, not `flow`, when run from a terminal.

**Known gotcha (unsigned builds):** every unsigned/ad-hoc-signed rebuild
mints a new code signature, and macOS silently revokes Input
Monitoring/Accessibility grants tied to the old signature — no error dialog,
the hotkey just stops working. Documented fix: remove+re-add the grant (not
just re-toggle) after every rebuild, or sign with a stable identity so
grants survive rebuilds (not yet done — see Out of scope below).

## Out of scope / roadmap

- **Code signing + notarization** — would make Accessibility/Input
  Monitoring grants survive rebuilds; currently a manual remove/re-add dance
  after every unsigned rebuild.
- **Windows daemon socket + hardware validation** — `flow_core::ipc`'s only
  transport is a Unix domain socket; Windows falls back to standalone-only
  CLI paths (no `flow status`/`toggle`/`cancel`, no daemon-driven overlay for
  `flow listen`, no MCP daemon path). Separately, the Windows build has
  **never been run on real Windows hardware** — hold/tap timing, permission
  prompts, and general stability are all "verified in code, untested in
  practice."
- **Apple `SpeechAnalyzer` as an alternate ASR engine** — an on-device Apple
  framework alternative to Parakeet, not yet integrated.
- **A "polish for Claude Code" cleanup mode** — tuned for dictating prompts
  to an agent rather than prose for a human reader.
- **Command mode ("rewrite selection")** — voice-driven editing of existing
  text/code rather than only inserting new dictation.
- **Streaming partial insertion** — today the full transcript pastes after
  the recording stops; no incremental/partial-result insertion while still
  speaking.
- **Windows-only gaps already documented in `docs/USAGE-Windows.md`**: no
  per-app profiles (bundle-ID resolution is macOS-only, `[default]` always
  applies), no `clean`/`polish` cleanup LLM (`LlamaCleanupProvider` is
  `#[cfg(target_os = "macos")]`-gated), no Escape-to-cancel, no secure-field
  paste protection, elevated-window (UIPI) paste can silently fail.

## Success criteria

1. **Replaces Wispr Flow for the owner's daily dictation use, at $0/mo.**
   The owner has stopped paying the $12-15/mo Wispr Flow subscription and
   uses VZT Flow as the daily-driver dictation tool instead — the concrete
   bar this project was built to clear (see the original planning doc,
   `~/.claude/plans/i-want-to-create-lazy-eclipse.md`).
2. **Installable by anyone in one command.**
   `curl -fsSL https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.sh | bash`
   downloads the latest GitHub Release, installs the `.app` to
   `/Applications`, puts `flow` on PATH, and registers the MCP server if the
   `claude` CLI is present — no manual build step required for a Release
   install (source build remains available and documented for
   contributors/Windows/unreleased-commit use).
