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
`.lock().unwrap()` on `AppState` in the desktop crate** — the only remaining
occurrences are in tests (one poisons a mutex deliberately), so a grep hit
outside `#[cfg(test)]` is a regression. Keep `run_coordinator`
borrowing its receiver rather than owning it (a moved receiver is dropped when
the first pass unwinds, so the restart has no channel to serve — pinned by
`a_panicking_pass_does_not_consume_the_channel`).

**(j) Before diagnosing "the hotkey stopped working", establish which build is
running and whether the threads are actually alive.** On 2026-08-01 that
symptom was neither a crash nor a permission: `/Applications/VZT Flow.app` was
an **ad-hoc-signed v0.3.2 dev build** copied there on Jul 29, while the repo
was on v0.3.3 — the release whose entire content was the (h) crash fix.
Installing the notarized v0.3.3 dmg fixed it outright. Three checks settle in
seconds what is otherwise an hour of theorising, and they should run *before*
any code is read:

- **Identity.** `codesign -dv --verbose=2 "/Applications/VZT Flow.app"`.
  `flags=0x2(adhoc)` + `TeamIdentifier=not set` is a local build; a release is
  `flags=0x10000(runtime)` + `TeamIdentifier=LKHKU5BW73` and passes
  `spctl -a -vvv -t install` with `source=Notarized Developer ID`. Check the
  version too (`PlistBuddy -c "Print :CFBundleShortVersionString"`), and note
  `flow status` prints the *daemon's own* `version:` — trust that over what
  you assume is installed.
- **Liveness.** `sample <pid> 2 -f /tmp/s.txt`, then read the thread names.
  `vzt-flow-hotkey-tap` parked in `__CFRunLoopRun`/`mach_msg` means the tap
  armed — its thread returns early if `CGEventTap::new` fails, so a live
  runloop *is* proof Input Monitoring was granted. An unnamed thread inside
  `run_coordinator` blocked in `Channel::recv` means the coordinator is
  healthy: that is how you rule out (i) when there is no crash report to find.
  An active recording additionally shows `caulk.*` CoreAudio threads, so their
  presence/absence tells you whether capture is really running.
- **Permissions, from the system's own record** instead of guesswork:
  ```bash
  log show --last 2m --predicate 'subsystem == "com.apple.TCC"' --style compact \
    | grep -E "AUTHREQ_RESULT|kTCCServiceMicrophone"
  ```
  `authValue=2` = allowed, `0` = denied. This is what proved Microphone was
  granted while every other signal still pointed at permissions.

**(k) The coordinator and the audio worker can disagree about whether a
recording is live, and the recovery command must work anyway.**
`start_recording` sets `dictation_state = Recording` when it *sends*
`AudioCommand::Start` — before the worker has dequeued it — so the two are
never synchronously in step. Through **0.3.3** the audio worker's *outer*
command loop answered `AudioCommand::Stop | AudioCommand::Cancel` with `{}` —
no reply, no state change — so a disagreement between the two had nothing that
could ever resolve it. Fixed in 0.3.4: that arm now replies
`AudioReply::NotRecording`, and the coordinator reconciles to Idle **only if it
still believes it is `Recording`**, guarded so a late ack can't tear down an
in-flight transcription (Transcribing) or fire spuriously (Idle). Pinned by
`cancel_with_no_recording_is_acknowledged_not_swallowed`, which fails by
timeout against the old `=> {}` arm.

**Read the rest of this before you trust the fix.** On 2026-08-01 the app was
twice observed genuinely stuck at `state: recording` — persisting across
minutes, several `flow cancel`/`flow toggle` calls and two process samples,
CoreAudio threads live, coordinator *and* audio worker both idle in `recv`,
cleared only by restarting. The `{}` arm is a real defect and a *sufficient*
explanation for a stall nothing can clear, which is why it was fixed. It was
never shown to be *the* cause, and **the original stall has never been
reproducible on demand, on any version** — so treat 0.3.4 as removing a known
hole, not as a confirmed cure.

**Do not measure this by sampling state right after `flow cancel` returns.** A
start and cancel issued back-to-back leave state legitimately `recording` for
under a second while CoreAudio opens the input device. An earlier pass read that
as a wedge and published "3 in 8 cycles"; adding a 1s delay before the cancel
gives 0/8, and on 0.3.4 a long-window measurement reaches idle within 0–1s in
6 of 6. Sub-second `recording` after a cancel is normal — only a state that
*stays* put is a fault, so always measure with a multi-second window.

Finally, **the symptom is invisible to hold-to-talk** — it only ever showed via
the tray toggle, the daemon socket and MCP `listen` — so a health check that
drives dictation must assert `flow status` returns to `idle`, with a settle
window, rather than assuming it.

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

**Three install paths, and nothing in THIS repo updates the third.** The
Releases page and `scripts/install.sh` both resolve `releases/latest`, so they
follow a new tag within seconds. The **Homebrew cask lives in a separate repo**
(`vonzelle-vzt/homebrew-vzt`, `Casks/vzt-flow.rb`) and pins `version` plus two
literal `sha256`s, so it used to need a manual bump — miss it and every
`brew upgrade` user stays on the old build with no failure anywhere to notice.
That nearly bit us at v0.3.3, whose entire content was a crash fix.

**The tap now updates itself and needs nothing from us.** Its own `auto-bump`
workflow polls this repo's `releases/latest` daily, and on a new stable version
downloads the published dmgs, hashes them, rewrites the cask and commits. It
lives there rather than here deliberately: pushing into the tap from this
workflow would need a PAT stored as a secret, whereas polling from inside the
tap uses its built-in `GITHUB_TOKEN` — no secret to leak, expire, or rotate.
So **cutting a release is just the tag; the cask follows within a day.**

Two things worth knowing. To get a release into brew immediately rather than
waiting for the cron, hit **Run workflow** on `auto-bump` in the tap. And
GitHub disables scheduled workflows after **60 days of repository inactivity** —
each bump is a commit so a normal cadence self-sustains, but after a 2+ month
release gap check the tap's Actions tab (GitHub emails first) or just run it
manually. Pre-release tags are skipped on purpose; brew keeps serving the last
stable version.

`windows-arm64` going red is *expected* and blocks nothing — it is
`continue-on-error: true`, attempt-only.

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
