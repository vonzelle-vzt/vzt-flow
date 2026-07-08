# VZT Flow

A local, private, $0 voice-dictation replacement for [Wispr Flow](https://wisprflow.ai). Hold a hotkey, talk, and the transcript is pasted wherever your cursor is — entirely on-device. No audio or screenshots ever leave your machine, no subscription, no word limits.

Hold-to-talk (or tap for hands-free) → local ASR (Parakeet TDT) → optional local LLM cleanup (Qwen3) → paste. Also works headless as a CLI and as an MCP voice-input tool for [Claude Code](https://claude.com/claude-code).

## VZT Flow vs. Wispr Flow

| | VZT Flow | Wispr Flow |
|---|---|---|
| Cost | $0, forever | Subscription |
| Where transcription happens | 100% on-device (ONNX/CPU or Metal) | Cloud |
| Audio/screenshots leave your machine | Never | Sent to their servers |
| Word/usage limits | None | Plan-dependent |
| Per-app behavior | Bundle-id profiles (`profiles.toml`), editable | Limited |
| Code mode (identifiers, symbols, no LLM rewrite) | Yes, deterministic | No |
| Custom dictionary (names, jargon, spellings) | Yes, local file | Yes |
| Text snippets/expansion | Yes, local file | Limited |
| CLI | Yes (`flow listen`, `flow transcribe`, `flow doctor`, ...) | No |
| Scriptable / MCP voice input for agents | Yes (`listen`, `transcribe_file`, `dictation_history` tools) | No |
| Source | Open, MIT | Closed |

## How it works

```
 hold-to-talk hotkey (Right Option, or tap for hands-free)
        │
        ▼
   audio capture (cpal) ──► Parakeet TDT v3 (ONNX, int8) ──► raw transcript
                                                                   │
                              dictionary correction (local)  ◄────┘
                                                                   │
                per-app profile: raw | clean | polish | code ◄────┘
                       (code = deterministic; clean/polish = local
                        Qwen3-1.7B via llama.cpp, deadline-bound)
                                                                   │
                          snippet expansion (local)          ◄────┘
                                                                   │
                     clipboard save → set → simulate paste ◄──────┘
                     → restore clipboard after a short delay
```

- **`crates/flow-core`** — the engine: audio capture, ASR, LLM cleanup, dictionary, code mode, snippets, profiles, history, hotkey monitoring, paste, model download/management, and the daemon IPC protocol. Platform-agnostic; macOS-only pieces are `#[cfg(target_os = "macos")]`-gated.
- **`crates/flow-cli`** — the `flow` binary. Talks to a running daemon first, falls back to a fully standalone (no daemon) capture/transcribe/cleanup pipeline.
- **`apps/desktop`** — the Tauri 2 menu-bar app: tray icon, recording overlay, Settings window, the hold-to-talk hotkey, and the daemon control socket that the CLI/MCP server drive.
- **`mcp/`** — a small Node/TypeScript MCP server (`listen`, `transcribe_file`, `dictation_history` tools) so Claude Code can take voice input, going through the daemon socket when the desktop app is running, or the standalone CLI otherwise.

## Install (build from source)

Models are **not** bundled — they download on first run (or via `flow models download`).

### Prerequisites
- Rust (stable) — `rustup default stable`
- Node 24+
- macOS: Xcode command line tools (for the `ApplicationServices`/`AppKit` frameworks and Metal)
- `ffmpeg` on PATH (optional — only needed for `flow transcribe` on non-wav files)

### Build

```bash
git clone https://github.com/<you>/vzt-flow.git
cd vzt-flow

# CLI
cargo build --release -p flow-cli
./target/release/flow doctor        # sanity check + points out anything missing
./target/release/flow models download parakeet-v3
./target/release/flow models download cleanup   # optional, needed for clean/polish modes

# Desktop app (macOS)
cd apps/desktop
npm install
cargo install tauri-cli --version "^2"
cargo tauri build          # unsigned/local build; fine for personal use
open src-tauri/target/release/bundle/dmg/*.dmg
```

### macOS permissions

The desktop app needs three grants, all in **System Settings → Privacy & Security**:

1. **Input Monitoring** — lets it detect the hold-to-talk key (a `CGEventTap`). Without this the tray's manual Start/Stop item still works.
2. **Accessibility** — lets it simulate Cmd+V to paste. Without it, the transcript is left on the clipboard for you to paste manually.
3. **Microphone** — prompted automatically the first time it opens an audio input stream.

## Hotkey defaults

- **Hold Right Option** to record — release to transcribe and paste.
- **Tap Right Option** (press+release faster than the hold threshold) to start a **hands-free** recording — it auto-stops on silence, or tap again to stop manually.
- **Esc** cancels an in-progress recording (discarded, nothing pasted).
- Rebind from the Settings window; per-app behavior (mode/tone) lives in `profiles.toml`.

## CLI reference

```
flow listen [--mode raw|clean|polish|code] [--max-secs N]   # record + transcribe + paste to stdout
flow transcribe <file> [--mode ...]                          # transcribe an existing audio file
flow models download <parakeet-v3|cleanup> [--force]
flow models status
flow doctor                                                   # environment/model/permission diagnostics
flow status                                                   # query the running daemon
flow toggle                                                   # start/stop a hands-free recording on the daemon
flow cancel                                                   # cancel the daemon's in-progress recording
flow history [-n 20]                                          # recent dictations
```

`flow listen`/`flow transcribe` are daemon-first (routes through the running desktop app, driving its overlay) with a fully standalone fallback when no daemon is running.

## MCP setup (Claude Code voice input)

```bash
cd mcp
npm install
npm run build
claude mcp add vzt-flow --scope user -- node "$(pwd)/dist/index.js"
```

This registers three tools: `listen` (record + return cleaned transcript), `transcribe_file`, and `dictation_history`. They go through the daemon socket if the desktop app is running, and fall back to the standalone `flow` CLI otherwise (set `FLOW_BIN` if it isn't on PATH).

## Config files

All under `~/.config/vzt-flow/` (macOS) — see [Windows status](#windows-status) for where this lives there.

| File | Purpose |
|---|---|
| `config.toml` | Hotkey binding, hold threshold, recording caps, idle-unload timers, cleanup deadline. |
| `dictionary.json` | Custom terms (names, jargon, spellings) applied via fuzzy correction before cleanup. |
| `profiles.toml` | Per-app (bundle-id, glob-matchable) `{mode, tone}` rules — e.g. Terminal/iTerm/Warp → `code`, Mail → `clean`+formal. |
| `snippets.json` | Text-expansion shortcuts applied after cleanup. |
| `models/` | Downloaded Parakeet + cleanup model files. |
| `history.jsonl` | Recent dictation log (`flow history` / `dictation_history` MCP tool). |
| `daemon.sock` | Unix control socket the desktop app listens on (macOS/Linux only — see below). |

## Windows status

The workspace **compiles and is built by CI** on `windows-latest` (msi/nsis installer artifacts), but this is **experimental** — untested on real Windows hardware, since development happens on macOS. Known gaps, all deliberate v1 scope cuts (not oversights):

- **No daemon control socket yet.** The IPC transport is Unix-domain-socket-only; on Windows every daemon-dependent path (`flow status`/`toggle`/`cancel`/`listen`-via-daemon, and the MCP server's daemon path) reports "not supported" and falls back to the standalone CLI pipeline instead. A named-pipe transport can slot into `flow_core::ipc`'s existing framing/transport split later.
- **Hotkey binding differs.** macOS defaults to holding Right Option (a bare modifier, via a `CGEventTap`); Windows uses `tauri-plugin-global-shortcut`, which doesn't support modifier-only bindings there, so the Windows default is **Ctrl+Shift+Space**.
- **No Escape-to-cancel on Windows yet.** A globally *registered* Escape shortcut would swallow Escape system-wide, unlike macOS's `ListenOnly` tap. Use the tray's Start/Stop item to end a recording early.
- **No TCC-equivalent permission gate.** Windows has no Accessibility-style one-time grant, so paste is never skipped for that reason; a lower-privileged process's paste into an elevated window (UIPI) can still silently fail — the transcript is always left on the clipboard first as a fallback either way.
- **Config path** is `%APPDATA%\vzt-flow` on Windows (via `dirs::config_dir()`), vs. the literal `~/.config/vzt-flow` on macOS.
- Parakeet/cleanup inference is CPU-only ONNX/llama.cpp on Windows (no CoreML/Metal execution provider).

## Architecture

```
crates/
  flow-core/    engine: audio, ASR, cleanup LLM, dictionary, code mode,
                snippets, profiles, history, hotkey, paste, model mgmt, IPC
  flow-cli/     `flow` binary — daemon-first, standalone fallback
apps/
  desktop/      Tauri 2 menu-bar app (tray, overlay, settings, daemon socket)
mcp/            Node/TS MCP server exposing listen/transcribe_file/dictation_history
```

## License

MIT — see [LICENSE](LICENSE).

## Credits

- [transcribe-rs](https://github.com/cjpais/transcribe-rs) / [Handy](https://github.com/cjpais/Handy) — the ONNX transcription plumbing and pre-packaged Parakeet int8 model archive this project uses.
- [NVIDIA Parakeet TDT](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) — the local ASR model.
- [Qwen3](https://huggingface.co/Qwen) (via [unsloth](https://huggingface.co/unsloth)'s GGUF re-quantization) — the local cleanup/rewrite LLM.
- [llama.cpp](https://github.com/ggml-org/llama.cpp) / [llama-cpp-2](https://github.com/utilityai/llama-cpp-rs) — local LLM inference.
