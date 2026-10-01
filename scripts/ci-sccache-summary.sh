#!/usr/bin/env bash
# Print WHY sccache cache writes failed in this CI job. sccache counts a failed
# write in `--show-stats` ("Cache write errors") but logs the reason only at
# debug level, so ci.yml points SCCACHE_ERROR_LOG at a file and raises
# sccache::server to debug. Hex digests are folded so equal errors group.
set -uo pipefail
log=${SCCACHE_ERROR_LOG:-/tmp/sccache.log}
[ -f "$log" ] || { echo "no sccache log at $log"; exit 0; }
fold() { sed -E 's/[0-9a-f]{16,}/<hex>/g' | cut -c1-400 | sort | uniq -c | sort -rn | head -25; }
echo "cache writes ok:     $(grep -c 'Cache write finished' "$log")"
echo "cache write errors:  $(grep -c 'Error executing cache write' "$log")"
echo "--- write errors by reason"
grep -o 'Error executing cache write.*' "$log" | fold
echo "--- other WARN/ERROR lines"
grep -E '\b(WARN|ERROR)\b' "$log" | sed -E 's/^\[[^]]*\]//' | fold
exit 0
