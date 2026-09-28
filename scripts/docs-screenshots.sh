#!/usr/bin/env bash
# Reproduce the docs screenshots (docs/screenshots/<id>.{light,dark}.webp).
#
#   scripts/docs-screenshots.sh              capture into a temp dir and compare with docs/screenshots/
#   scripts/docs-screenshots.sh --update     capture and write the WebP files into docs/screenshots/
#   scripts/docs-screenshots.sh --only a,b   only the shots with these ids
#
# Needs a release binary (DELTAGLIDER_PROXY_BIN, default target/release/...)
# built after `npm run build` in demo/s3-browser/ui, Playwright's Chromium,
# and MinIO on 127.0.0.1:9000 that holds no buckets other than the ones the
# seed creates (the S3 backends list every bucket they can see). When nothing
# answers there, the script starts the repo's docker compose MinIO with a
# fresh volume and removes it again at the end.
#
# The shots are declared in demo/s3-browser/ui/e2e/docs-screenshots/shots/.
# Exit 1 when a shot fails, a file is over budget, or (without --update) a
# shot differs from the committed file.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UI="$ROOT/demo/s3-browser/ui"
UPDATE=0
ONLY=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --update) UPDATE=1 ;;
    --only) ONLY="$2"; shift ;;
    *) echo "usage: $0 [--update] [--only id,id]" >&2; exit 2 ;;
  esac
  shift
done

OUT="${DOCS_SHOT_OUT:-$(mktemp -d)}"
# The browser computes relative times ("just now") from this fixed instant,
# and the seed runs after it, so every server time reads the same way.
export DOCS_NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
mkdir -p "$OUT/png" "$OUT/webp"
rm -f "$OUT"/png/*.png "$OUT"/webp/*.webp

MINIO="${DOCS_MINIO_ENDPOINT:-http://127.0.0.1:9000}"
STARTED_MINIO=0
if ! curl -sf "$MINIO/minio/health/live" >/dev/null 2>&1; then
  echo "docs-screenshots: starting MinIO (docker compose)"
  # Only the minio service: minio-setup would add the test buckets.
  docker compose -f "$ROOT/docker-compose.yml" up -d --wait minio >/dev/null
  STARTED_MINIO=1
fi

# shellcheck source=lib/e2e-proxy.sh
source "$ROOT/scripts/lib/e2e-proxy.sh"
export E2E_STATE_DIR="${E2E_STATE_DIR:-/tmp/dgp-docs}"
# A fixed port: the System page shows the listen address.
export E2E_PORT="${DOCS_PORT:-19480}"
e2e_start_proxy docs

finish() {
  e2e_cleanup
  if [[ "$STARTED_MINIO" -eq 1 ]]; then
    docker compose -f "$ROOT/docker-compose.yml" down -v >/dev/null 2>&1 || true
  fi
}
trap finish EXIT

VERSION="$(grep -m1 '^version = ' "$ROOT/Cargo.toml" | sed 's/^version = "\(.*\)"/\1/')"

cd "$UI"
DOCS_SCREENSHOTS=1 \
  DOCS_SHOT_DIR="$OUT/png" \
  DOCS_ONLY="$ONLY" \
  DOCS_CRATE_VERSION="$VERSION" \
  DOCS_MINIO_ENDPOINT="$MINIO" \
  npx playwright test e2e/docs-screenshots/docs-screenshots.spec.ts

args=(--png "$OUT/png" --webp "$OUT/webp" --committed "$ROOT/docs/screenshots")
[[ "$UPDATE" -eq 1 ]] && args+=(--update)
node scripts/docs-screenshots-finish.mjs "${args[@]}"

# The binary serves the files as image/webp at /_/screenshots/ (rust-embed +
# mime_guess). A binary built before the first WebP was committed has none.
mapfile -t webps < <(find "$ROOT/docs/screenshots" -name '*.webp' | sort)
sample="${webps[0]:-}"
if [[ -n "$sample" ]]; then
  url="$PLAYWRIGHT_BASE_URL/_/screenshots/$(basename "$sample")"
  ctype="$(curl -s -o /dev/null -w '%{http_code} %{content_type}' "$url")"
  case "$ctype" in
    "200 image/webp") ;;
    404*) echo "note: the binary does not embed $(basename "$sample") yet; rebuild the UI and the binary to check how it is served" ;;
    *) echo "error: $url answered '$ctype', want '200 image/webp'" >&2; exit 1 ;;
  esac
fi
echo "docs-screenshots: output in $OUT"
