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

## What you get when a meeting ends

The desktop companion keeps a timestamped **Markdown transcript** in
`~/Documents/vzt-flow/meetings/` by default. Stopping capture first drains the
remaining audio, merges your saved notes under **`## My notes`**, generates a
local summary and action items, and exports a **PDF to the Desktop** when
`meeting_pdf` is on. The notes section is omitted when you have not typed any
notes. A custom `meeting_pdf_dir` changes only the PDF destination.

The PDF includes the meeting title, date and duration, summary, action items,
your notes labelled **"Your notes (verbatim — typed by you, not model-generated)"**,
and the full transcript as an appendix. Its footer names the source Markdown
file. It uses a Latin-1-oriented Helvetica font with **WinAnsi/Windows-1252**
encoding, including smart punctuation. Unsupported characters, such as CJK or
emoji, become `·`; the footer counts those replacements. The **`.md` is the
lossless Unicode original** of the recognized transcript and typed notes.

PDF export failure does not remove the Markdown transcript or saved notes and
does not by itself make the meeting fail. The session reports the export error;
use the Markdown file if a PDF cannot be written. Missing or unavailable summary
generation likewise leaves the transcript available. A completed meeting does
not necessarily mean that every optional output succeeded.

The session states are `Recording` → `Stopping` → `Finalizing { step }` →
`Completed`, with `Failed` for a session failure. Finalization steps include
"merging notes", "summarizing i/n", and "writing pdf". The notepad displays
**Recording**, **Finalizing…**, **Saved**, or **Failed**; its file links reveal
the available transcript and PDF in Finder after completion.

## The meeting notepad

When `meeting_notepad` is on, the desktop app opens the notes window with the
meeting. **Closing the window hides it; it does not stop the meeting.** Clicking
inside it activates VZT Flow by design, so you can type there. It is an editable
window, unlike the dictation overlay that avoids taking keyboard focus.

Notes autosave after a 500ms typing pause, and saving is also requested on blur.
They are written atomically beside the transcript as **`<stem>.notes.txt`**.
Successfully saved notes survive a crash; text still waiting for autosave may
not. The footer shows Saved or a save error, and failed saves retry every five
seconds. The sidecar remains separate from the `.md` during recording and is
merged on finalization.

The Markdown notes section uses the heading `## My notes`, followed by exact
marker lines `<!-- vzt-flow:notes:start -->` and
`<!-- vzt-flow:notes:end -->`. These comments are invisible in rendered Markdown.
The disclaimer identifies your notes as typed, not model-generated. Section
replacement matches the marker lines, so a heading you type inside your notes
does not truncate them. Notes also contribute to the summary input.

You can keep editing after completion. If your notes are newer than the PDF,
the window displays **"Edited after the PDF was written"**. Choose **Update
files** to save your current notes and re-export the meeting files.

## Interview mode

Turn on **Interview** in the notepad, or enable **Start meetings with interview
coaching on** in Settings. Coaching is **100% local**: your context, transcript,
and prompts are processed by the on-device model, never sent to a remote model.
In Settings → **Interview mode**, paste your resume, the job description, and
talking points. The editor saves them to **`~/.config/vzt-flow/interview.md`**.
The coach loads a bounded portion of this file when its session starts, so keep
the most useful information near the beginning and save it before the meeting.

Tips fire at the **end of a recognized question from the other speaker**,
after silence closes that audio chunk. Interview mode defaults to a 0.8s silence
hold. Chunks also close at the **30s cap**, so a tip can lag during an
uninterrupted monologue; this is not word-by-word coaching while the interviewer
is still speaking. A tip has a headline, three talking points, and the question's
timestamp and latency. Older tips grey out after 45 seconds. Coaching requests
are latest-only: a newer question supersedes an older request rather than
building a backlog.

Measured on this **M5**, a quiet machine with a roughly **600-token prompt** gave
**p50 2.15s / p95 2.76s** tip latency; under heavy build load, measured p50 was
**3.2–4.4s**. These are tip-generation benchmark results, not a guarantee for the
entire audio-to-tip path or a measurement of meeting finalization time. The
shipped defaults are **`interview_context_max_chars = 2400`** and
**`interview_tip_timeout_ms = 5000`**: at 4,800 context characters, **3 of 8 tips
failed to parse and one recited the resume**. A timeout or rejected response can
leave a question without a tip; context-recitation output is discarded.

The desktop uses a **single resident LLM**, owned by `cleanup_manager`, for
dictation cleanup, coaching, and meeting summaries. Dictation cleanup can
preempt a running summary, while pending coaching keeps only the latest request.
Preemption is bounded so repeated dictations cannot starve a summary forever.
There is no second summary model loaded alongside the dictation model.

## Meeting companion settings

These nine settings live in `~/.config/vzt-flow/config.toml`. Settings exposes
the notepad, PDF, and interview switches and the PDF directory; the remaining
values tune generation and chunking. `meeting_auto` remains the separate
Ask/Auto/Off setting described above.

| Key | Default | Meaning |
|---|---|---|
| `meeting_notepad` | `true` | Open the notes window when a meeting starts. |
| `meeting_pdf` | `true` | Write a PDF when a meeting ends. |
| `meeting_pdf_dir` | `""` | Empty selects the Desktop; otherwise use this directory. |
| `meeting_interview` | `false` | Start meetings with interview coaching on. |
| `interview_tip_timeout_ms` | `5000` | Deadline for one coaching tip, in milliseconds. |
| `interview_silence_hold_secs` | `0.8` | Other-speaker silence hold while interview mode is on; ordinary mode uses 1.2s. |
| `interview_context_max_chars` | `2400` | Maximum leading characters of interview context supplied to the coach. |
| `meeting_summary_window_chars` | `6000` | Initial summary window size; grows to bound the number of window passes. |
| `meeting_summary_partial_timeout_ms` | `25000` | Deadline for each partial-summary pass, in milliseconds. |

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

## My notes
<!-- vzt-flow:notes:start -->
> _Typed by you during the meeting; not model-generated._
Ask finance about the revised budget.
<!-- vzt-flow:notes:end -->

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
- Long meetings use hierarchical summaries: split the transcript and notes into
  windows based on `meeting_summary_window_chars`, summarize the windows, then
  merge the partial summaries. The window size grows for long inputs to keep at
  most 12 window passes, followed by one final merge pass (up to 13 model calls).
- A coverage note reports full or partial coverage and the section count. It
  replaces the old `_(summary of final portion)_` label. Failed or timed-out
  partials are marked as unavailable; they do not erase the transcript.

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
