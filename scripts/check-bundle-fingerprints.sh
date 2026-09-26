#!/usr/bin/env bash
# =============================================================================
# check-bundle-fingerprints.sh <dist-dir> <Cargo.toml>
# -----------------------------------------------------------------------------
# Everything under dist/ is served to ANONYMOUS callers at /_/. This guard
# fails the build when the bundle would let them fingerprint the deployment:
#   1. the crate version string (a Vite `define`, a hard-coded chip, ...)
#   2. a build timestamp literal (ISO 8601 with a trailing Z)
#   3. product docs inlined at build time (the changelog names every release)
#   4. source maps (the full UI source)
#   5. (size) the markdown/docs stack loaded by index.html up front
# The running version and build time reach the UI only through the
# session-authenticated /_/api/whoami, and the docs through /_/api/docs.
# Runs after `npm run build`: wired into the Dockerfile UI stage and CI.
#
# Library code can legitimately contain a dotted number equal to our version
# (a dependency at the same version). Match 1 is boundary-aware and prints the
# file; if that ever collides, widen the check rather than drop it.
# =============================================================================
set -euo pipefail

dist="${1:?usage: $0 <dist-dir> <Cargo.toml>}"
cargo_toml="${2:?usage: $0 <dist-dir> <Cargo.toml>}"
[ -d "$dist" ] || { echo "check-bundle-fingerprints: no such dist dir: $dist" >&2; exit 2; }
version=$(grep -m1 -E '^version *= *"' "$cargo_toml" | sed -E 's/.*"([^"]+)".*/\1/')
[ -n "$version" ] || { echo "check-bundle-fingerprints: cannot read version from $cargo_toml" >&2; exit 2; }

# GNU grep is required (`--include`). BusyBox grep rejects the option, and a
# swallowed error here would turn the guard into a silent pass, so probe once
# and fail loudly. (This is why the Dockerfile runs the guard in the Debian
# Rust stage, not in the node:alpine UI stage.)
if ! grep --version 2>/dev/null | grep -q 'GNU grep'; then
  echo "check-bundle-fingerprints: GNU grep required (BusyBox grep has no --include)" >&2; exit 2
fi

fail=0
report() { echo "FINGERPRINT: $1" >&2; fail=1; }
# grep exits 1 on "no match" (fine) and 2 on an error (not fine).
hits_of() {
  local out
  out=$(grep -rlE "$1" "$dist" --include='*.js' --include='*.html' --include='*.css' --include='*.json'); local rc=$?
  if [ "$rc" -gt 1 ]; then echo "check-bundle-fingerprints: grep failed (rc=$rc) for pattern: $1" >&2; exit 2; fi
  printf '%s' "$out"
}

# 1. The crate version, standalone (not a longer dotted number).
escaped=$(printf '%s' "$version" | sed 's/\./\\./g')
hits=$(hits_of "(^|[^0-9.])${escaped}([^0-9.]|$)")
[ -z "$hits" ] || report "crate version ${version} baked into: $(echo "$hits" | tr '\n' ' ')"

# 2. Build timestamp literals.
hits=$(hits_of '20[0-9]{2}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(\.[0-9]+)?Z')
[ -z "$hits" ] || report "build timestamp literal in: $(echo "$hits" | tr '\n' ' ')"

# 3. Product docs inlined: these strings exist only under docs/product/.
for needle in 'Single source of truth for product-docs grouping' '## v[0-9]+\.[0-9]+\.[0-9]+ '; do
  hits=$(hits_of "$needle")
  [ -z "$hits" ] || report "product docs inlined (matched '$needle') in: $(echo "$hits" | tr '\n' ' ')"
done

# 4. Source maps.
maps=$(find "$dist" -name '*.map' -print)
[ -z "$maps" ] || report "source maps present: $(echo "$maps" | tr '\n' ' ')"

# 5. (Size, not a fingerprint.) The file-browser entry must not load the
#    markdown stack: index.html's entry script and modulepreloads must not
#    contain the markdown parser (micromark). It belongs to the lazy docs
#    chunks; a manual `markdown` chunk once dragged react/jsx-runtime in and
#    made every page preload 330 kB for the docs.
if [ -f "$dist/index.html" ]; then
  for asset in $(grep -oE '(src|href)="/_/assets/[^"]+\.js"' "$dist/index.html" | sed -E 's#.*/_/assets/([^"]+)"#\1#'); do
    if grep -q 'micromark' "$dist/assets/$asset"; then
      report "index.html loads the markdown stack up front via assets/$asset (keep it in the lazy docs chunks)"
    fi
  done
fi

if [ "$fail" -ne 0 ]; then
  echo "check-bundle-fingerprints: FAILED - the bundle identifies the build to anonymous callers, or preloads the docs stack" >&2
  exit 1
fi
echo "check-bundle-fingerprints: OK (no version, build time, docs, source maps or up-front docs stack in $dist)"
