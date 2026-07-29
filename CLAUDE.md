# CLAUDE.md — VZT Flow

Local, private, on-device voice dictation for macOS (+ experimental Windows).
Hold a key, talk, transcript lands wherever the cursor is — no cloud, no
subscription. Full product context: [docs/PRD.md](docs/PRD.md). Full user
docs: [README.md](README.md), [docs/USAGE-macOS.md](docs/USAGE-macOS.md),
[docs/USAGE-Windows.md](docs/USAGE-Windows.md), [docs/MEETINGS.md](docs/MEETINGS.md).

## Workspace layout

```
crates/flow-core/   engine: audio capture, ASR (Parakeet), LLM cleanup, dictionary,
                     code mode, snippets, profiles, history, hotkey, paste,
                     model download/management, daemon IPC. Platform-agnostic;
                     macOS-only pieces are #[cfg(target_os = "macos")]-gated.
crates/flow-cli/     the `flow` binary — daemon-first, standalone fallback.
apps/desktop/        Tauri 2 menu-bar app: tray, overlay, Settings, hotkey,
                     daemon control socket (apps/desktop/src-tauri/src/).
mcp/                 Node/TypeScript MCP server (listen, transcribe_file,
                     dictation_history, meeting_transcript) for Claude Code.
```

## Build / test / run

```bash
source ~/.cargo/env                                   # Rust not on PATH by default here

cargo build --release -p flow-cli
cargo test --release --workspace
./target/release/flow doctor                          # env/model/daemon sanity check

cd apps/desktop && npm install && cargo tauri build    # unsigned local build
open ../../target/release/bundle/dmg/*.dmg             # bundle lands at workspace-root target/, not src-tauri/target/

cd mcp && npm install && npm run build
```

## Critical gotchas

**(a) Unsigned rebuilds drop macOS TCC grants.** Every unsigned/ad-hoc-signed
`cargo tauri build` mints a new code signature; macOS silently revokes Input
Monitoring/Accessibility grants tied to the old one — no error dialog, the
hotkey just stops working. After any rebuild: relaunch the daily-driver app
via `nohup <dev-path-binary> &` (e.g.
`nohup ./target/release/bundle/macos/VZT\ Flow.app/Contents/MacOS/vzt-flow &`),
**not** `open` — `open` re-resolves to whatever's in `/Applications` and can
mask which binary you're actually testing. **Never kill the user's running
daily-driver app without relaunching it** — they lose their dictation tool
mid-session. See [docs/USAGE-macOS.md#the-rebuild-drops-permissions-gotcha](docs/USAGE-macOS.md#the-rebuild-drops-permissions-gotcha)
for the full remove/re-add grant fix.

**(b) transcribe-rs Parakeet memory is quadratic in audio length.** No
internal chunking (`supports_streaming: false`). Measured on this repo's M5:
~15GB peak for 49s of audio, ~37GB for 93s, OOM kill at ~146s. **Never call
`.transcribe()` on >60s of audio directly** — route long audio through the
chunked path (`crates/flow-core/src/chunking.rs`) instead. See
[docs/PRD.md](docs/PRD.md#memory-budget-including-the-quadratic-asr-lesson)
for the full numbers. **Long-audio latency is handled separately from
memory** by `crates/flow-core/src/rolling.rs`: it transcribes
silence-completed chunks *during* recording (reusing the chunker's `plan_cut`
+ seam-dedup) so only the final <35s tail runs at release (measured: end-
latency 25.15s → 0.53s on a 465s clip). Both the memory ceiling (chunking)
and the release-latency wall (rolling) are already solved — don't reinvent
either; extend them.

**(c) SCK `CMSampleBuffer` audio needs `make_data_ready()`.** ScreenCaptureKit
system-audio capture (`crates/flow-core/src/meeting/syscapture.rs`) will
yield empty/garbage buffers if this isn't called before reading sample data.

**(d) `CGEventTap` must be re-armed on `TapDisabledByTimeout`.** macOS can
disable an event tap under system load; the hotkey monitor
(`crates/flow-core/src/hotkey.rs`) re-arms from both the tap's own
`TapDisabledByTimeout`/`TapDisabledByUserInput` callbacks and a
belt-and-braces 5-second poll. Don't remove either path — they cover
different failure windows.

**(e) llama generation threads must be cancelled + joined, never detached.**
Cleanup (`crates/flow-core/src/cleanup.rs`) races LLM generation against the
deadline on a worker thread. A detached thread that outlives the deadline can
leak a live Metal context; always cancel+join on timeout.

**(f) No `timeout` binary on the dev Mac.** Use the perl-alarm pattern for
bounding a shell command instead of GNU `timeout(1)`:
```bash
perl -e 'alarm 30; exec @ARGV' -- <command> <args...>
```

**(g) `interprocess` named-pipe `set_recv_timeout` is unsupported on Windows
— everywhere, not just CI.** The Windows daemon transport
(`crates/flow-core/src/ipc.rs`, `pub mod windows`) opens a named pipe at
`\\.\pipe\vzt-flow-daemon`. `set_recv_timeout` on named-pipe *client* streams
fails ("named pipes do not support I/O timeouts") on GitHub's windows-2025
runners **and on real Windows 11 hardware (verified 2026-07-10)** — the
blocking-read degradation is the *normal* Windows path, not a CI quirk. The
fix is: log and continue if `set_recv_timeout` errors — callers already gate
on `is_alive` first, so an unanswerable pipe is caught before the read rather
than by a timeout. Don't turn that into a hard error, and don't assume a
recv-timeout is available on any named-pipe stream.

**(h) macOS input-source / TSM APIs abort off the main thread — and the
coordinator IS off the main thread.** `TISCopyCurrentKeyboardInputSource`,
`TISGetInputSourceProperty` and friends go through HIToolbox's
`islGetInputSourceListWithAdditions`, which asserts the main queue and
**aborts the process** (SIGTRAP/SIGABRT, not a catchable Rust panic) when it
has to build the input-source list. This killed the whole app mid-dictation on
2026-07-29: `enigo`'s `Key::Unicode('v')` resolves the character against the
live layout (256 TSM calls per paste) and `insert::simulate_paste` runs on the
coordinator thread. Fix was to send the raw keycode (`Key::Other(0x09)` =
`kVK_ANSI_V`) so no lookup happens — see `paste_v_key` in
`crates/flow-core/src/insert.rs`. **It is a race**: one background caller
usually wins and survives, which is why it looked intermittent — reproduce it
with concurrency (`paste_from_background_thread_does_not_trap`, 8 threads),
never with a single call. Before calling any Carbon/HIToolbox/AppKit API from
a worker thread, check whether it is main-thread-only; Tauri's own window ops
(`show`/`hide`) are safe because they marshal via `send_user_message`, but
that is a property of Tauri, not of the platform.

**(i) A panic on the coordinator thread is invisible, not fatal — which is
worse.** The release profile unwinds (no `panic = "abort"`), so a panic in
`run_coordinator` kills only that thread: the process, tray icon and Settings
window all survive while the hotkey silently stops responding forever. There
is no crash report to find. The loop is therefore supervised (`catch_unwind`
→ `AppState::reset_after_panic` → restart) and every `AppState` mutex is
locked via `LockRecover::lock_or_recover`, because a panic holding a lock
poisons it and `.lock().unwrap()` would then panic on every subsequent press —
the restart has to be able to make progress. **Do not reintroduce
`.lock().unwrap()` in the desktop crate**, and keep `run_coordinator`
borrowing its receiver rather than owning it (a moved receiver is dropped when
the first pass unwinds, so the restart has no channel to serve — pinned by
`a_panicking_pass_does_not_consume_the_channel`).

## Verification norms

- **Test with real TTS audio**, not silence/noise: `say -o /tmp/clip.aiff
  "your test sentence" && ffmpeg -y -i /tmp/clip.aiff /tmp/clip.wav`, then
  `flow transcribe /tmp/clip.wav` or `flow clean-test`.
- **Report real numbers, not estimates**: RTF (wall time / audio duration)
  and `ps -o rss` memory, not "should be fast." The README/PRD numbers were
  all measured this way — match that standard for anything new.
- **Screenshot the overlay via the tray's "Test overlay" item** — cycles
  Recording→Transcribing→Done with no mic/model involved, the only reliable
  way to visually QA overlay states (the menu-bar extra itself doesn't
  screenshot reliably on this multi-monitor dev machine under scripted
  clicks).
- Full ladder (build, tests, TTS-transcribe checks, clean-test latency,
  paste-test, daemon socket, overlay states): see
  `.claude/agents/flow-verifier.md` and `.claude/skills/verify-dictation/SKILL.md`.

## Releasing

Push a `v*` tag on `main` and `.github/workflows/release.yml` does the rest:
builds every platform, signs + notarizes + staples the macOS dmgs, publishes
the GitHub Release, then bumps the Homebrew cask.

Bump the version in **three** places first — `Cargo.toml` (workspace),
`apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json` — plus a
`CHANGELOG.md` entry.

**Three install paths, and they do NOT update together.** The Releases page and
`scripts/install.sh` both resolve `releases/latest`, so they follow a new tag
automatically. The **Homebrew cask lives in a separate repo**
(`vonzelle-vzt/homebrew-vzt`, `Casks/vzt-flow.rb`) and pins `version` plus two
literal `sha256`s — nothing in this repo would touch it, so before the
`homebrew` job existed a release silently left every `brew upgrade` user on the
previous build. That bit us at v0.3.3, whose entire content was a crash fix.

The `homebrew` job needs a repository secret **`HOMEBREW_TAP_TOKEN`**: a
fine-grained PAT scoped to `vonzelle-vzt/homebrew-vzt` only, with
**Contents: read and write**. It hashes the *published* dmgs (the bytes users
actually download, not the local artifacts), rewrites the cask via
`.github/scripts/bump_cask.py`, and pushes to the tap.

If that job goes red, **the cask is stale and brew users are stuck** — that is
why a missing token is a hard failure rather than a skipped job. The release
itself is already published by then, so recovery is just re-running the job (or
running `bump_cask.py` by hand against a tap checkout; it takes the same four
arguments and is idempotent). `windows-arm64` going red is *expected* and does
not block anything — it is `continue-on-error: true`, attempt-only.

Pre-release tags (`v0.4.0-rc1`, anything with a `-`) skip the cask bump on
purpose; brew must keep serving the last stable version.

## Shared-worktree hygiene

Multiple agents may be working in `~/vzt-flow` concurrently. Never
`git stash` or `git add -A`/`git add .` — another agent's uncommitted WIP can
be sitting in the same tracked files, and a broad `git add` will **sweep
their in-flight files into your commit** (this has happened here — the Linux
port had to ship a combined green tree because isolating hunks risked
destroying a parallel agent's WIP). **Stage by explicit pathspec only** —
name each file you intentionally changed on the `git add` line, never a
wildcard or directory. `git pull --rebase` before pushing; never force-push.
Note `.claude/agent-memory/` and `.claude/worktrees/` are expected untracked
noise — leave them, and never `git add` them.
