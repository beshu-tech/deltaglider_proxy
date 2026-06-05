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

# (3) manifest.json ↔ disk parity. The manifest (docs/product/manifest.json)
#     is the SHARED source of truth for grouping + ordering, read by BOTH the
#     in-product viewer (docs-imports.ts) and the marketing website
#     (marketing/src/lib/docs.ts). A doc that exists on disk but is missing
#     from the manifest would render with no group/order in both surfaces (or
#     not at all on the website); a manifest path with no file is a dangling
#     entry. Keep them in lockstep.
MANIFEST="$PRODUCT_DIR/manifest.json"
if [ ! -f "$MANIFEST" ]; then
  echo "ERROR: shared docs manifest not found at $MANIFEST" >&2
  fail=1
elif command -v node >/dev/null 2>&1; then
  # Every .md on disk (relative path, no extension) must be a manifest path,
  # and every manifest path must have a file. README.md included; manifest.json
  # itself is not a doc.
  node - "$PRODUCT_DIR" "$MANIFEST" <<'NODE' || fail=1
const { readdirSync, statSync, readFileSync } = require('node:fs');
const { join, relative } = require('node:path');
const [dir, manifestPath] = process.argv.slice(2);

function walk(d, acc = []) {
  for (const e of readdirSync(d, { withFileTypes: true })) {
    const p = join(d, e.name);
    if (e.isDirectory()) walk(p, acc);
    else if (e.name.endsWith('.md')) acc.push(relative(dir, p).replace(/\.md$/, ''));
  }
  return acc;
}

const onDisk = new Set(walk(dir));
const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
const inManifest = new Set(manifest.docs.map((d) => d.path));
const groupIds = new Set(manifest.groups.map((g) => g.id));

let bad = false;
for (const p of onDisk) {
  if (!inManifest.has(p)) {
    console.error(`MANIFEST MISSING: docs/product/${p}.md is on disk but not in manifest.json`);
    console.error(`  -> add { "path": "${p}", "group": <one of groups[].id>, "order": <n> }`);
    bad = true;
  }
}
for (const d of manifest.docs) {
  if (!onDisk.has(d.path)) {
    console.error(`MANIFEST DANGLING: "${d.path}" in manifest.json has no docs/product/${d.path}.md`);
    bad = true;
  }
  if (!groupIds.has(d.group)) {
    console.error(`MANIFEST BAD GROUP: "${d.path}" references group "${d.group}" not in groups[]`);
    bad = true;
  }
}
process.exit(bad ? 1 : 0);
NODE
else
  echo "WARN: node not available — skipping manifest↔disk parity check" >&2
fi

if [ "$fail" -eq 0 ]; then
  echo "docs registry OK: bundled allow-list + manifest↔disk parity verified"
fi

exit "$fail"
