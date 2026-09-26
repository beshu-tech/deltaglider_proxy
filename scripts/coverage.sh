#!/usr/bin/env bash
# Lib + integration coverage with cargo-llvm-cov (nightly job `coverage`;
# also runs locally).
#
# `show-env` puts the coverage RUSTFLAGS and LLVM_PROFILE_FILE into this
# shell, so the proxy binary that the integration harness spawns
# (TestServer) is instrumented and writes its own profile. The harness
# stops a child with SIGTERM when LLVM_PROFILE_FILE is set (a SIGKILLed
# child writes no profile; see stop_child in tests/common/mod.rs).
#
# Writes to $COV_OUT (default target/coverage):
#   summary-lib.txt, lcov-lib.info            lib unit tests only
#   summary-combined.txt, lcov.info           lib + integration
#   compare.md                                TOTAL lib vs combined, and
#                                             every file under 50% combined
#
# Usage: scripts/coverage.sh [libtest filters for `cargo test --test all`]
# (no filter = every integration test). Exits non-zero if a test failed,
# after the reports are written.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="${COV_OUT:-target/coverage}"
mkdir -p "$OUT"

# shellcheck disable=SC1090
source <(cargo llvm-cov show-env --export-prefix)
cargo llvm-cov clean --workspace

status=0
cargo test --locked --lib || status=$?
cargo llvm-cov report --summary-only > "$OUT/summary-lib.txt"
cargo llvm-cov report --lcov --output-path "$OUT/lcov-lib.info"

cargo test --locked --test all -- "$@" || status=$?
cargo llvm-cov report --summary-only > "$OUT/summary-combined.txt"
cargo llvm-cov report --lcov --output-path "$OUT/lcov.info"

# Line cover % is column 10 of the summary table (Filename, Regions,
# Missed, Cover, Functions, Missed, Executed, Lines, Missed, Cover, ...).
line_cover() {
  awk '$1 ~ /\.rs$/ || $1 == "TOTAL" {gsub(/%/, "", $10); print $1, $10, $8}' "$1"
}
line_cover "$OUT/summary-lib.txt" | sort > "$OUT/.lib"
line_cover "$OUT/summary-combined.txt" | sort > "$OUT/.combined"
{
  echo "| | lib only | lib + integration |"
  echo "|---|---:|---:|"
  join "$OUT/.lib" "$OUT/.combined" | awk '$1 == "TOTAL" {printf "| lines | %s%% | %s%% |\n", $2, $4}'
  echo
  echo "Files under 50% line coverage, lib + integration (lines, lib %, combined %):"
  echo
  echo "| file | lines | lib % | combined % |"
  echo "|---|---:|---:|---:|"
  join -a 2 -e 0 -o 0,1.2,2.2,2.3 "$OUT/.lib" "$OUT/.combined" \
    | awk '$1 != "TOTAL" && $3 < 50 {printf "| %s | %s | %s | %s |\n", $1, $4, $2, $3}' \
    | sort -t'|' -k5 -n
} > "$OUT/compare.md"
rm -f "$OUT/.lib" "$OUT/.combined"
cat "$OUT/compare.md"
exit "$status"
