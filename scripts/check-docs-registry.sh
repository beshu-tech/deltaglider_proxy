#!/usr/bin/env bash
# Enforce docs bundling integrity. Both the in-product viewer
# (rust-embed of docs/product/ in src/demo.rs, served at GET /_/api/docs) and the marketing site
# (marketing/src/lib/docContent.ts) load docs via a Vite glob over
# `docs/product/**/*.md`, so the only file allowed into the bundle is one
# that lives under docs/product/. That leaves two things to enforce:
#
#  0. No symlinks under docs/product/ — a symlink could point a glob hit
#     at a dev-only doc outside the tree.
#  1. manifest.json <-> disk parity — every .md is in the manifest (so it
#     gets a group/order and renders) and every manifest path has a file.
#
# Exit codes:
#   0 — clean
#   1 — mismatch (see stderr for the offending file(s))
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PRODUCT_DIR="$ROOT/docs/product"

if [ ! -d "$PRODUCT_DIR" ]; then
  echo "ERROR: $PRODUCT_DIR not found" >&2
  exit 2
fi

fail=0

# (0) No symlinks under docs/product/ — a symlink could smuggle a dev doc
#     into the glob by pointing at a target outside the tree.
while IFS= read -r -d '' link; do
  rel="${link#"$ROOT"/}"
  echo "SYMLINK REJECTED: $rel" >&2
  echo "  -> symlinks inside docs/product/ can bypass the bundling checks; remove it" >&2
  fail=1
done < <(find "$PRODUCT_DIR" -type l -print0 2>/dev/null)

# (1) manifest.json ↔ disk parity. The manifest (docs/product/manifest.json)
#     is the SHARED source of truth for grouping + ordering, read by BOTH the
#     in-product viewer (src/demo.rs rust-embed → GET /_/api/docs) and the marketing website
#     (marketing/src/lib/docs.ts). A doc that exists on disk but is missing
#     from the manifest would render with no group/order in both surfaces (or
#     not at all on the website); a manifest path with no file is a dangling
#     entry. This is also what guarantees every bundled .md is intentional —
#     the glob picks up any file on disk, the manifest decides what renders.
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
  echo "docs registry OK: no smuggling symlinks + manifest↔disk parity verified"
fi

exit "$fail"
