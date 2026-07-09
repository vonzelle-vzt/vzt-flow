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

## No-terminal usage (menu-bar app)

You don't need the terminal. The VZT Flow **menu-bar app** transcribes meetings
from its tray menu, and can **auto-detect** a Zoom/Meet/Teams call and offer to
start for you.

**Tray menu items:**

- **Start meeting transcription** / **Stop meeting transcription (● recording)**
  — a single toggle. Starting captures system + mic audio (same engine as the
  CLI) and writes the transcript live; stopping generates the summary and shows
  a **"Transcript ready"** notification.
- **Open meetings folder** — reveals `~/Documents/vzt-flow/meetings/` in Finder.
- **Meeting auto-detect ▸ Ask / Auto / Off** — see below.

### Auto-detect modes

VZT Flow can notice when you're in a call and act automatically. The mode is set
from the tray submenu (**Meeting auto-detect**) and stored in `config.toml` as
`meeting_auto`:

| Mode        | Behavior                                                                 |
|-------------|--------------------------------------------------------------------------|
| `ask` (default) | On detecting a call, shows a **notification** asking you to start. Click the menu-bar icon → **Start meeting transcription** to begin. |
| `auto`      | On detecting a call, **starts transcribing immediately** and shows a "Transcribing meeting…" notification. |
| `off`       | No detection. Only the manual tray toggle starts a meeting.              |

When a detected meeting ends (or you stop it manually), the session stops, the
summary is generated, and a **"Transcript ready"** notification names the file.

> The "ask" prompt instructs you to click the tray item rather than offering an
> in-notification button — the bundled notification plugin has no reliable
> cross-version action-button/click callback, so we ship the robust path.

Hold-to-talk **dictation keeps working while a meeting is being transcribed** —
the two use independent microphone streams (macOS CoreAudio shares the input
device across streams), so you can dictate into another app mid-call.

### How detection works (privacy)

Detection is **100% local and metadata-only — window titles, never pixels**. No
screenshots, no OCR, no audio inspection, no network. It combines two cheap
signals polled every 5 seconds:

- **A meeting window is open** — the app reads on-screen window *titles* via
  `CGWindowListCopyWindowInfo` and matches them against a small table (Zoom's
  "Zoom Meeting" window, a browser tab titled "Meet – …" / `meet.google.com`, a
  "Microsoft Teams" window titled with "Meeting"). Reading other apps' window
  titles requires the **Screen Recording** permission — the same grant meeting
  capture already needs — so detection is inactive (and logs a one-time note)
  until it's granted.
- **The microphone is live** — a single CoreAudio boolean
  (`kAudioDevicePropertyDeviceIsRunningSomewhere`).

A meeting is only considered active when **both** hold for two consecutive polls
(debounced against transient matches), and only considered ended after the
window has been gone for three consecutive polls — muting yourself (which turns
the mic-live signal off) never ends a meeting.

## Usage (CLI)

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
