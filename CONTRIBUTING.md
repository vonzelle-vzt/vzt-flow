# Contributing to VZT Flow

Issues and PRs are welcome. The codebase is small enough to read end to
end — start with `crates/flow-core/src/lib.rs` and the architecture section
of the [README](README.md#architecture).

The one norm that matters most here: **verify claims against the code and
against real measurements.** The numbers in the README and PRD were all
measured, not estimated. Anything you add should meet that standard.

## Prerequisites

- **Rust** (stable, edition 2021). There is no `rust-toolchain.toml`, so
  whatever stable you have should work. On this project's dev Macs, Rust is
  not on `PATH` by default — `source ~/.cargo/env` first.
- **Node.js** — needed for the MCP server (`mcp/`) and the Tauri frontend
  (`apps/desktop/`).
- **Tauri CLI v2** — only if you're building the desktop app:
  `cargo install tauri-cli --version "^2" --locked`.

## Layout

| Path | What lives there |
|---|---|
| `crates/flow-core/` | Engine: audio capture, ASR, LLM cleanup, dictionary, code mode, snippets, profiles, history, hotkey, paste, model management, daemon IPC |
| `crates/flow-cli/` | The `flow` binary — daemon-first, standalone fallback |
| `apps/desktop/` | Tauri 2 menu-bar app: tray, overlay, Settings, control socket |
| `mcp/` | Node/TypeScript MCP server for Claude Code |
| `docs/` | PRD, per-platform usage guides, meeting-mode docs |

## Build and test

```bash
source ~/.cargo/env                    # Rust may not be on PATH

cargo build --release -p flow-cli
cargo test --release --workspace
./target/release/flow doctor           # env/model/daemon sanity check

# Desktop app (unsigned local build)
cd apps/desktop && npm install && cargo tauri build
# The bundle lands at workspace-root target/, not src-tauri/target/

# MCP server
cd mcp && npm install && npm run build
```

CI (`.github/workflows/build.yml`) runs `cargo test --release --workspace`
and a Tauri build on macOS, a `tsc --noEmit` typecheck of `mcp/`, and
Linux + Windows build jobs. It skips entirely for changes confined to
`docs/**`, `**.md`, and `scripts/**`.

## Verifying a change

Tests alone are not sufficient for anything touching audio, ASR, or paste.

- **Test with real speech, not silence or noise.** Generate a clip and run
  it through the real path:
  ```bash
  say -o /tmp/clip.aiff "your test sentence"
  ffmpeg -y -i /tmp/clip.aiff /tmp/clip.wav
  ./target/release/flow transcribe /tmp/clip.wav
  ```
- **Report real numbers, not adjectives.** Real-time factor (wall time ÷
  audio duration) and `ps -o rss` memory, not "should be fast."
- **QA overlay states via the tray's "Test overlay" item**, which cycles
  Recording → Transcribing → Done with no mic or model involved.

## Two gotchas that will waste your afternoon

**Unsigned rebuilds silently drop macOS permission grants.** Every unsigned
`cargo tauri build` mints a new code signature, and macOS revokes the Input
Monitoring / Accessibility grants tied to the old one. There is no error
dialog — the hotkey just stops working. After a rebuild, relaunch via
`nohup <path-to-built-binary> &` rather than `open`, since `open` re-resolves
to whatever is in `/Applications` and hides which binary you're testing. The
full remove/re-add fix is in
[docs/USAGE-macOS.md](docs/USAGE-macOS.md#the-rebuild-drops-permissions-gotcha).

**Parakeet memory is quadratic in audio length.** The underlying
`transcribe-rs` Parakeet path does no internal chunking. Measured on an M5:
~15 GB peak for 49 s of audio, ~37 GB for 93 s, OOM kill around 146 s. Never
call `.transcribe()` on more than ~60 s directly — route long audio through
`crates/flow-core/src/chunking.rs`. Release latency on long clips is handled
separately by `crates/flow-core/src/rolling.rs`. Both problems are already
solved; extend those paths rather than reinventing them.

## Pull requests

Before opening one:

- `cargo test --release --workspace` passes.
- `npm run build --prefix mcp` passes if you touched `mcp/`.
- `./target/release/flow doctor` is clean if you touched the engine, the
  CLI, or the daemon.
- Any performance or memory claim in the PR body is backed by a measurement.

Commit messages follow a short `type: subject` prefix — `docs:`, `CI:`,
`fix:`, `feat:` — matching the existing history. Keep the subject in the
imperative mood.

**Stage by explicit pathspec.** Do not use `git add -A` or `git add .`;
this repo is frequently worked in a shared checkout, and a broad `git add`
will sweep unrelated in-flight files into your commit. `git pull --rebase`
before pushing, and never force-push a shared branch.

## Reporting bugs

Use the [issue templates](https://github.com/vonzelle-vzt/vzt-flow/issues/new/choose).
Include your `flow --version`, your OS and architecture, and `flow doctor`
output — those three answer most of the first-round questions.

**Security problems do not go in public issues.** See
[SECURITY.md](SECURITY.md) for private disclosure.

## Code of Conduct

Participation is governed by the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE), the same terms that cover the project.
