#!/usr/bin/env python3
"""Point the Homebrew cask at a new release.

Used by the `homebrew` job in .github/workflows/release.yml, and safe to run
by hand against a checkout of the tap:

    python3 .github/scripts/bump_cask.py path/to/Casks/vzt-flow.rb 0.3.4 <arm-sha> <intel-sha>

The whole point is the assertions. A `sed` that matches nothing still exits 0,
so the obvious version of this step reports success while silently leaving the
cask on the old release — which is the exact bug the job exists to prevent,
just automated and harder to notice. Every substitution here therefore checks
its replacement count and fails loudly on anything other than exactly one.
"""

import pathlib
import re
import sys

SHA256_RE = re.compile(r"\A[a-f0-9]{64}\Z")


def die(msg: str) -> None:
    # ::error:: renders in the Actions log and on the job summary.
    print(f"::error::{msg}", file=sys.stderr)
    raise SystemExit(1)


def sub_exactly_once(pattern: str, replacement: str, text: str, what: str) -> str:
    new, count = re.subn(pattern, replacement, text, flags=re.MULTILINE)
    if count != 1:
        die(
            f"expected exactly 1 {what} line in the cask, matched {count}. "
            f"The cask's formatting probably changed — bump it by hand, then "
            f"fix the pattern in .github/scripts/bump_cask.py so the next "
            f"release is automatic again."
        )
    return new


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        die(f"usage: {argv[0]} <cask.rb> <version> <arm-sha256> <intel-sha256>")
    _, cask_path, version, arm, intel = argv

    for label, sha in (("arm", arm), ("intel", intel)):
        if not SHA256_RE.match(sha):
            die(f"{label} sha256 is not 64 lowercase hex characters: {sha!r}")
    if arm == intel:
        die("arm and intel sha256 are identical — that means the wrong file was hashed twice")

    path = pathlib.Path(cask_path)
    original = path.read_text()

    text = sub_exactly_once(r'^  version "[^"]*"$', f'  version "{version}"', original, "version")
    text = sub_exactly_once(
        r'^(  sha256 arm:   )"[a-f0-9]{64}",$', rf'\g<1>"{arm}",', text, "arm sha256"
    )
    text = sub_exactly_once(
        r'^(         intel: )"[a-f0-9]{64}"$', rf'\g<1>"{intel}"', text, "intel sha256"
    )

    # Belt and braces: prove the values are present, not merely that a
    # substitution ran.
    for needle, what in ((f'version "{version}"', "version"), (arm, "arm sha256"), (intel, "intel sha256")):
        if needle not in text:
            die(f"{what} is missing from the rewritten cask")

    if text == original:
        print(f"cask already at {version} — nothing to change")
        return 0

    path.write_text(text)
    print(f"cask bumped to {version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
