---
name: verify-dictation
description: The canonical "prove VZT Flow actually works" checklist — the human hotkey test plus its automated equivalents. Use before claiming a dictation-pipeline change works, before a release, or when the user runs /verify-dictation.
---

# Verify dictation

VZT Flow's core promise is "hold a key, talk, transcript lands wherever your
cursor is." This checklist proves that promise end to end, with real audio
and real numbers — not "the code compiles so it probably works."

## 0. Prerequisites

```bash
source ~/.cargo/env
cargo build --release --workspace
./target/release/flow doctor
```

`flow doctor` must show both models present (Parakeet always; Qwen3 cleanup
if you're testing `clean`/`polish`). If a model is missing:
```bash
./target/release/flow models download parakeet-v3
./target/release/flow models download cleanup   # only if testing clean/polish
```

## 1. The human hotkey test (do this yourself, not via script)

1. Launch the desktop app (or relaunch a dev build — see CLAUDE.md gotcha
   (a): use `nohup <dev-binary> &`, never `open`, and never kill the user's
   running daily-driver instance without relaunching it for them).
2. Hold **Right Option**, speak a full sentence, release. Confirm: overlay
   shows Recording → Transcribing (with a mode badge) → Done, and the
   transcript pastes into whatever's focused.
3. **Tap** Right Option (press+release under 300ms). Confirm hands-free
   starts, then stops on its own after ~2.5s of silence (or tap again to
   stop manually).
4. Hold, start speaking, hit **Esc** mid-recording. Confirm nothing is
   transcribed or pasted — the overlay just clears.
5. Focus a password field, dictate. Confirm the overlay shows "Secure field
   — transcript on clipboard" and nothing gets typed into the field.

This step has no automated equivalent for the actual key event capture and
paste — it depends on real Accessibility/Input Monitoring grants and a real
frontmost app. Everything below automates what *can* be automated.

## 2. Automated equivalents

**ASR accuracy + speed** (real TTS audio, not silence):
```bash
say -o /tmp/flow-verify.aiff "the quick brown fox jumps over the lazy dog near the supabase database"
ffmpeg -y -i /tmp/flow-verify.aiff /tmp/flow-verify.wav
./target/release/flow transcribe /tmp/flow-verify.wav
```
Check: transcript is accurate, "supabase" comes back correctly cased (tests
the dictionary pass), and the RTF line on stderr is in the expected ballpark
(~0.1x / ~10x realtime on Apple Silicon — see `docs/PRD.md` for the measured
baseline; flag anything meaningfully worse as a regression).

**Cleanup deadline behavior:**
```bash
./target/release/flow clean-test "um so like I think we should uh go with option two" --mode clean
./target/release/flow clean-test "a normal sentence" --mode clean --timeout-ms 1
```
The second command forces the deadline miss — confirm it reports the raw
fallback path, not an LLM result, and doesn't hang.

**Code mode (deterministic, no model):**
```bash
./target/release/flow code-test "const camel case user profile equals await get user open paren close paren"
```
Expect exactly: `const userProfile = await getUser()`

**Paste mechanics in isolation:**
```bash
./target/release/flow paste-test "vzt-flow verify $(date +%s)"
```

**Daemon socket** (only if the desktop app is already running — check with
`flow status` first, don't launch an extra instance):
```bash
./target/release/flow status
./target/release/flow toggle   # start hands-free
./target/release/flow toggle   # stop it
./target/release/flow history -n 5
```

**Long-audio safety** (only relevant if touching ASR/chunking — see CLAUDE.md
gotcha (b)): never feed >60s of audio straight into `flow transcribe`
without going through the chunked path first; watch `ps -o rss` on the
`flow`/desktop process while it runs and compare against
`docs/PRD.md`'s memory-budget numbers.

## 3. Overlay visual QA

Use the tray's **Test overlay** item — cycles Recording→Transcribing→Done
with no mic/model involved. This is the reliable way to screenshot overlay
states; scripted clicks on the menu-bar extra itself are documented as
unreliable on this multi-monitor dev machine.

## 4. Report format

For each section above: command/action → actual output/observation →
pass/fail. Real numbers only (RTF, RSS, timing) — no "should be fine."
Anything that can't be verified in the current environment (e.g. no
Windows hardware, no physical hotkey access in a headless agent run) must
be called out explicitly as unverified, not silently skipped.
