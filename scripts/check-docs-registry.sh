#!/usr/bin/env bash
# Enforce the docs bundling allow-list.
#
# Two blocking checks:
#  1. Every .md under docs/product/ must be imported in
#     demo/s3-browser/ui/src/docs-imports.ts. An unregistered file
#     would ship on disk for GitHub rendering but never reach the
#     product bundle — we want "if you add a product doc, register it."
#  2. The docs-imports.ts imports must not reference anything outside
#     docs/product/. Dev docs (docs/dev/, anything under /historical/)
#     must never be bundled into the binary.
#
# Exit codes:
#   0 — registry and filesystem agree
#   1 — mismatch (see stderr for the offending file(s))
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REGISTRY="$ROOT/demo/s3-browser/ui/src/docs-imports.ts"
PRODUCT_DIR="$ROOT/docs/product"

if [ ! -f "$REGISTRY" ]; then
  echo "ERROR: registry not found at $REGISTRY" >&2
  exit 2
fi
if [ ! -d "$PRODUCT_DIR" ]; then
  echo "ERROR: $PRODUCT_DIR not found" >&2
  exit 2
fi

fail=0

# (1) Every docs/product/**/*.md must be imported.
#     We look for a literal `docs/product/<relative>.md?raw` substring
#     in the registry; exact match on the relative path (no regex
#     shenanigans), so a file rename keeps it strict.
while IFS= read -r -d '' file; do
  rel="${file#"$ROOT"/docs/product/}"
  needle="docs/product/${rel}?raw"
  if ! grep -qF -- "$needle" "$REGISTRY"; then
    echo "UNREGISTERED: docs/product/$rel" >&2
    echo "  -> add a matching import + PRODUCT_DOCS entry in docs-imports.ts" >&2
    fail=1
  fi
done < <(find "$PRODUCT_DIR" -type f -name '*.md' -print0)

# (2) No dev-docs import path should appear.
#     Match on the project-root relative prefix — the registry imports
#     use `../../../../docs/dev/...`, so we look for that literal
#     substring.
if grep -E "from '\\.\\./\\.\\./\\.\\./\\.\\./docs/(dev|historical)" "$REGISTRY" >/dev/null 2>&1; then
  echo "DEV-DOC IMPORT in product registry:" >&2
  grep -nE "from '\\.\\./\\.\\./\\.\\./\\.\\./docs/(dev|historical)" "$REGISTRY" >&2
  echo "  -> dev docs must never be bundled; move the import out of docs-imports.ts" >&2
  fail=1
fi

# (3) Every registry import MUST point at docs/product/*.
#     The registry's own line syntax is stable:
#       import <NAME> from '../../../../docs/<path>.md?raw';
#     Assert each such line has `docs/product/` as its prefix under docs/.
while IFS= read -r line; do
  if [[ "$line" == *"'../../../../docs/"* ]] && [[ "$line" != *"'../../../../docs/product/"* ]]; then
    echo "NON-PRODUCT IMPORT in registry: $line" >&2
    fail=1
  fi
done < <(grep -E "^import .* from '\\.\\./\\.\\./\\.\\./\\.\\./docs/" "$REGISTRY" || true)

if [ "$fail" -eq 0 ]; then
  echo "docs registry OK: all files under docs/product/ are bundled, no dev docs leaked"
fi

exit "$fail"
