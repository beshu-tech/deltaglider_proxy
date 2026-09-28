#!/usr/bin/env bash
# Truth table for scripts/cargo-jobs.sh (memory-derived cargo parallelism).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
GiB=1073741824
fail=0
case_() { # name, cpus, memory.max (or "-"), v1 limit (or "-"), MemAvailable KiB (or "-"), expected
    local name="$1" cpus="$2" v2="$3" v1="$4" avail="$5" want="$6" d="$T/$1"
    mkdir -p "$d/cg/memory"
    [[ "$v2" != "-" ]] && echo "$v2" > "$d/cg/memory.max"
    [[ "$v1" != "-" ]] && echo "$v1" > "$d/cg/memory/memory.limit_in_bytes"
    if [[ "$avail" != "-" ]]; then printf 'MemTotal: 999999999 kB\nMemAvailable: %s kB\n' "$avail" > "$d/meminfo"; else : > "$d/meminfo"; fi
    local got
    got="$(CARGO_JOBS_CGROUP_ROOT="$d/cg" CARGO_JOBS_MEMINFO="$d/meminfo" CARGO_JOBS_NPROC="$cpus" "$ROOT/scripts/cargo-jobs.sh" 2>/dev/null)"
    if [[ "$got" != "$want" ]]; then echo "FAIL $name: got $got, want $want" >&2; fail=1; else echo "ok $name ($got)"; fi
}
case_ v2-limit-8g        16 $((8*GiB))  -              $((64*1024*1024))  4
case_ v2-max-uses-avail  16 max         -              $((12*1024*1024))  6
case_ v1-nolimit-sentinel 16 -          9223372036854771712 $((6*1024*1024)) 3
case_ v1-limit-wins      16 -           $((4*GiB))     $((64*1024*1024))  2
case_ cpus-cap           2  $((64*GiB)) -              $((64*1024*1024))  2
case_ tiny-budget-is-1   8  $((1*GiB))  -              -                  1
case_ no-info-uses-cpus  6  -           -              -                  6
exit $fail
