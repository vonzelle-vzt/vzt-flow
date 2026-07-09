# Meeting mode

`flow meeting` live-transcribes a video call (Zoom, Google Meet, Microsoft
Teams, or anything that plays audio) **fully locally** — no audio ever leaves
your machine. It captures two sources at once:

- **System / application audio** (the other participants) via Apple's
  **ScreenCaptureKit**.
- **Your microphone** (you) via a dedicated capture stream.

Both streams are transcribed by the same local Parakeet engine and written to a
timestamped, speaker-labelled Markdown file. When you stop the meeting, a local
LLM (the same Qwen3 GGUF used for dictation cleanup) appends a summary and
action items.

> macOS only. ScreenCaptureKit (system-audio capture) is a macOS 13+ framework.

## Usage

```bash
# Start a meeting. Ctrl+C stops it and appends the summary.
flow meeting --title "Weekly Sync"

# Choose an output directory (default: ~/Documents/vzt-flow/meetings/).
flow meeting --title "Design Review" --out ~/notes/calls

# List recent transcripts (newest first).
flow meeting list          # last 10
flow meeting list -n 25
```

Live transcript lines are mirrored to the terminal (on stderr) as they are
recognized; the final transcript path is printed to stdout on exit.

### Requirements

Both models must be downloaded first (one-time):

```bash
flow models download parakeet-v3   # speech-to-text
flow models download cleanup       # summary LLM (Qwen3-1.7B GGUF)
```

## Permissions (Screen Recording)

System-audio capture requires the **Screen Recording** permission (macOS treats
system audio as part of screen capture).

- Grant it in **System Settings › Privacy & Security › Screen Recording**.
- When you run `flow` **from a terminal**, the permission belongs to the
  **terminal app** (Terminal, iTerm, Ghostty, …) — enable the checkbox for that
  app, **not** for `flow`.
- After granting it for the first time, you may need to **quit and reopen the
  terminal** for the grant to take effect.

`flow meeting` checks the permission on startup (via
`CGPreflightScreenCaptureAccess`) and prints this guidance if it's missing; it
will also trigger the one-time system prompt (`CGRequestScreenCaptureAccess`).

If capture runs but no participant audio appears, `flow meeting` prints a
per-source diagnostic when it stops, e.g.:

```
[vzt-flow] system (SCK) source: 148 blocks, 15.4s audio, peak amplitude 0.2100
[vzt-flow] mic source: 9 blocks, 1.2s audio, peak amplitude 0.0400
```

A `peak amplitude … (SILENT — nothing usable captured)` line means that source
delivered no usable audio — most often a missing/updated Screen Recording grant
for the terminal app.

## Headphones (echo separation)

**Wear headphones for the best speaker separation.** Without them, your
microphone also picks up the other participants coming out of your speakers, a
beat after ScreenCaptureKit already captured the same audio from the system
mix. That would create duplicate lines attributed to you.

Meeting mode guards against this with an **echo filter**: a `Me:` line is
dropped when it overlaps a `Them:` line in time **and** is textually
near-identical to it (normalized-token Jaccard similarity > 0.7). Short
back-channel interjections ("yeah", "right", "makes sense") are kept — they're
legitimate speech and rarely a word-for-word match. Headphones remove the echo
at the source and are still the recommended setup.

## Transcript file format

```markdown
# Meeting: Weekly Sync — 2026-07-08 20:15

[00:00:03] Them: Thanks everyone for joining. The deadline is next Friday.
[00:00:11] Me: Got it — I'll own the budget update.
[00:00:19] Them: Sarah will finalize the mockups by Wednesday.

## Summary
- The launch deadline is next Friday; the team agreed to move quickly.
- Ownership was split between the budget update and the design mockups.

## Action items
- [ ] Sarah — finalize the design mockups by Wednesday
- [ ] Me — send the updated budget to finance
```

- Timestamps are the meeting-relative offset (`HH:MM:SS`) at the start of each
  chunk.
- Lines are written **immediately** and flushed to disk, so a crash mid-meeting
  keeps everything transcribed so far.
- Personal-dictionary corrections (product names, jargon) are applied per line.
- For a very long meeting, the summary is generated from the final portion of
  the transcript and marked `_(summary of final portion)_` so it never exceeds
  the LLM's context window.

## MCP tool

The bundled MCP server exposes meeting transcripts to Claude Code (and any MCP
client) via the `meeting_transcript` tool:

```jsonc
// Latest meeting (index 0):
{ "name": "meeting_transcript", "arguments": { "meeting": 0 } }

// By filename:
{ "name": "meeting_transcript", "arguments": { "meeting": "2026-07-08-weekly-sync" } }
```

It returns the transcript text (truncated to a head + tail if longer than
50,000 characters). Point it at a non-default directory with the
`FLOW_MEETINGS_DIR` environment variable.

Example uses: "summarize my last meeting", "pull the action items from the
design review", "what did we decide about the deadline?"
