#!/usr/bin/env bash
# Configure Developer ID signing + notarization for the release workflow.
#
# Everything Apple requires you to do by hand (enrol, create the certificate,
# mint an app-specific password) stays by hand. This script does the rest:
# reads the identity and Team ID straight out of your .p12, base64-encodes it,
# and writes the six secrets `.github/workflows/release.yml` reads.
#
#   1. Keychain Access -> My Certificates -> right-click your
#      "Developer ID Application: ..." identity -> Export -> save a .p12.
#   2. appleid.apple.com -> Sign-In and Security -> App-Specific Passwords.
#   3. bash scripts/setup-signing.sh ~/Desktop/developer-id.p12
#
# The .p12 password, your Apple ID and the app-specific password are read
# without echo and are never written to disk or to your shell history.
#
# Until all six secrets exist the release workflow builds ad-hoc and says so.
# The moment they exist it signs, notarizes and staples, and the guards in
# release.yml verify all of that against the artifact users actually download.
set -euo pipefail

REPO="${REPO:-vonzelle-vzt/vzt-flow}"

die() { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }
log() { printf '\033[36m==>\033[0m %s\n' "$1"; }
ok()  { printf '\033[32m ok\033[0m %s\n' "$1"; }

P12="${1:-}"
[ -n "$P12" ] || die "usage: bash scripts/setup-signing.sh <path-to-developer-id.p12>"
[ -f "$P12" ] || die "no such file: $P12"

command -v openssl >/dev/null 2>&1 || die "openssl not found"
command -v gh >/dev/null 2>&1 || die "gh not found — https://cli.github.com"
gh auth status >/dev/null 2>&1 || die "gh is not authenticated — run: gh auth login"

# --- read the certificate out of the .p12 -----------------------------------

printf 'Password for %s: ' "$(basename "$P12")" >&2
read -r -s P12_PASSWORD; printf '\n' >&2
[ -n "$P12_PASSWORD" ] \
  || die "an empty .p12 password will not survive the keychain import in CI — re-export with one"

# A .p12 from modern Keychain Access is encrypted with legacy RC2/3DES, which
# OpenSSL 3 refuses unless asked. Older OpenSSL has no -legacy flag. Try both.
cert_subject() {
  P12_PASSWORD="$P12_PASSWORD" openssl pkcs12 -in "$P12" -nokeys -passin env:P12_PASSWORD -legacy 2>/dev/null \
    | openssl x509 -noout -subject 2>/dev/null && return 0
  P12_PASSWORD="$P12_PASSWORD" openssl pkcs12 -in "$P12" -nokeys -passin env:P12_PASSWORD 2>/dev/null \
    | openssl x509 -noout -subject 2>/dev/null && return 0
  return 1
}

SUBJECT="$(cert_subject || true)"
[ -n "$SUBJECT" ] || die "could not read the certificate — wrong password, or not a .p12"

# subject: ... CN = Developer ID Application: Jane Doe (ABCDE12345), OU = ABCDE12345, ...
IDENTITY="$(printf '%s' "$SUBJECT" | sed -n 's/.*CN *= *\(Developer ID Application: [^,/]*\).*/\1/p' | sed 's/[[:space:]]*$//')"
TEAM_ID="$(printf '%s' "$SUBJECT" | sed -n 's/.*OU *= *\([A-Z0-9]\{10\}\).*/\1/p' | head -n1)"

[ -n "$IDENTITY" ] || die "not a 'Developer ID Application' certificate.
  subject: $SUBJECT
  A 'Developer ID Installer', 'Apple Development' or 'Apple Distribution'
  certificate cannot sign an app for distribution outside the App Store."
[ -n "$TEAM_ID" ] || die "no 10-character Team ID (OU) in the subject: $SUBJECT"

ok "identity: $IDENTITY"
ok "team id:  $TEAM_ID"

# --- the two credentials Apple will not let a script create ------------------

printf 'Apple ID (the email you sign in to developer.apple.com with): ' >&2
read -r APPLE_ID
[ -n "$APPLE_ID" ] || die "Apple ID is required for notarization"

printf 'App-specific password (appleid.apple.com — abcd-efgh-ijkl-mnop): ' >&2
read -r -s APPLE_PASSWORD; printf '\n' >&2
case "$APPLE_PASSWORD" in
  ????-????-????-????) ;;
  "") die "app-specific password is required for notarization" ;;
  *)  die "that is not an app-specific password (expected abcd-efgh-ijkl-mnop).
  Your normal Apple ID password will not work — notarytool rejects it." ;;
esac

# --- write the secrets -------------------------------------------------------

log "writing 6 secrets to $REPO"
base64 < "$P12" | tr -d '\n' | gh secret set APPLE_CERTIFICATE --repo "$REPO"
gh secret set APPLE_CERTIFICATE_PASSWORD --repo "$REPO" --body "$P12_PASSWORD"
gh secret set APPLE_SIGNING_IDENTITY     --repo "$REPO" --body "$IDENTITY"
gh secret set APPLE_ID                   --repo "$REPO" --body "$APPLE_ID"
gh secret set APPLE_PASSWORD             --repo "$REPO" --body "$APPLE_PASSWORD"
gh secret set APPLE_TEAM_ID              --repo "$REPO" --body "$TEAM_ID"

log "verifying"
missing=0
for s in APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY \
         APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID; do
  if gh secret list --repo "$REPO" | grep -q "^${s}[[:space:]]"; then
    ok "$s"
  else
    printf '\033[31mmissing:\033[0m %s\n' "$s" >&2
    missing=1
  fi
done
[ "$missing" -eq 0 ] || die "not all secrets were written"

cat <<EOF

Done. The next tag you push will sign, notarize and staple.

  git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z

release.yml verifies the result against the artifact users download: a
Developer ID authority, the hardened runtime flag, the microphone entitlement,
spctl acceptance, and a stapled notarization ticket. If any of those regress
the release fails, rather than shipping a build that looks fine right up until
somebody downloads it.

Notarization adds roughly 3-10 minutes per macOS job while Apple's service
processes the upload.
EOF
