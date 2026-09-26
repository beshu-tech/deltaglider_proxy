#!/usr/bin/env bash
# SPDX-License-Identifier: BUSL-1.1
#
# Run one libFuzzer target from fuzz/ for N seconds on the STABLE toolchain.
#
#   scripts/fuzz.sh <target> [seconds]      # targets: fuzz/fuzz_targets/*.rs
#
# cargo-fuzz needs nightly only for its sanitizers (-Zsanitizer). These
# targets fuzz safe Rust parsers, where the bugs are panics (overflow, slice
# bounds, failed invariants), not memory errors, so the build uses the same
# coverage flags cargo-fuzz passes with `--sanitizer none`, plus debug
# assertions and overflow checks, and the pinned toolchain.
#
# A crash leaves its input in fuzz/artifacts/<target>/ and exits non-zero.
# Reproduce: fuzz/target/<triple>/release/<target> fuzz/artifacts/<target>/crash-*
set -euo pipefail

target="${1:?usage: scripts/fuzz.sh <target> [seconds]}"
seconds="${2:-300}"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root/fuzz"

triple="$(rustc -vV | sed -n 's/^host: //p')"
# --target keeps these flags off build scripts and proc macros.
RUSTFLAGS="--cfg fuzzing -Cpasses=sancov-module \
-Cllvm-args=-sanitizer-coverage-level=4 \
-Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
-Cllvm-args=-sanitizer-coverage-pc-table \
-Cllvm-args=-sanitizer-coverage-trace-compares \
-Cdebug-assertions -Coverflow-checks" \
  cargo build --release --target "$triple" --bin "$target"

mkdir -p "corpus/$target" "artifacts/$target"
exec "target/$triple/release/$target" \
  -max_total_time="$seconds" \
  -rss_limit_mb=2048 \
  -timeout=10 \
  -print_final_stats=1 \
  -artifact_prefix="artifacts/$target/" \
  "corpus/$target" "seeds/$target"
