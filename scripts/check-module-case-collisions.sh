#!/usr/bin/env bash
# Fail when two tracked JS/TS module paths differ only in case or extension.
# On a case-insensitive filesystem (macOS) `./htmlPreview` then resolves to
# `HtmlPreview.tsx`, and tsc fails in the macOS release builds. The extension
# is stripped on purpose: `git ls-files | sort -f | uniq -di` misses a pair
# such as `htmlPreview.ts` + `HtmlPreview.tsx`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

dups=$(git ls-files -- '*.ts' '*.tsx' '*.js' '*.jsx' '*.mjs' \
  | sed -E 's/\.(ts|tsx|js|jsx|mjs)$//' \
  | tr '[:upper:]' '[:lower:]' \
  | sort | uniq -d)

if [[ -n "$dups" ]]; then
  echo "error: module paths that differ only in case or extension:" >&2
  while IFS= read -r d; do
    echo "  $d" >&2
    git ls-files -- '*.ts' '*.tsx' '*.js' '*.jsx' '*.mjs' \
      | awk -v d="$d" '{ p = tolower($0); sub(/\.(ts|tsx|js|jsx|mjs)$/, "", p); if (p == d) print "    " $0 }' >&2
  done <<< "$dups"
  echo "Rename one of each pair: macOS resolves imports case-insensitively." >&2
  exit 1
fi
echo "ok: no module path case collisions"
