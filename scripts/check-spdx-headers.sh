#!/usr/bin/env bash
# Fail CI if any .rs file is missing the SPDX-License-Identifier header.
#
# Every Rust source file in this repo MUST start with:
#     // SPDX-License-Identifier: GPL-3.0-only
#
# This enforces the GPL-3.0 relicense (v0.10.0) at the file level so that
# new files added in PRs can't accidentally ship without the header.
#
# Run locally:   ./scripts/check-spdx-headers.sh
# Run in CI:     same script, invoked by .github/workflows/ci.yml

set -euo pipefail

EXPECTED='// SPDX-License-Identifier: GPL-3.0-only'

# Find every tracked + untracked .rs file (excluding target/, .git/, node_modules/).
#
# Exact-match the first line — NOT a substring grep. A file with a
# stray space, BOM, or other leading character must NOT pass; the SPDX
# spec requires the header to be the first thing on the first line.
missing=()
while IFS= read -r f; do
    first_line=$(head -n 1 "$f")
    if [ "$first_line" != "$EXPECTED" ]; then
        missing+=("$f")
    fi
done < <(find . -name '*.rs' \
    -not -path './target/*' \
    -not -path './.git/*' \
    -not -path './node_modules/*' \
    -not -path './marketing/node_modules/*')

if [ "${#missing[@]}" -gt 0 ]; then
    echo "::error::SPDX-License-Identifier header missing in ${#missing[@]} file(s):"
    for f in "${missing[@]}"; do
        echo "  $f"
    done
    echo ""
    echo "Every .rs file must start with:"
    echo "  $EXPECTED"
    echo ""
    echo "To fix locally, prepend the header to each file. For bulk fixes:"
    echo "  for f in <files>; do"
    echo "    { echo '$EXPECTED'; echo; cat \"\$f\"; } > \"\$f.tmp\" && mv \"\$f.tmp\" \"\$f\""
    echo "  done"
    exit 1
fi

echo "OK: SPDX-License-Identifier header present in all .rs files."
