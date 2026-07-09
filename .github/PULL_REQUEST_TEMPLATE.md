<!--
Security fixes: do not open a public PR. See SECURITY.md.
-->

## What this changes

<!-- One or two sentences. Link the issue it closes, if there is one. -->

## Why

<!-- What was broken or missing. If this is a performance or memory change,
     put the before/after numbers here, measured — not estimated. -->

## How it was verified

<!-- Delete what doesn't apply. Tests alone are not sufficient for anything
     touching audio, ASR, or paste. -->

- [ ] `cargo test --release --workspace` passes
- [ ] `npm run build --prefix mcp` passes (if `mcp/` changed)
- [ ] `flow doctor` is clean (if the engine, CLI, or daemon changed)
- [ ] Exercised with real speech, not silence or noise:
      `say -o /tmp/clip.aiff "test sentence" && ffmpeg -y -i /tmp/clip.aiff /tmp/clip.wav && flow transcribe /tmp/clip.wav`
- [ ] Overlay states checked via the tray's "Test overlay" item (if UI changed)

**Measurements** (RTF = wall time ÷ audio duration; memory via `ps -o rss`):

<!-- e.g. RTF 0.11 → 0.09; peak RSS 1.4 GB → 1.4 GB. Write "n/a" if this
     change cannot affect either. -->

## Checklist

- [ ] Commit messages use a `type: subject` prefix (`fix:`, `feat:`, `docs:`, `CI:`)
- [ ] Only intentionally-changed files are staged — no `git add -A`
- [ ] Docs updated if behavior changed (README, `docs/USAGE-*.md`, `CHANGELOG.md`)
- [ ] No new runtime network access (the one-time model download is the only
      network traffic this project makes)
