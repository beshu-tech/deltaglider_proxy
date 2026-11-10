#!/usr/bin/env bash
# RSS-bounded LIST load test (F3, epic #142). One folder of 200,000 files,
# 20 concurrent LIST requests of one key each, for the flat listing, the `/`
# delimiter and another delimiter. The proxy's peak RSS (VmHWM) must stay
# under a bound that does not depend on the folder size: before the fix this
# took 1.85 GB. Nightly CI runs it; it is not in the PR gate.
#
#   DELTAGLIDER_PROXY_BIN=target/release/deltaglider_proxy ./scripts/list-load.sh
#   LIST_LOAD_FILES=20000 RSS_MAX_MB=300 ./scripts/list-load.sh   # a smaller run
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FILES="${LIST_LOAD_FILES:-200000}"
CONCURRENT="${LIST_LOAD_CONCURRENT:-20}"
RSS_MAX_MB="${RSS_MAX_MB:-400}"

# shellcheck source=lib/e2e-proxy.sh
source "$ROOT/scripts/lib/e2e-proxy.sh"
e2e_start_proxy open
base="$PLAYWRIGHT_BASE_URL"

curl -fsS -X PUT "$base/load" >/dev/null
folder="$E2E_DIR/data/load/deltaspaces/big"
mkdir -p "$folder"
python3 - "$folder" "$FILES" <<'PY'
import sys
folder, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    open(f"{folder}/k_{i:07d}.jpg", "wb").close()
PY
echo "created $FILES files in $folder"

hwm_mb() { awk '/VmHWM/ {print int($2 / 1024)}' "/proc/$E2E_PID/status"; }
before="$(hwm_mb)"

fail=0
for query in \
  "list-type=2&prefix=big%2F&max-keys=1" \
  "list-type=2&prefix=big%2F&max-keys=1&delimiter=%2F" \
  "list-type=2&prefix=big%2F&max-keys=1&delimiter=_" \
  "list-type=2&max-keys=1&delimiter=_"; do
  pids=()
  for _ in $(seq "$CONCURRENT"); do
    curl -fsS -o /dev/null "$base/load?$query" &
    pids+=($!)
  done
  for pid in "${pids[@]}"; do
    wait "$pid" || fail=1
  done
  echo "$query: peak RSS $(hwm_mb) MB"
done

after="$(hwm_mb)"
echo "VmHWM before the load: ${before} MB, after: ${after} MB (bound ${RSS_MAX_MB} MB)"
if [[ "$fail" != 0 ]]; then
  echo "FAIL: a LIST request failed" >&2
  exit 1
fi
if (( after > RSS_MAX_MB )); then
  echo "FAIL: peak RSS ${after} MB is above ${RSS_MAX_MB} MB" >&2
  exit 1
fi
echo "OK"
