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
for the full numbers.

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

## Shared-worktree hygiene

Multiple agents may be working in `~/vzt-flow` concurrently. Never
`git stash` or `git add -A` — another agent's uncommitted WIP can be sitting
in the same tracked files and either command can interleave or clobber it.
`git pull --rebase` before pushing; stage only the specific files you
intentionally changed.
