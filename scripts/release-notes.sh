#!/usr/bin/env bash
# Print the GitHub Release body for one version from CHANGELOG.md.
#
#   scripts/release-notes.sh 2.0.0 > release-notes.md
#
# The body holds the section's "Upgrade steps" block (when there is one) in
# full, then every other entry heading of the section, grouped by kind
# (Security, Changed, Added, Fixed, Docs), then a link to the full text.
# The full section can pass GitHub's 125,000-character body limit, so the
# entries are listed by heading; each heading is one change.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${1:?usage: $0 X.Y.Z}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "error: bad version '$VERSION'" >&2; exit 1; }

CHANGELOG="${CHANGELOG:-$ROOT/CHANGELOG.md}"
section="$(awk -v v="## v$VERSION " '
  index($0, v) == 1 { on = 1; next }
  on && /^## / { exit }
  on { print }
' "$CHANGELOG")"
[ -n "$section" ] || { echo "error: no '## v$VERSION' section in CHANGELOG.md" >&2; exit 1; }

# The Upgrade steps block: from its heading to the next "### ".
awk '
  /^### Upgrade steps/ { on = 1; sub(/^### /, "## "); print; next }
  on && /^### / { exit }
  on { print }
' <<< "$section"

for kind in Security Changed Added Fixed Docs; do
  list="$(grep -E "^### $kind — " <<< "$section" | sed -E "s/^### $kind — /- /" || true)"
  [ -n "$list" ] || continue
  printf '\n## %s (%s)\n\n%s\n' "$kind" "$(grep -c '' <<< "$list")" "$list"
done

printf '\nThe full text of every entry is in [CHANGELOG.md](https://github.com/beshu-tech/deltaglider_proxy/blob/v%s/CHANGELOG.md).\n' "$VERSION"
