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

# (0) No symlinks under docs/product/ — symlinks are the classic way
#     to smuggle a dev doc into the bundle since check (1) would only
#     see the symlink's target path after resolution.
#     `find -L` follows symlinks, so we detect the discrepancy by
#     comparing the symlink form vs the no-follow form.
while IFS= read -r -d '' link; do
  rel="${link#"$ROOT"/}"
  echo "SYMLINK REJECTED: $rel" >&2
  echo "  -> symlinks inside docs/product/ can bypass the bundling checks; remove it" >&2
  fail=1
done < <(find "$PRODUCT_DIR" -type l -print0 2>/dev/null)

# (1) Every docs/product/**/*.md must be imported.
#     We look for a literal `docs/product/<relative>.md?raw` substring
#     in the registry; exact match on the relative path (no regex
#     shenanigans), so a file rename keeps it strict. `-L` follows
#     symlinks (which we've already rejected at step 0) so even if the
#     rejection somehow failed-open, we'd still require a registry
#     entry for the resolved file.
while IFS= read -r -d '' file; do
  rel="${file#"$ROOT"/docs/product/}"
  needle="docs/product/${rel}?raw"
  if ! grep -qF -- "$needle" "$REGISTRY"; then
    echo "UNREGISTERED: docs/product/$rel" >&2
    echo "  -> add a matching import + PRODUCT_DOCS entry in docs-imports.ts" >&2
    fail=1
  fi
done < <(find -L "$PRODUCT_DIR" -type f -name '*.md' -print0)

# (2) Registry imports must come from docs/product ONLY.
#     Positive allow-list instead of a deny-list so a new sibling like
#     docs/drafts/ can't sneak in. Every `from '../../../../docs/<x>'`
#     must have `product/` as its first path segment under docs/.
while IFS= read -r line; do
  if [[ "$line" == *"'../../../../docs/"* ]] && [[ "$line" != *"'../../../../docs/product/"* ]]; then
    echo "NON-PRODUCT IMPORT in registry: $line" >&2
    echo "  -> only docs/product/** may be bundled. Move this file or the import." >&2
    fail=1
  fi
done < <(grep -E "^import .* from '\\.\\./\\.\\./\\.\\./\\.\\./docs/" "$REGISTRY" || true)

if [ "$fail" -eq 0 ]; then
  echo "docs registry OK: all files under docs/product/ are bundled, no dev docs leaked"
fi

exit "$fail"
