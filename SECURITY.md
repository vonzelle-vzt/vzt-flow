# Security Policy

## Supported versions

Only the latest released version receives security fixes. VZT Flow is
pre-1.0 and ships from `main`; there are no maintained backport branches.

| Version | Supported |
|---|---|
| 0.2.x | Yes |
| 0.1.x | No — upgrade to 0.2.x |

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub: go to the
[Security tab](https://github.com/vonzelle-vzt/vzt-flow/security/advisories/new)
and choose **Report a vulnerability**. That opens a private advisory visible
only to you and the maintainers.

Include what you'd need if you were on the receiving end: the version
(`flow --version`), your OS and architecture, what you did, what happened,
and what you expected instead. A proof of concept helps but isn't required
to file.

This is a small project, so response is best-effort rather than
contractual. Expect an initial acknowledgement within about a week. If a
report turns out to be valid, you'll be credited in the advisory and the
release notes unless you'd rather not be.

## Threat model

VZT Flow is a local, on-device tool. It holds **Microphone**,
**Accessibility**, and **Input Monitoring** permissions, it can read the
focused text field and paste into it, and it can leave a transcript on the
clipboard when a paste fails. It also exposes a local control socket and an
MCP server. That surface is what makes the interesting bugs interesting.

Things worth reporting:

- Audio, transcripts, or screenshots leaving the machine — the only network
  traffic VZT Flow should ever make is the one-time model download from
  Hugging Face. Anything else is a bug in the privacy guarantee, and is the
  single most serious class of report for this project.
- Privilege escalation, or abuse of the Accessibility/Input Monitoring
  grant to act outside dictation (reading fields the user didn't focus,
  synthesizing input the user didn't dictate).
- Any local user or process on the machine driving the control socket or
  the MCP server to capture audio, read dictation history, or paste text
  without the user initiating it.
- Model files being loaded without integrity verification, or a download
  path that a network attacker can redirect to attacker-controlled weights.
- Transcript or history data written somewhere world-readable.

Generally out of scope:

- Attacks that require an already-compromised machine, root, or physical
  access to an unlocked session.
- The fact that the first run downloads models over the network. That is
  documented, expected, and the only network access by design.
- Transcription accuracy, or the cleanup LLM producing wrong text. Those
  are quality bugs — open a normal issue.
- Findings from automated scanners with no demonstrated impact.
