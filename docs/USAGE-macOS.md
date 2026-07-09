# VZT Flow — macOS Usage Guide

macOS is the primary, actively-developed platform for VZT Flow. Everything on
this page has been verified against the code in this repo. For the
experimental Windows build, see [USAGE-Windows.md](USAGE-Windows.md).

## Install

Models are **not** bundled with the app — they download separately (see
[First run](#first-run-model-downloads) below).

### Option A: build from source

Prerequisites:
- Rust (stable) — `rustup default stable`
- Node 24+
- Xcode command line tools (for the `ApplicationServices`/`AppKit` frameworks
  and Metal)
- `ffmpeg` on PATH (optional — only needed for `flow transcribe` on
  non-`.wav` files)

```bash
git clone https://github.com/vonzelle-vzt/vzt-flow.git
cd vzt-flow

# CLI
cargo build --release -p flow-cli
./target/release/flow doctor        # sanity check + points out anything missing
./target/release/flow models download parakeet-v3
./target/release/flow models download cleanup   # optional, needed for clean/polish modes

# Desktop app
cd apps/desktop
npm install
cargo install tauri-cli --version "^2"
cargo tauri build          # unsigned/local build; fine for personal use
open ../../target/release/bundle/dmg/*.dmg
```

Note: because it's a Cargo workspace, `cargo tauri build` (run from
`apps/desktop`) actually places the bundle under the **workspace-root**
`target/` directory (i.e. `target/release/bundle/dmg/` relative to the repo
root, `../../target/release/bundle/dmg/` relative to `apps/desktop`), **not**
`apps/desktop/src-tauri/target/...` — the `open` command above accounts for
this.

### Option B: grab the CI-built .dmg from GitHub Actions

Every push/PR to `main` runs `.github/workflows/build.yml`'s `macos` job,
which builds an unsigned `aarch64-apple-darwin` bundle and uploads it as an
artifact named `vzt-flow-macos-aarch64-dmg` (containing a single `.dmg`,
named by Tauri's own `{productName}_{version}_{arch}.dmg` convention — e.g.
`VZT Flow_0.1.0_aarch64.dmg`).

Via the `gh` CLI:

```bash
# Latest successful run on main:
gh run list --workflow=build.yml --branch=main --status=success --limit 1
gh run download <run-id> --name vzt-flow-macos-aarch64-dmg --dir ./vzt-flow-dmg
open ./vzt-flow-dmg/*.dmg
```

Via the Actions UI: open the repo → **Actions** → **build** → pick a green
run → scroll to **Artifacts** → download `vzt-flow-macos-aarch64-dmg`.

An Intel Mac build is also produced, as artifact `vzt-flow-macos-x86_64-dmg`
(dmg named `VZT Flow_0.1.0_x64.dmg` by Tauri's own arch-naming convention —
see [Hardware requirements](#hardware-requirements) below for what's
different about it).

### Building from source on Intel (x86_64-apple-darwin)

`ort` (the ONNX Runtime bindings `transcribe-rs` uses for Parakeet) ships
**no prebuilt binaries for `x86_64-apple-darwin`** — verified directly:
`cargo check --target x86_64-apple-darwin` fails with `ort does not provide
prebuilt binaries for the target x86_64-apple-darwin`. The workaround is to
link against Microsoft's own official ONNX Runtime release for that target
instead of `ort`'s prebuilt-binary CDN:

```bash
curl -fsSL -o ort-x64.tgz \
  https://github.com/microsoft/onnxruntime/releases/download/v1.22.0/onnxruntime-osx-x86_64-1.22.0.tgz
mkdir -p /tmp/ort-x64 && tar xzf ort-x64.tgz -C /tmp/ort-x64 --strip-components=1

export ORT_STRATEGY=system
export ORT_PREFER_DYNAMIC_LINK=1
export ORT_LIB_PATH=/tmp/ort-x64/lib

cargo build --release --target x86_64-apple-darwin -p flow-cli
```

That builds and links cleanly (confirmed locally), but the resulting binary
references `libonnxruntime.dylib` via `@rpath` with no `LC_RPATH` entry set
(the `system` linking strategy doesn't add one), so it won't find the dylib
at runtime unless you either add an rpath yourself
(`install_name_tool -add_rpath <dir-containing-the-dylib> <binary>`) or put
`libonnxruntime.*.dylib` on `DYLD_LIBRARY_PATH`. The CI build
(`.github/workflows/build.yml`'s `macos-x64` job) does this automatically
and bundles the dylib into the `.app`/CLI tarball, which is the easier path
for most people — see [Option B](#option-b-grab-the-ci-built-dmg-from-github-actions)
above.

## Hardware requirements

| | Apple Silicon | Intel (x86_64) |
|---|---|---|
| ASR (Parakeet) | CoreML execution provider (ANE-accelerated) | Plain CPU ONNX — no CoreML |
| Cleanup (`clean`/`polish`, Qwen3) | Metal-offloaded llama.cpp | CPU-only llama.cpp build (no `metal` feature — see `crates/flow-core/Cargo.toml`'s `target_arch` split) |
| Status | Actively developed and tested (this repo's own dev hardware is an M5 MacBook Air) | Built and packaged by CI (`macos-x64` job), cross-compiled from an arm64 runner — never run on real Intel hardware; community testing welcome |
| Minimum OS | macOS 12.0 (`tauri.conf.json`'s `minimumSystemVersion`) | **Effectively macOS 13.3**, not 12.0 — verified via `otool -l` on Microsoft's official `onnxruntime-osx-x86_64-1.22.0` dylib (`LC_BUILD_VERSION`'s `minos` is `13.3`). The app's `Info.plist` still says 12.0 (that value isn't arch-specific in `tauri.conf.json`), but dyld will refuse to load the bundled ONNX Runtime dylib below 13.3, so the Intel build's real floor is higher than what it advertises. Not yet fixed — would need a per-target Tauri config override. |

**Honest performance estimate for Intel:** CPU-only int8 Parakeet is
typically still faster than realtime on modern x86_64 (roughly 5-10x
realtime is a reasonable estimate for a recent Intel Mac, vs. the ~10x this
repo measures on Apple Silicon with CoreML — see the README's ASR section
for the measured number), but cleanup is meaningfully slower without Metal:
a 1.7B-parameter model decoding on CPU alone is commonly single-digit
tokens/sec rather than the ~40-60 tok/s this repo measures on M5 with
Metal. On an older/lower-end Intel Mac, that likely means `clean`/`polish`
routinely miss the cleanup deadline and fall back to the raw transcript
(harmless — dictation still works, just without the LLM rewrite) — consider
setting `cleanup_enabled = false` in `config.toml` (see below), or using
`raw`/`code` mode via `profiles.toml`, on older Intel hardware rather than
paying the load-and-usually-timeout cost every dictation.

**Memory footprint** (measured on this repo's dev machine — an M5
MacBook Air — via `ps`; Intel figures are the same models/runtime and
should be architecturally similar, not independently measured):
- **Idle** (both models unloaded, the common steady state after
  `idle_unload_secs` of inactivity): **~30-40MB RSS** for the desktop app
  process — verified directly against the running dev build (`ps -o rss`
  on `vzt-flow-desktop`, no models loaded).
- **During dictation with ASR loaded**: roughly **~2GB**, engineering
  estimate from the Parakeet int8 ONNX model's ~456MB on-disk footprint
  plus ONNX Runtime session/execution-provider overhead — not independently
  re-measured for this pass.
- **During dictation with cleanup also loaded** (`clean`/`polish` mode):
  roughly **~3.5GB**, estimate adding the ~1.1GB Qwen3 GGUF plus its 8192-
  token llama.cpp context allocation on top of the ASR figure above.
- **Disk**: ~2.5GB total for both models combined (456MB Parakeet archive +
  1.1GB Qwen3 GGUF, plus some overhead for the extracted/staged files).

**8GB Mac note:** VZT Flow works on an 8GB machine, Apple Silicon or Intel.
The cleanup model is the biggest optional cost — skip installing it
entirely (just never run `flow models download cleanup`) to stay ASR-only
and avoid the ~3.5GB peak, or install it but set `cleanup_enabled = false`
in `config.toml` (`~/.config/vzt-flow/config.toml`) to keep the file
present without ever loading it. Either way, `raw`/`code` mode dictation is
completely unaffected — only `clean`/`polish` need the cleanup model.

## First run: model downloads

VZT Flow needs two local models, neither of which ships in the app bundle:

| Model | Purpose | Size | Auto-downloads? |
|---|---|---|---|
| Parakeet TDT v3 (int8 ONNX) | Speech-to-text (always required) | ~456MB archive | No — the app does **not** auto-fetch it; run `flow models download parakeet-v3` (or `flow models download` with no argument, which defaults to `parakeet-v3`) before first use |
| Qwen3-1.7B-Instruct (Q4_K_M GGUF) | Optional `clean`/`polish` LLM rewrite pass | ~1.1GB | No — run `flow models download cleanup` |

```bash
flow models download            # parakeet-v3 (the default)
flow models download cleanup    # optional — needed for clean/polish modes
flow models download parakeet-v3 --force   # re-download even if already present
```

Both live under `~/.config/vzt-flow/models/`:
- `~/.config/vzt-flow/models/parakeet-v3/` — extracted ONNX encoder/decoder/
  preprocessor files + `vocab.txt`
- `~/.config/vzt-flow/models/cleanup/Qwen3-1.7B-Q4_K_M.gguf`

Run `flow doctor` at any time to check whether both are present, along with
your default input device, `ffmpeg`, the daemon socket, and MCP registration.

Without the cleanup model installed, `clean`/`polish` modes silently fall
back to the dictionary-corrected raw transcript rather than failing — code
mode never needs it (it's a deterministic transform).

## Permissions

VZT Flow needs three grants, all in **System Settings → Privacy & Security**:

1. **Microphone** — prompted automatically the first time the app opens an
   audio input stream (i.e. the first recording you start).
2. **Accessibility** (`System Settings → Privacy & Security → Accessibility`)
   — lets the app simulate Cmd+V to paste the transcript. Without it, the
   transcript is left on the clipboard for you to paste manually
   (`PasteOutcome::SkippedNoAccessibility`).
3. **Input Monitoring** (`System Settings → Privacy & Security → Input
   Monitoring`) — lets the app install a `CGEventTap` to detect the
   hold-to-talk key. Without it, the hotkey silently does nothing (the tray's
   manual "Start/Stop dictation" item still works, and the app logs
   `hotkey monitor failed to install a CGEventTap` to its console).

> [!WARNING]
> ### The rebuild-drops-permissions gotcha
>
> **This app is unsigned (or ad-hoc-signed) in a normal local build.**
> macOS ties Input Monitoring/Accessibility grants to the binary's code
> signature. Every time you `cargo tauri build` (or otherwise rebuild the
> app), the binary gets a **new** signature — and macOS silently **revokes**
> the Input Monitoring and Accessibility grants for the old one. The toggle
> in System Settings can still show "on" while the *new* binary is, in
> reality, unauthorized. There is no error dialog; the hotkey just stops
> doing anything.
>
> **Fix, every time you rebuild:**
> 1. Open System Settings → Privacy & Security → Input Monitoring (and
>    Accessibility).
> 2. If VZT Flow's toggle looks *on* but the hotkey still doesn't work,
>    **remove it from the list (the `-` button) and re-add it** rather than
>    just re-toggling the switch — macOS sometimes won't recognize a signature
>    change without a full remove/re-add.
> 3. Quit and relaunch VZT Flow.
>
> **Permanent fix:** sign the app with a *stable* identity so rebuilds don't
> rotate the signature:
> - If you have an Apple Developer account, set `signingIdentity` in
>   `apps/desktop/src-tauri/tauri.conf.json`'s `bundle.macOS` config to your
>   "Apple Development" certificate's identity (see `tauri signer` /
>   Tauri's macOS code-signing docs), or
> - For local-only use, create and use a self-signed certificate (Keychain
>   Access → Certificate Assistant → Create a Certificate → Code Signing) and
>   pass its identity the same way.
>
> Either way, once the app is signed with an identity that doesn't change
> between builds, macOS keeps the grant across rebuilds.

## Daily use

### Hotkey

- **Hold Right Option** (macOS virtual keycode `61`, `kVK_RightOption`) to
  record — **release** to transcribe and paste. This is a bare-modifier
  binding, monitored via a `CGEventTap` on `FlagsChanged` events (not
  `tauri-plugin-global-shortcut`, which can't register modifier-only
  shortcuts on macOS).
- **Tap** Right Option — press and release **faster than the hold
  threshold (300ms by default, `hold_threshold_ms`)** — to start a
  **hands-free** recording instead. Hands-free auto-stops after
  **~2.5s of continuous silence** following at least one loud frame
  (`handsfree_silence_secs`), or tap the key again to stop manually.
- **Esc** cancels an in-progress recording (discarded — nothing is
  transcribed or pasted). The tap is `ListenOnly`, so Escape still reaches
  whatever app is frontmost; only its "cancel recording" *effect* is gated
  on whether a recording is active.
- **Typing a special character never starts dictation.** Right Option is
  macOS's special-character modifier (Option+e = ´, Option+u = ¨, Option+n =
  ˜, …). If you hold Right Option and press **any other key**, VZT Flow treats
  it as typing, not push-to-talk: it never arms hands-free, and if a recording
  had already started (you held past the 300ms threshold before typing), that
  false start is **discarded immediately** — the overlay hides and nothing is
  pasted. Because the tap is `ListenOnly`, the special character itself still
  reaches the app unmodified. This is the *accidental-press guard*.
- Recording is hard-capped so a stuck key or wedged hands-free session can
  never run forever: **600s (10min)** for both a held recording
  (`max_hold_secs`) and hands-free (`max_handsfree_secs`) — sized for
  long-form dictation (holding the key for several minutes at a stretch),
  not just short commands. Hitting the cap auto-stops and transcribes
  whatever was captured — it isn't discarded. The overlay pill shows a
  running mm:ss elapsed timer while recording, and switches to a subtle
  amber warning appearance in the last 30s before the cap.
  > **Long recordings are handled via automatic chunking** (since commit
  > `4757636`). The bundled Parakeet ASR engine has no internal
  > streaming (`supports_streaming: false` in transcribe-rs) and its memory
  > use grows faster than linearly — ≈quadratically — in the length of a
  > single `transcribe` call: measured on an M5 Mac, ~15.6GB peak for 49s of
  > audio, ~37GB for 93s, and an out-of-memory kill at ~146s (2m26s). To keep
  > that behavior from ever reaching the engine, recordings longer than ~35s
  > are transparently split into ~30s chunks (cut at the quietest point of a
  > 25–35s window, so seams land in natural pauses) and transcribed one after
  > another on the same engine, bounding peak memory to a single chunk's
  > footprint. A ~7min (438s) clip now completes in 32.5s (RTF 0.074) with a
  > peak of 8.9GB RSS — comfortably within the 10min (`max_hold_secs`) cap. See
  > `crates/flow-core/src/chunking.rs`. The underlying transcribe-rs quadratic
  > memory growth is unchanged upstream; the chunker is what makes the raised
  > cap safe, so there is no longer a reason to keep long holds under ~90s.
  > **Rolling transcription (on by default, `rolling_transcription`)** goes one
  > step further for long holds: instead of buffering the whole recording and
  > transcribing it *after* you release, silence-completed chunks are
  > transcribed **in the background while you keep talking**, so at release only
  > the final <35s tail still needs transcribing. Measured on the M5 for a
  > ~7min45s clip: end-latency after release drops from **25.15s** (batch,
  > everything transcribed at release) to **0.53s** (rolling, tail only) —
  > identical transcript, 15 chunks transcribed during recording. It reuses the
  > same silence-aware cut and seam-dedup as the chunker, and drops each chunk's
  > audio once transcribed so the capture buffer stays small. Recordings under
  > ~35s are unaffected (no chunks to roll — transcription still happens once at
  > release, exactly as before). See `crates/flow-core/src/rolling.rs`; you can
  > exercise it on any wav with the hidden `flow rolling-test <file>`.
- Rebind the key from the Settings window (tray → **Settings…**).

### Overlay pill

A small floating pill window appears near the bottom-center of your primary
display while dictating, and shows:
- **Recording** — a live level indicator + running mm:ss timer while you talk.
- **Live preview** (long recordings only) — once rolling transcription starts
  cutting chunks (past ~35s), a subdued one-line strip appears **under the
  timer** showing the tail of the transcript so far. It's deliberately dim and
  italic because it's **raw** text — dictionary-corrected but pre-cleanup, so
  the final pasted result (after the `clean`/`polish` LLM pass) may differ.
  Short recordings show only the timer, no preview.
- **Transcribing** — with a small badge for the resolved mode
  (raw/clean/polish/code) for whatever app was frontmost when you stopped
  talking.
- **Done** — a brief confirmation flash before the pill hides.
- **Message** — a short-lived note for non-fatal issues (e.g. "Secure field
  — transcript on clipboard", "No Accessibility permission — transcript on
  clipboard", "Microphone disconnected", "Transcription failed", "Paste may
  have failed — transcript on clipboard").

### Tray menu

Click the mic icon in the menu bar:

| Item | Effect |
|---|---|
| `Status: <idle/recording/…> · model <unloaded/loading…/loaded>` | Read-only status line |
| Start/Stop dictation | Manual hands-free toggle — same effect as tapping the hotkey |
| Copy last transcript | Puts the most recent dictation back on the clipboard |
| Start/Stop meeting transcription (● recording) | Toggle a live meeting transcription (Zoom/Meet/Teams or any call). Stopping generates the summary and notifies "Transcript ready" |
| Open meetings folder | Reveals `~/Documents/vzt-flow/meetings/` in Finder |
| Meeting auto-detect ▸ Ask/Auto/Off | How VZT Flow reacts to a detected call (see below) |
| Settings… | Opens the Settings window (hotkey rebind, permission status, history) |
| Test overlay | Cycles the overlay through Recording→Transcribing→Done with no mic/model involved — visual QA only |
| Launch at login | Checkbox; mirrors `tauri-plugin-autostart` |
| Quit VZT Flow | Exits the app (this is the only path that actually terminates the process — closing all windows does not, since it's a menu-bar-only app) |

### Meeting transcription (no terminal)

The tray transcribes video calls without touching the terminal, and can
**auto-detect** a Zoom/Google Meet/Microsoft Teams call:

- **Ask** (default) — a notification asks you to start; click the menu-bar icon
  → **Start meeting transcription**.
- **Auto** — starts transcribing the moment a call is detected.
- **Off** — no detection; use the manual toggle only.

Detection is **local and titles-only** (window titles + a mic-in-use boolean —
no screenshots, no OCR, no audio inspection, no network) and needs the **Screen
Recording** permission (same one meeting capture uses) to read window titles.
Muting yourself never ends a meeting; the transcript + summary land in the
meetings folder. Hold-to-talk dictation keeps working during a meeting. Full
details, privacy model, and the transcript format are in
[`docs/MEETINGS.md`](./MEETINGS.md).

### Secure fields

If a secure input field (e.g. a password box) is focused when a dictation
finishes, VZT Flow does **not** attempt to synthesize a paste into it — the OS
blocks/flags programmatic input to secure fields anyway. The transcript is
left on the clipboard, and the overlay shows "Secure field — transcript on
clipboard" instead.

### Clipboard behavior

Every paste is a save-set-paste-restore cycle: your existing clipboard
contents are saved, replaced with the transcript, Cmd+V is simulated, and
your previous clipboard contents are restored **~1 second later** — but only
if the transcript is still what's on the clipboard at that point (if you
copied something new in the meantime, VZT Flow leaves it alone rather than
clobbering it).

**Paste verification.** ~150ms after the simulated Cmd+V, VZT Flow reads the
focused field via the Accessibility API and checks that the transcript's tail
actually landed. If the field is readable and the text is **missing**, it
retries the paste once; if it's still missing, the paste is left on the
clipboard (not restored) and the overlay shows "Paste may have failed —
transcript on clipboard" plus a notification. If the focused field's value
isn't readable — most web pages, Electron apps, and secure fields don't expose
it — verification is skipped and the paste is assumed to have worked (the
prior behavior). The whole check is bounded to well under half a second.

## Modes & per-app behavior

Four pipeline modes, resolved per-app via `profiles.toml`:

| Mode | What happens |
|---|---|
| `raw` | Parakeet's output, dictionary-corrected only — no LLM, no code transform |
| `clean` (default) | Local Qwen3 LLM removes filler words/false starts/repeats and fixes grammar/punctuation, preserving wording and meaning |
| `polish` | Local Qwen3 LLM restructures the dictation into clear, well-formatted writing for the target app/tone — bigger rewrite than `clean` |
| `code` | Deterministic, no-LLM transform of spoken code syntax into real punctuation/casing (see [Code mode](#code-mode-spoken-form-reference) below) |

`clean`/`polish` are deadline-bound: if the LLM hasn't produced output within
a computed deadline, the raw (dictionary-corrected) transcript is pasted
instead — cleanup is never allowed to add unbounded latency to a dictation.
The deadline scales with input length rather than being a flat value:
`cleanup_timeout_ms` (2500ms base) + `cleanup_timeout_per_char_ms` (6ms
default) per character of input, capped at `cleanup_timeout_max_ms` (20000ms
default) so even a very long dictation can't make you wait more than 20s for
the LLM. A one-sentence dictation gets a deadline close to the 2.5s base; a
multi-minute ramble (many hundreds of words) hits the 20s cap and, if the LLM
hasn't finished by then, falls back to the raw transcript rather than
blocking further.

### `profiles.toml`

Persisted at `~/.config/vzt-flow/profiles.toml`, seeded on first run with:

```toml
[default]
mode = "clean"
tone = "neutral"

["com.apple.Terminal"]
mode = "code"
tone = "neutral"

["com.googlecode.iterm2"]
mode = "code"
tone = "neutral"

["dev.warp.Warp"]
mode = "code"
tone = "neutral"

["com.apple.mail"]
mode = "clean"
tone = "formal"

["com.tinyspeck.slackmacgap"]
mode = "clean"
tone = "casual"
```

Keys are macOS bundle identifiers (case-insensitive), optionally ending in
`*` for a prefix match (e.g. `"com.example.*"`). `tone` is free-form text
passed through to the `polish`/`clean` prompt as a hint (`neutral`/`formal`/
`casual` are just the seeded values, not an enforced enum). Edit the file
directly — the Settings window shows its path (read-only) but doesn't
provide an editor UI for it. An app with no matching rule falls back to
`[default]`.

### Code mode: spoken-form reference

Available in any app once its profile resolves to `mode = "code"` (or by
dictating with `--mode code` from the CLI). Pulled directly from
`crates/flow-core/src/codemode.rs`:

**Case keywords** — consume every following word up to a symbol word, a
language keyword, ASR-attached punctuation, or end of input, then join per
the requested casing:

| Spoken | Casing | Example |
|---|---|---|
| `camel case ...` | camelCase | `camel case user id` → `userId` |
| `snake case ...` | snake_case | `snake case api key` → `api_key` |
| `pascal case ...` | PascalCase | `pascal case flow core` → `FlowCore` |
| `kebab case ...` | kebab-case | `kebab case my app` → `my-app` |

**Symbol words** — two-word phrases are checked before one-word ones (so
`fat arrow` wins over bare `arrow`, `double equals` over `equals`):

| Spoken | Symbol |
|---|---|
| `open paren` / `close paren` | `(` / `)` |
| `open brace` / `close brace` | `{` / `}` |
| `open bracket` / `close bracket` | `[` / `]` |
| `at sign` | `@` |
| `dollar sign` | `$` |
| `double equals` | `==` |
| `fat arrow` | `=>` |
| `equals` | `=` |
| `arrow` | `->` |
| `pipe` | `\|` |
| `ampersand` | `&` |
| `backtick` | `` ` `` |
| `underscore` | `_` |
| `dot` | `.` |
| `colon` | `:` |
| `semicolon` | `;` |
| `slash` | `/` |
| `star` | `*` |
| `plus` | `+` |
| `minus` | `-` |
| `percent` | `%` |
| `hash` | `#` |

**Language keywords** (stay literal, single tokens, and act as stop
boundaries for case-groups/bare-word runs): `const`, `let`, `var`, `return`,
`await`, `async`, `function`, `new`, `throw`, `typeof`, `instanceof`, `if`,
`else`, `for`, `while`, `do`, `switch`, `case`, `break`, `continue`, `class`,
`extends`, `import`, `export`, `from`, `default`, `static`, `public`,
`private`, `protected`, `true`, `false`, `null`, `undefined`, `this`,
`super`, `yield`, `in`, `of`, `delete`, `void`, `try`, `catch`, `finally`.

**Implicit call-name merge** — two or more consecutive bare words directly
before an opening symbol (`(`/`{`/`[`) are automatically camelCased as an
identifier, without needing an explicit `camel case`:

```
get user open paren close paren        → getUser()
const camel case user profile equals await get user open paren close paren
                                        → const userProfile = await getUser()
console dot log open paren close paren → console.log()
```

No space is inserted around `(`, `)`, or `.` (call/member-access syntax);
everything else gets single-space separation. The whole input is lowercased
up front and trailing sentence-punctuation ASR adds is stripped, so a
naturally-capitalized, period-ended utterance like `"Get user open paren
close paren."` still produces `getUser()`.

## Dictionary & snippets

### Dictionary (`dictionary.json`)

Persisted at `~/.config/vzt-flow/dictionary.json`, applied to every
transcript **before** cleanup/code-mode ever sees it. Each entry is
`{"term": "...", "hints": [...]}`; `hints` are known ASR mishearings, and the
canonical `term` itself is always an implicit (case-fixing) candidate too.
Real seed entries:

```json
[
  { "term": "Supabase", "hints": ["superbase", "super base"] },
  { "term": "Whop", "hints": ["whopp", "wop", "wap"] },
  { "term": "VZT", "hints": [] },
  { "term": "Vercel", "hints": ["versel", "verscel"] },
  { "term": "Tauri", "hints": ["tory", "torii"] },
  { "term": "TypeScript", "hints": ["type script"] },
  { "term": "VZT Flow", "hints": ["wispr flow", "whisper flow"] }
]
```
(the full seed list also includes Resend, Parakeet, TradeScriptAI, FlagPlay,
NextPlay, Anthropic, Claude, Stripe, Expo, Postgres, Next.js, Whisper — see
`crates/flow-core/src/dictionary.rs::seed_dictionary`).

Matching is word-boundary-aware, case-insensitive, and fuzzy (Levenshtein
distance budget of `len/4` per word, minimum 1) — but only for terms **4+
characters**; shorter terms (like `VZT`) require an exact match so they don't
fuzzy-fire on unrelated short words. Add your own entries by hand-editing the
JSON file (no in-app editor yet); restart the app (or the next standalone CLI
invocation) to pick up changes.

### Snippets (`snippets.json`)

Persisted at `~/.config/vzt-flow/snippets.json` as `{"trigger": "expansion"}`.
Applied **after** cleanup, and only if the **entire** (cleaned, normalized)
transcript matches a trigger — a trigger phrase appearing mid-sentence is
left alone as ordinary dictation.

Seed:
```json
{ "my email": "vonzelle@vzttechconsulting.com" }
```

Two ways to fire a snippet by voice:
- Say the bare trigger: *"my email"* → expands.
- Say `"insert <trigger>"`: *"insert my email"* → expands.

Matching is case- and punctuation-insensitive (`"My Email!"` and
`"Insert, My Email."` both match). Hand-edit the JSON file to add your own.

## Config reference (`config.toml`)

Persisted at `~/.config/vzt-flow/config.toml`. Every field, with its default
(from `crates/flow-core/src/config.rs::Config::default`):

| Field | Default | Meaning |
|---|---|---|
| `hotkey_keycode` | `61` (Right Option) | macOS virtual keycode of the hold-to-talk key |
| `hotkey_label` | `"Right Option"` | Human-readable label shown in Settings |
| `hold_threshold_ms` | `300` | Minimum hold duration (ms) before a press counts as "hold" rather than a tap that toggles hands-free |
| `idle_unload_secs` | `300` | Seconds of transcriber/cleanup-model inactivity before it's unloaded from memory |
| `max_hold_secs` | `600` | Hard cap (seconds) on a single hold-to-talk recording (10min, for long-form dictation) |
| `max_handsfree_secs` | `600` | Hard cap (seconds) on a single hands-free recording |
| `launch_at_login` | `false` | Mirrors `tauri-plugin-autostart` state |
| `cleanup_timeout_ms` | `2500` | Base component (ms) of the LLM cleanup deadline; see `cleanup_timeout_per_char_ms` |
| `cleanup_timeout_per_char_ms` | `6` | Additional cleanup deadline (ms) granted per character of input, so long dictations get a longer window |
| `cleanup_timeout_max_ms` | `20000` | Absolute ceiling (ms) on the cleanup deadline regardless of input length; on miss, the raw transcript is used instead |
| `handsfree_silence_secs` | `2.5` | Seconds of continuous sub-threshold audio (after at least one loud frame) before hands-free auto-stops |
| `cleanup_enabled` | `true` | Set `false` to force every profile to behave as `raw` mode without editing `profiles.toml` — skips ever loading the cleanup model, for low-RAM machines. Wired into `flow-cli`'s standalone pipeline; the desktop daemon path doesn't check it yet. |
| `meeting_auto` | `"ask"` | Meeting auto-detect behavior: `"ask"` (notify on a detected call), `"auto"` (start transcribing immediately), or `"off"` (disable). Also settable from the tray's **Meeting auto-detect** submenu. See [`docs/MEETINGS.md`](./MEETINGS.md). |
| `rolling_transcription` | `true` | Transcribe silence-completed chunks *during* a long recording so only the final <35s tail is transcribed at release (end-latency ~25s → <1s on a 7min clip). Set `false` to fall back to transcribing everything at release. Recordings under ~35s are unaffected either way. |

The file is created with these defaults on first run if it doesn't exist.
Most fields (notably `hotkey_keycode` and `hold_threshold_ms`) take effect
live when changed from Settings; `idle_unload_secs` requires an app restart
to apply, since it's read once when the model-manager thread spawns.

## CLI reference

```
flow listen [--mode raw|clean|polish|code] [--max-secs N]
    # Record + transcribe (+ optionally clean/code-transform) + print to stdout.
    # Daemon-first: if the desktop app's daemon socket is reachable, records
    # through it (driving its overlay); otherwise falls back to a fully
    # standalone capture/transcribe/cleanup pipeline that waits for Enter to
    # stop recording (or --max-secs, if given).

flow transcribe <file> [--mode raw|clean|polish|code]
    # Transcribe an existing audio file (.wav, or anything ffmpeg can read).
    # --mode is optional; with none given, only dictionary correction runs
    # (no code-mode/cleanup pass). (--clean is accepted as an alias for --mode.)

flow models download [parakeet-v3|cleanup] [--force]
    # Download a model (defaults to parakeet-v3 if no argument given).
    # --force re-downloads even if already present.
    # (`Models` currently has only this one subcommand — there is no
    # `flow models status`; use `flow doctor` for model presence checks.)

flow doctor
    # Environment/model/device diagnostics: flow-cli version, rustc version,
    # model root dir + Parakeet/cleanup model presence, default input
    # device, ffmpeg presence, daemon socket state (+ version/state if
    # alive), and whether `claude mcp list` shows vzt-flow registered.

flow status
    # Query the running daemon: state (idle/recording/transcribing), whether
    # the Parakeet/cleanup models are loaded, daemon version. Prints
    # "daemon: not running" if unreachable.

flow toggle
    # Start/stop a hands-free recording on the running daemon — same effect
    # as the tray's "Start/Stop dictation" item. Requires a running daemon.

flow cancel
    # Cancel the daemon's in-progress recording, if any. Requires a running
    # daemon.

flow history [-n 20]
    # Recent dictations: timestamp, duration, mode, frontmost app, and the
    # pasted text. Reads from the daemon if reachable, else directly from
    # ~/.config/vzt-flow/history.jsonl.
```

Hidden diagnostic commands (not shown in `--help`, but real and useful):

```
flow paste-test "<text>"          # exercises save/set/[Cmd+V]/restore in isolation
flow clean-test "<text>" [--mode clean|polish] [--timeout-ms <ms>]
                                   # runs the LLM cleanup pass standalone, reporting
                                   # model-load time, warm-up time, and which path won
                                   # (llm vs. deadline/raw fallback). --timeout-ms
                                   # defaults to the same length-scaled deadline formula
                                   # the app uses (base + per-char, capped) rather than
                                   # a flat value; pass it explicitly to force a specific
                                   # deadline, e.g. a tiny one to test the raw fallback.
flow code-test "<text>"           # runs the deterministic code-mode transform, no mic/model
```

Only the final transcript goes to `flow listen`'s stdout — every diagnostic
line (recording status, model load timing, realtime factor) goes to stderr —
so `flow listen | pbcopy` gives you a clean clipboard copy with no noise
mixed in:

```bash
flow listen --mode clean | pbcopy
flow listen --mode code --max-secs 30
```

## MCP (Claude Code voice input)

```bash
cd mcp
npm install
npm run build
claude mcp add vzt-flow --scope user -- node "$(pwd)/dist/index.js"
```

This registers three tools (`mcp/src/index.ts`), backed by the daemon socket
when the desktop app is running, falling back to the standalone `flow` CLI
otherwise (set `FLOW_BIN` if the binary isn't discoverable — the fallback
resolver checks `FLOW_BIN`, then `~/vzt-flow/target/release/flow`, then bare
`flow` on PATH):

| Tool | Args | Behavior |
|---|---|---|
| `listen` | `mode` (raw/clean/polish/code, default `clean`), `max_seconds` (default `120`, up to `600`) | Records from the mic and returns the transcribed, cleaned text |
| `transcribe_file` | `path` (absolute path) | Transcribes an existing audio file through dictionary correction |
| `dictation_history` | `n` (default `10`) | Returns recent dictation history entries |

**Using `listen` from Claude Code**: once registered, ask Claude Code to
"listen for my voice input" (or similar) and it can invoke the `listen` tool
directly — no need to leave the terminal to dictate a prompt.

To re-register after moving the repo or rebuilding the MCP server:

```bash
claude mcp remove vzt-flow
cd mcp && npm run build
claude mcp add vzt-flow --scope user -- node "$(pwd)/dist/index.js"
```

Check registration status any time with `flow doctor` (it shells out to
`claude mcp list` and checks for `vzt-flow`) or `claude mcp list` directly.

## Troubleshooting

**Hotkey does nothing (no overlay, nothing pasted):**
- Almost always a permissions issue — see the
  [rebuild-drops-permissions gotcha](#the-rebuild-drops-permissions-gotcha)
  above. This is *especially* likely right after a fresh build.
- Check the app's console output (see [Checking logs](#checking-logs) below)
  for `hotkey monitor failed to install a CGEventTap` — that confirms Input
  Monitoring isn't granted (or isn't recognized for the current binary
  signature).
- The tray's "Start/Stop dictation" item bypasses the hotkey monitor
  entirely — use it to confirm the rest of the pipeline (mic, models, paste)
  works while you sort out the permission.
- Sleep/wake is **already handled automatically**: the `CGEventTap` watchdog
  re-arms itself both from the tap's own `TapDisabledByTimeout`/
  `TapDisabledByUserInput` callbacks and a belt-and-braces 5-second poll, so
  a Mac waking from sleep does not require restarting the app.

**First dictation after launch is slow:**
- The Parakeet model isn't loaded until the first recording finishes (lazy
  load, then idle-unloaded again after `idle_unload_secs` of inactivity) —
  expect a few seconds of one-time load latency on that first dictation only
  (subsequent ones reuse the loaded model). The exact time is logged (see
  below).
- If you're using `clean`/`polish` mode, the cleanup LLM is pre-warmed (model
  load + a throwaway generation to force Metal kernel-pipeline JIT
  compilation) as soon as a recording *starts*, in parallel with you talking
  — so by the time you finish speaking it's typically already warm. The very
  first recording of a session still pays that cost, though, since warm-up
  only has as long as your speech to finish before the deadline-bound real
  cleanup call begins.

**A term keeps coming out wrong (e.g. "Superbase" instead of "Supabase"):**
- Add it to `~/.config/vzt-flow/dictionary.json`: `{"term": "Supabase",
  "hints": ["superbase", "super base"]}`. Terms under 4 characters need an
  exact-match hint (fuzzy correction is disabled for them to avoid false
  positives); longer terms get fuzzy (edit-distance) matching for free even
  with no hints, which also fixes casing-only mistakes.
- `clean`/`polish` mode also receives the full dictionary term list as a
  spelling hint in the LLM's system prompt, so once a term is in the
  dictionary it's less likely to get "corrected" back to a mishearing during
  the cleanup pass too.

#### Checking logs

- The desktop app is normally launched via Finder/Dock/menu bar with no
  visible console. Launch it from a terminal instead to see its stderr
  output live:
  ```bash
  /Applications/VZT\ Flow.app/Contents/MacOS/vzt-flow
  # or, for a source build:
  ./target/release/bundle/macos/VZT\ Flow.app/Contents/MacOS/vzt-flow
  ```
  This surfaces model load times, hotkey monitor status, daemon socket bind
  status, cleanup fallback reasons, and every `[vzt-flow] ...` diagnostic
  line the app prints.

**Daemon socket looks stale / `flow status` says "not running" even though
the app is open:**
- Run `flow doctor` — it reports one of "not present", "PRESENT and alive",
  or "STALE file present, nothing listening" for
  `~/.config/vzt-flow/daemon.sock`.
- A stale socket file (left behind by a crash or `kill -9`) is automatically
  cleared the next time the app starts and binds successfully — you don't
  need to delete it by hand. If the app is running but the socket looks
  stale, check the app's console output for `daemon control socket failed to
  start` (usually means another instance is already bound to it — quit any
  duplicate instances and relaunch).
