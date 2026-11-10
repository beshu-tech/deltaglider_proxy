#!/usr/bin/env bash
# A Playwright spec that skips itself on an env var nobody sets never runs:
# the "reproduction" specs for fixed findings (EXPLORE=1, REVIEW=1) sat in the
# tree for months and no job ran them. Every `test.skip(!process.env.X ...)`
# / `test.skip(process.env.X ...)` in demo/s3-browser/ui/e2e must name a
# variable that a script under scripts/ or a workflow under .github/ sets
# (`X=...`, `export X=...`, or `X: ...` in a workflow env block). This script
# and its self-test name such variables in text and do not count.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

err=0
shopt -s nullglob globstar
specs=(demo/s3-browser/ui/e2e/**/*.spec.ts)
for spec in "${specs[@]}"; do
  case "$spec" in */docs-screenshots/*) continue ;; esac
  while IFS= read -r name; do
    [[ -z "$name" ]] && continue
    if ! grep -rqE --exclude=check-e2e-specs-run.sh --exclude=test-guards.sh "(^|[^A-Za-z0-9_])${name}[=:]" scripts .github 2>/dev/null; then
      echo "error: $spec skips itself on process.env.${name}, which no script or workflow sets: the spec never runs" >&2
      err=1
    fi
  done < <(grep -E 'test\.skip\(' "$spec" | grep -oE 'process\.env\.[A-Z][A-Z0-9_]*' | sed 's/^process\.env\.//' | sort -u)
done
((err == 0)) || exit 1
echo "OK: every e2e spec's skip variable is set by a script or workflow."
