#!/usr/bin/env bash
# Run the QA pass against a release binary: e2e/qa-regression.spec.ts and
# every e2e/qa-jobs-*.spec.ts. Nightly CI runs this; it is not in the PR gate
# (it takes minutes and drives the whole admin UI).
#
# Each spec gets its OWN proxy with a fresh data dir, so the order of the
# specs does not matter (a job that one spec creates never shows up in
# another spec's "No jobs yet" step). qa-jobs-maintenance needs two named
# backends (mode qa-jobs); the others run in bootstrap mode. A failed spec
# does not stop the others; the script fails when any spec failed.
#
#   DELTAGLIDER_PROXY_BIN=target/release/deltaglider_proxy ./scripts/qa-e2e.sh
#   QA_SPECS="qa-jobs-overview" ./scripts/qa-e2e.sh   # a subset
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UI="$ROOT/demo/s3-browser/ui"

mode_for() {
  case "$1" in
    qa-jobs-maintenance) echo qa-jobs ;;
    *) echo bootstrap ;;
  esac
}

specs=()
if [[ -n "${QA_SPECS:-}" ]]; then
  read -r -a specs <<< "$QA_SPECS"
else
  specs=(qa-regression)
  for f in "$UI"/e2e/qa-jobs-*.spec.ts; do
    specs+=("$(basename "$f" .spec.ts)")
  done
fi

LOG_BASE="${E2E_LOG_OUT:-}"
failed=()
for spec in "${specs[@]}"; do
  echo "=== $spec ($(mode_for "$spec") proxy)"
  # A subshell per spec: its EXIT trap stops that spec's proxy.
  if (
    # shellcheck source=lib/e2e-proxy.sh
    source "$ROOT/scripts/lib/e2e-proxy.sh"
    if [[ -n "$LOG_BASE" ]]; then E2E_LOG_OUT="${LOG_BASE%.log}-$spec.log"; fi
    e2e_start_proxy "$(mode_for "$spec")"
    export QA_ACCESS_KEY="$E2E_ACCESS_KEY" QA_SECRET_KEY="$E2E_SECRET_KEY" QA_PUBLIC_BUCKET=qa-public
    # The data dir of the default filesystem backend: qa-jobs-lifecycle
    # plants objects with old modification times there, as a restore does.
    export QA_DATA_DIR="$E2E_DIR/data"
    export QA_REGRESSION=1
    # One report and one results dir per spec, so a later spec does not
    # overwrite the evidence of an earlier failure.
    export PLAYWRIGHT_HTML_OUTPUT_DIR="$UI/playwright-report/$spec"
    cd "$UI"
    npx playwright test "e2e/$spec.spec.ts" --output "test-results/$spec"
  ); then
    echo "=== $spec: passed"
  else
    echo "=== $spec: FAILED"
    failed+=("$spec")
  fi
done

if [[ ${#failed[@]} -gt 0 ]]; then
  echo "QA e2e: ${#failed[@]} of ${#specs[@]} specs failed: ${failed[*]}" >&2
  exit 1
fi
echo "QA e2e: all ${#specs[@]} specs passed"
