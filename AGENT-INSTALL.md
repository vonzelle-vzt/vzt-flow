# AGENT-INSTALL.md — installing VZT Flow with an AI agent

This file is written for an **AI coding agent with shell access** (Claude
Code, or anything comparable) that has been pointed at this repo and asked to
install [VZT Flow](README.md) for its human. It is a runbook, not prose: run
the steps in order, verify with the stated command, and stop where it says to
stop.

Humans: you don't need this file. Use the
[one-liner](README.md#macos-one-liner). If you'd rather have your agent do it,
paste this into Claude Code:

> Install VZT Flow on this machine by following
> https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/AGENT-INSTALL.md

---

## 0. Preflight — read before running anything

**What gets installed, and where.** Nothing here is a package manager; the
installer writes to exactly these paths:

| Path | What |
|---|---|
| `/Applications/VZT Flow.app` | The menu-bar app (macOS) |
| `/usr/local/bin/flow`, else `~/.local/bin/flow` | The `flow` CLI |
| `~/.vzt-flow/mcp/` | The MCP server (`index.js` + `node_modules`) |
| `~/.config/vzt-flow/models/` | Parakeet ASR (~640 MB), optional cleanup LLM (~1.1 GB) |
| `~/.claude.json` (via `claude mcp add --scope user`) | MCP registration |

**Network + time budget.** The release assets are ~40 MB. Models are the
expensive part: Parakeet is a 456 MB download, the optional cleanup LLM
another ~1.1 GB. On a slow link the model step can exceed a default shell-tool
timeout — see [Step 2](#2-download-the-models).

**Platform gate.** Check `uname -s` / `uname -m` first and tell your human
what they're getting:

| Platform | Reality | Installer | Installs |
|---|---|---|---|
| macOS Apple Silicon | Supported, tested | `scripts/install.sh` | app + CLI + MCP |
| macOS Intel | CI-built, CPU-only inference, never run on real Intel hardware | `scripts/install.sh` | app + CLI + MCP |
| Linux x86_64 | Experimental; CI-built, never run on real Linux hardware. X11 full, Wayland degraded. No cleanup LLM, no meeting mode | `scripts/install.sh` | app + CLI + MCP |
| Windows x64 | Experimental; CI-built, never run on real Windows hardware. No cleanup LLM, no per-app profiles | `scripts/install.ps1` | **app only** |

**Windows is the app only.** `install.ps1` does not package the `flow` CLI or
the MCP server — that packaging doesn't exist yet. Steps 2 and 4 below assume a
`flow` binary and therefore do not apply on Windows; the app's own tray still
works, and the CLI can be built from source
([docs/USAGE-Windows.md](docs/USAGE-Windows.md)). Don't promise your human an
MCP `listen` tool on Windows.

Anything else (Windows on Arm, 32-bit, BSD): stop and say so. Don't improvise a
build from source unless asked.

**Consent.** This writes to `/Applications`, may invoke `sudo` (Linux `.deb`
path only), downloads ~1.6 GB if models are included, and modifies the user's
`claude mcp` configuration. Confirm with your human before Step 1 unless they
already told you to go ahead. If they asked for "just the app, no models," pass
`INSTALL_MODELS=none`.

**What you cannot do.** macOS permission grants (Microphone, Accessibility,
Input Monitoring) are TCC-protected: they cannot be granted from the shell, by
`sudo`, or by editing a plist. Step 3 is a human step. Do not attempt to
automate it, and do not report the install as complete before it happens — the
hotkey does not work without it.

---

## 1. Install the app, CLI, and MCP server

macOS / Linux:

```bash
curl -fsSL https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.sh \
  | INSTALL_YES=1 NO_LAUNCH=1 INSTALL_MODELS=none bash
```

Windows (PowerShell):

```powershell
iwr https://raw.githubusercontent.com/vonzelle-vzt/vzt-flow/main/scripts/install.ps1 -UseBasicParsing | iex
```

The flags exist for you specifically:

| Flag | Why an agent wants it |
|---|---|
| `INSTALL_YES=1` | Skips the "overwrite existing app?" prompt. Without it the installer blocks on `read` and your shell call hangs. |
| `NO_LAUNCH=1` | Doesn't `open` the app. Launch it in Step 3, once, at the moment the user is looking at the screen to answer permission dialogs. |
| `INSTALL_MODELS=none\|asr\|all` | Model download. Left at `none` here on purpose — see Step 2. |
| `GITHUB_TOKEN` | Only if unauthenticated GitHub API calls are rate-limited (60/hr per IP). Not normally needed; the repo is public. |

The installer registers the MCP server itself if the `claude` CLI is on PATH.
If it wasn't, register it after the fact:

```bash
claude mcp add vzt-flow --scope user -- node "$HOME/.vzt-flow/mcp/index.js"
```

**Already installed via Homebrew?** `brew install --cask vonzelle-vzt/vzt/vzt-flow`
installs the `.app` only. Running the script afterward is the supported path —
it detects the brew-installed app and won't overwrite it — and is how you get
the CLI and MCP server.

---

## 2. Download the models

Parakeet (speech-to-text) is **required**; nothing transcribes without it. The
cleanup LLM is **optional** — it powers `clean`/`polish` modes, and `raw`/`code`
modes never touch it. macOS only for now; Windows and Linux have no cleanup LLM.

Run these as separate commands rather than folding them into Step 1's
`INSTALL_MODELS`, because a 456 MB (or 1.6 GB) download will blow past the
default timeout on most agent shell tools and you'll lose the process:

```bash
flow models download parakeet-v3   # required — 456 MB down, ~640 MB on disk
flow models download cleanup       # optional — ~1.1 GB, macOS only
```

Give each one a **generous explicit timeout** (10 minutes) or run it
backgrounded and poll. Both are idempotent and resumable-by-re-running: a
failed or interrupted download is fixed by running the same command again, and
`--force` re-downloads a model you suspect is corrupt.

If `flow` isn't found, the CLI landed in `~/.local/bin` and that directory isn't
on PATH. Call it by absolute path rather than editing the user's shell profile
without asking.

`INSTALL_MODELS=asr` (Parakeet) or `INSTALL_MODELS=all` (both) does the same
work inline during Step 1 — correct for CI or an unattended box, wrong for an
interactive agent session that will time out.

---

## 3. Permissions — hand this to your human

Launch the app once, then stop and ask:

```bash
open -a "VZT Flow"
```

macOS will prompt for permissions as they're first needed. If the prompts were
dismissed, these open the exact panes:

```bash
open "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
open "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
```

Three grants, all required, none optional:

1. **Microphone** — records the audio.
2. **Accessibility** — synthesizes the paste keystroke, reads the focused field
   for paste verification.
3. **Input Monitoring** — the `CGEventTap` that watches for the Right Option
   hotkey.

Tell your human, verbatim, what to do: *open System Settings → Privacy &
Security, enable VZT Flow under each of those three, and quit-and-relaunch the
app afterward.* Toggling a grant does not apply to an already-running process.

On **Linux** there is only a microphone grant; there is no accessibility grant
to make. On **Windows** there are none of these.

---

## 4. Verify — and don't skip this

`flow doctor` is the oracle. It reports every piece of state this install
touches, so read its output rather than assuming:

```bash
flow doctor
```

A healthy macOS install prints, among other lines:

```
flow-cli version: 0.2.0
Parakeet v3 model: PRESENT
Default input device: MacBook Air Microphone (48000 Hz, 1 channel(s))
Cleanup model: PRESENT
Daemon socket: PRESENT and alive (/Users/you/.config/vzt-flow/daemon.sock)
MCP registration: vzt-flow IS registered with `claude mcp`
```

`Parakeet v3 model: MISSING` means Step 2 didn't finish. `Daemon socket` absent
means the app isn't running — fine if you passed `NO_LAUNCH=1` and haven't
opened it yet, a problem otherwise.

Then prove the transcription pipeline actually works, end to end, on real
audio. This needs no microphone, no permissions, and no network — `say` and
`afconvert` ship with macOS:

```bash
say -o /tmp/vzt-check.aiff "the quick brown fox jumps over the lazy dog"
afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/vzt-check.aiff /tmp/vzt-check.wav
flow transcribe /tmp/vzt-check.wav
```

Expected — the sentence back, at a realtime factor around 0.1–0.2x on Apple
Silicon:

```
Transcription wall time: 0.558s | audio: 2.92s | realtime factor: 0.191x

Segments:
  [0.00s - 2.84s] The quick brown fox jumps over the lazy dog.
```

If that transcript is right, the models and the ASR engine are good. Note what
it does **not** cover: microphone capture, the global hotkey, and the paste
step all depend on Step 3's grants and can only be verified by a human holding
Right Option and talking. Ask them to, then report.

Last, confirm the MCP server is reachable:

```bash
claude mcp list   # expect: vzt-flow ... ✔ Connected
```

A fresh `claude` session is required before the `listen` /
`transcribe_file` / `dictation_history` / `meeting_transcript` tools appear.

---

## 5. Report honestly

Tell your human exactly this shape of thing, with the parts that are true:

- Installed: app at `/Applications`, CLI at `<path>`, MCP registered.
- Models: Parakeet present; cleanup LLM present / skipped.
- Verified: `flow doctor` clean, TTS round trip transcribed correctly.
- **Not** verified by me: mic capture, hotkey, paste — needs the three
  permission grants and a human saying a sentence.
- Next: hold Right Option, talk, release. Transcript pastes at the cursor.

Don't claim the hotkey works. You have no way to know.

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| Installer hangs with no output | `read -p` overwrite prompt; you forgot `INSTALL_YES=1` | Kill it, re-run with the flag |
| `no release asset matching '*.dmg' found` | GitHub API rate limit (60/hr per IP, unauthenticated) | Set `GITHUB_TOKEN`, or install `gh` — the script prefers `gh release download` |
| `flow: command not found` | CLI went to `~/.local/bin`, not on PATH | Call it by absolute path; suggest the `export PATH=` line, don't edit their profile unprompted |
| `Parakeet v3 model: MISSING` | Step 2 skipped or interrupted | Re-run `flow models download parakeet-v3` — idempotent |
| Hotkey does nothing, app is running | Input Monitoring / Accessibility not granted | Step 3, then **quit and relaunch** the app |
| Hotkey stopped working right after a rebuild | Every unsigned `cargo tauri build` mints a new code signature and macOS silently revokes the old one's grants | Remove **and re-add** VZT Flow in both panes. See [the rebuild gotcha](docs/USAGE-macOS.md#the-rebuild-drops-permissions-gotcha) |
| `clean`/`polish` produce raw text | Cleanup model missing, or generation missed the 2500 ms deadline | `flow models download cleanup`; the raw-on-deadline fallback is by design |
| MCP tools absent in Claude Code | Registered after the session started | Restart `claude`; check `claude mcp list` |
| Transcript on clipboard, "paste may have failed" | Secure or unreadable focused field | Press Cmd+V. Expected in password fields and some Electron apps |

Deeper: [docs/USAGE-macOS.md](docs/USAGE-macOS.md) ·
[docs/USAGE-Windows.md](docs/USAGE-Windows.md) ·
[docs/USAGE-Linux.md](docs/USAGE-Linux.md) ·
[docs/MEETINGS.md](docs/MEETINGS.md)

---

## Uninstall

```bash
claude mcp remove vzt-flow --scope user
rm -rf "/Applications/VZT Flow.app" ~/.vzt-flow
rm -f /usr/local/bin/flow ~/.local/bin/flow
rm -rf ~/.config/vzt-flow          # config, history, and the ~1.7 GB of models
```

Confirm the last line with your human before running it — it deletes their
config, dictionary, snippets, and dictation history along with the models.
Leave the revoked permission entries in System Settings; macOS prunes them.
