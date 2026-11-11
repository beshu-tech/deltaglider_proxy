#!/usr/bin/env bash
# Workflow drift guard (run by the ci.yml verify job).
#   1. Every `uses:` in .github/workflows is pinned to a 40-hex commit SHA and
#      carries a version comment (`@<sha> # v1.2.3`). A tag can move; a SHA cannot.
#   2. The MinIO service image (pgsty/silo) has ONE digest across all
#      workflows: the copies of the MinIO block must agree.
#   3. `[skip-integration]` in the head commit message is allowed only when
#      the whole push range touches only *.md, docs/** and CHANGELOG.md.
#      Set SKIP_CHECK_BEFORE, SKIP_CHECK_AFTER and SKIP_CHECK_MESSAGE to run it
#      (ci.yml passes github.event.before, github.sha, head_commit.message).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

err=0
fail() { echo "error: $*" >&2; err=1; }

shopt -s nullglob
workflows=(.github/workflows/*.yml .github/workflows/*.yaml)

# ── 1. SHA pins ──────────────────────────────────────────────────────────
while IFS= read -r hit; do
  file=${hit%%:*}; rest=${hit#*:}; line=${rest%%:*}; text=${rest#*:}
  ref=$(sed -E 's/^[[:space:]-]*uses:[[:space:]]*["'\'']?([^"'\'' #]+).*/\1/' <<< "$text")
  case "$ref" in ./*|docker://*) continue ;; esac
  if ! [[ "$ref" =~ @[0-9a-f]{40}$ ]]; then
    fail "$file:$line: '$ref' is not pinned to a commit SHA"
  elif ! [[ "$text" =~ \#[[:space:]]*[^[:space:]]+ ]]; then
    fail "$file:$line: '$ref' has no version comment (# vX.Y.Z)"
  fi
done < <(grep -nE '^[[:space:]-]*uses:' "${workflows[@]}" /dev/null || true)

# ── 2. One MinIO digest ──────────────────────────────────────────────────
if grep -nE 'pgsty/silo:[^[:space:]]' "${workflows[@]}" /dev/null; then
  fail "a workflow names pgsty/silo by tag; pin it by digest (pgsty/silo@sha256:...)"
fi
digests=$(grep -ohE 'pgsty/silo@sha256:[0-9a-f]{64}' "${workflows[@]}" /dev/null | sort -u || true)
if [[ $(grep -c . <<< "$digests") -gt 1 ]]; then
  fail "workflows pin different pgsty/silo digests; use one:"$'\n'"$digests"
fi

# ── 3. [skip-integration] only for docs-only pushes ──────────────────────
docs_only_path() {
  case "$1" in
    docs/*|CHANGELOG.md|*.md) return 0 ;;
    *) return 1 ;;
  esac
}
if [[ "${SKIP_CHECK_MESSAGE:-}" == *"[skip-integration]"* ]]; then
  before=${SKIP_CHECK_BEFORE:-}
  after=${SKIP_CHECK_AFTER:-HEAD}
  if [[ -z "$before" || "$before" =~ ^0+$ ]] || ! git cat-file -e "${before}^{commit}" 2>/dev/null; then
    fail "[skip-integration]: the push range has no known start ('${before}'); run the integration tests"
  else
    while IFS= read -r path; do
      [[ -z "$path" ]] && continue
      docs_only_path "$path" || fail "[skip-integration] but ${before:0:12}..${after:0:12} changes $path (only *.md, docs/**, CHANGELOG.md may skip)"
    # --no-renames: a rename `src/a.rs -> docs/a.md` lists BOTH paths (a delete
    # and an add). With rename detection only the new, docs-only path shows.
    done < <(git diff --no-renames --name-only "$before" "$after")
  fi
fi

if [[ $err -ne 0 ]]; then
  exit 1
fi
echo "ok: workflow refs SHA-pinned, one MinIO digest, [skip-integration] scope"
