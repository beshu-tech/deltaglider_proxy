#!/usr/bin/env bash
# Generate docs/product/changelog.md from the root CHANGELOG.md.
#
# The root CHANGELOG.md is the single source of truth. This script
# projects it into the shared docs pipeline (manifest.json +
# rust-embed in src/demo.rs, served at GET /_/api/docs) so the changelog renders on BOTH the marketing site
# (/docs/changelog) and the in-product docs viewer (/_/docs/changelog),
# using the same renderer/styling as every other doc.
#
# What it does:
#   - Replaces the `# Changelog` title with a docs-friendly title + a
#     one-line intro (the first `# heading` becomes the page title in
#     both surfaces).
#   - Drops the `## Unreleased` section (nothing to show users yet).
#   - Stamps a "generated — do not edit" banner so nobody hand-edits the
#     bundled copy.
#
# Run it whenever CHANGELOG.md changes. CI (--check) fails if the
# committed docs copy is stale, keeping the two in lockstep without a
# symlink (which the docs-registry check rejects).
#
# Usage:
#   scripts/gen-changelog-doc.sh            # write docs/product/changelog.md
#   scripts/gen-changelog-doc.sh --check    # fail if it would change
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/CHANGELOG.md"
OUT="$ROOT/docs/product/changelog.md"

[ -f "$SRC" ] || { echo "ERROR: $SRC not found" >&2; exit 2; }

# Build the generated doc:
#   1. Header block (title + intro + do-not-edit banner).
#   2. The body of CHANGELOG.md from the first `## v` version heading
#      onward — i.e. skip the `# Changelog` title and the `## Unreleased`
#      placeholder, start at the newest real release.
# Newest release date = the date on the first `## vX.Y.Z — DATE` heading
# that actually has content under it. Drives the "Last updated" line.
newest_release_date() {
  awk '
    found { exit }
    /^## v/ {
      # flush the previous block: if it had content, its date wins (we
      # scan newest-first, so the FIRST non-empty block is the answer).
      if (have && content) { print date; found = 1; next }
      have = 1; content = 0
      date = $0
      sub(/^## v[^ ]*[[:space:]]*—[[:space:]]*/, "", date)  # strip "## vX — "
      sub(/^## v[^ ]*[[:space:]]*$/, "", date)              # no-date heading → empty
      gsub(/[[:space:]]+$/, "", date)
      next
    }
    have && /[^[:space:]]/ && !/^<!--/ { content = 1 }
    END { if (!found && have && content) print date }
  ' "$SRC"
}

generate() {
  local updated
  updated="$(newest_release_date)"

  cat <<HEADER
<!-- GENERATED FILE — do not edit.
     Source of truth: CHANGELOG.md at the repo root.
     Regenerate with: scripts/gen-changelog-doc.sh -->

# Changelog

Every released version of DeltaGlider Proxy, newest first. Versions
follow [semantic versioning](https://semver.org/); the Docker image
\`beshultd/deltaglider_proxy:<version>\` is published for each tag.

_Last updated: ${updated}_

HEADER

  # Emit version sections from the first "## v" heading to EOF, but DROP
  # any version that has no changeset (an empty `## vX.Y.Z` header with
  # nothing but blank lines before the next `##`). This skips placeholder
  # headers (e.g. a freshly-cut tag whose notes weren't filled in) on the
  # rendered page while leaving CHANGELOG.md itself untouched. Buffer each
  # `## v` block, print it only once we've seen real content.
  awk '
    function flush() {
      if (n > 0 && nonblank) { for (i = 0; i < n; i++) print buf[i] }
      n = 0; nonblank = 0
    }
    /^## v/ { flush(); started = 1 }
    !started { next }
    {
      buf[n++] = $0
      # A line counts as "content" if it is non-blank and is not the
      # version heading itself.
      if ($0 !~ /^## v/ && $0 ~ /[^[:space:]]/) nonblank = 1
    }
    END { flush() }
  ' "$SRC"
}

if [ "${1:-}" = "--check" ]; then
  if ! diff -u "$OUT" <(generate) >/dev/null 2>&1; then
    echo "ERROR: docs/product/changelog.md is stale vs CHANGELOG.md" >&2
    echo "  -> run scripts/gen-changelog-doc.sh and commit the result" >&2
    diff -u "$OUT" <(generate) >&2 || true
    exit 1
  fi
  echo "changelog doc OK: docs/product/changelog.md matches CHANGELOG.md"
  exit 0
fi

generate > "$OUT"
echo "wrote $OUT"
