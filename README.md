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

```
crates/
  flow-core/    engine: audio, ASR, cleanup LLM, dictionary, code mode,
                snippets, profiles, history, hotkey, paste, model mgmt, IPC
  flow-cli/     `flow` binary — daemon-first, standalone fallback
apps/
  desktop/      Tauri 2 menu-bar app (tray, overlay, settings, daemon socket)
mcp/            Node/TS MCP server exposing listen/transcribe_file/dictation_history
```

## Quickstart

Full, verified-against-the-code guides live in `docs/`:

- **[docs/USAGE-macOS.md](docs/USAGE-macOS.md)** — the primary, actively
  developed platform. Install (source or CI `.dmg`), model downloads,
  permissions (**read this if the hotkey stops working after a rebuild** —
  there's a known macOS code-signing gotcha), daily use, per-app modes,
  code-mode reference, dictionary/snippets, full config/CLI/MCP reference,
  and troubleshooting.
- **[docs/USAGE-Windows.md](docs/USAGE-Windows.md)** — experimental. Compiles
  and is CI-built, but has never been run on real Windows hardware. Covers
  the Ctrl+Shift+Space binding, and every verified difference from macOS
  (no daemon socket yet, no per-app profiles, no `clean`/`polish` cleanup
  LLM, Ctrl+V paste with no secure-field detection, `%APPDATA%\vzt-flow`
  config path).

### macOS, in short

```bash
git clone https://github.com/vonzelle-vzt/vzt-flow.git
cd vzt-flow

cargo build --release -p flow-cli
./target/release/flow doctor
./target/release/flow models download parakeet-v3
./target/release/flow models download cleanup   # optional, for clean/polish modes

cd apps/desktop
npm install
cargo install tauri-cli --version "^2"
cargo tauri build
open ../../target/release/bundle/dmg/*.dmg
```

Then grant **Microphone**, **Accessibility**, and **Input Monitoring** in
System Settings → Privacy & Security — see
[docs/USAGE-macOS.md#permissions](docs/USAGE-macOS.md#permissions) for exact
steps and the rebuild-revokes-permissions gotcha.

### Windows, in short (experimental)

```powershell
cargo build --release -p flow-cli
.\target\release\flow.exe models download parakeet-v3
cd apps\desktop
npm install
cargo install tauri-cli --version "^2"
cargo tauri build --target x86_64-pc-windows-msvc
```

Default hotkey is **Ctrl+Shift+Space** (Windows doesn't support the
modifier-only binding macOS uses). See
[docs/USAGE-Windows.md](docs/USAGE-Windows.md) for what does and doesn't work
yet.

### MCP setup (Claude Code voice input), either platform

```bash
cd mcp
npm install
npm run build
claude mcp add vzt-flow --scope user -- node "$(pwd)/dist/index.js"
```

## License

MIT — see [LICENSE](LICENSE).

## Credits

- [transcribe-rs](https://github.com/cjpais/transcribe-rs) / [Handy](https://github.com/cjpais/Handy) — the ONNX transcription plumbing and pre-packaged Parakeet int8 model archive this project uses.
- [NVIDIA Parakeet TDT](https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3) — the local ASR model.
- [Qwen3](https://huggingface.co/Qwen) (via [unsloth](https://huggingface.co/unsloth)'s GGUF re-quantization) — the local cleanup/rewrite LLM.
- [llama.cpp](https://github.com/ggml-org/llama.cpp) / [llama-cpp-2](https://github.com/utilityai/llama-cpp-rs) — local LLM inference.
