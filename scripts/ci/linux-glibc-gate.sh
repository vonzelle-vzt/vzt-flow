#!/usr/bin/env bash
# Fail if any ELF under an extracted AppDir needs glibc > 2.35, libstdc++
# GLIBCXX > 3.4.30, or CXXABI > 1.3.13 (Ubuntu 22.04 / jammy baseline).
# Usage: linux-glibc-gate.sh <extracted-AppDir>
set -euo pipefail
ROOT="${1:?usage: $0 <AppDir>}"
MAX_GLIBC=2.35; MAX_GLIBCXX=3.4.30; MAX_CXXABI=1.3.13

newer() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ] && [ "$1" != "$2" ]; }  # $1 > $2

offenders=""; seen_glibc=0; seen_glibcxx=0; seen_cxxabi=0; n=0
while IFS= read -r -d '' f; do
  [ "$(head -c4 "$f" 2>/dev/null | od -An -c | tr -d ' ')" = '177ELF' ] || continue
  n=$((n+1))
  und="$(objdump -T "$f" 2>/dev/null | awk '/\*UND\*/' || true)"
  for pair in "GLIBC:$MAX_GLIBC" "GLIBCXX:$MAX_GLIBCXX" "CXXABI:$MAX_CXXABI"; do
    tag="${pair%%:*}"; max="${pair##*:}"
    top="$(echo "$und" | grep -o "${tag}_[0-9][0-9.]*" | sed "s/^${tag}_//" | sort -Vu | tail -1 || true)"
    [ -n "$top" ] || continue
    if newer "$top" "$max"; then
      offenders="${offenders}${f#"$ROOT"/}: needs ${tag}_${top} (max ${max})"$'\n'
    fi
    # track global maxima for the log
    cur_var="seen_$(echo "$tag" | tr 'A-Z' 'a-z')"; cur="${!cur_var}"
    if [ "$cur" = 0 ] || newer "$top" "$cur"; then printf -v "$cur_var" '%s' "$top"; fi
  done
done < <(find "$ROOT" -type f \( -path '*/usr/bin/*' -o -name '*.so' -o -name '*.so.*' -o -name 'AppRun.wrapped' \) -print0)

echo "Scanned $n ELF files"
echo "Max GLIBC required:   $seen_glibc (limit $MAX_GLIBC)"
echo "Max GLIBCXX required: $seen_glibcxx (limit $MAX_GLIBCXX)"
echo "Max CXXABI required:  $seen_cxxabi (limit $MAX_CXXABI)"
if [ "$n" -eq 0 ]; then echo "::error::no ELF files found under $ROOT"; exit 1; fi
if [ -n "$offenders" ]; then
  echo "::error::binaries need symbols newer than Ubuntu 22.04 provides:"
  printf '%s' "$offenders"
  exit 1
fi
echo "glibc floor gate passed"
