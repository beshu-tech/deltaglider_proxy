#!/usr/bin/env bash
# Print how many parallel rustc jobs this machine can afford by MEMORY, not
# by CPU count. cargo alone starts one job per CPU it sees and never looks at
# memory, so a build container with many cores and a modest memory limit is
# killed mid-build (the v2.0.1 amd64 image job lost its runner that way).
#
#   CARGO_BUILD_JOBS="$(scripts/cargo-jobs.sh)" cargo build --release
#
# Measured on a clean release build (2026-09-28): the final crate
# `deltaglider_proxy` peaks at 3.5 GiB and compiles alone; among the
# dependencies, aws_sdk_s3 peaks at 2.0 GiB, s3s at 1.0 GiB and the rest at
# 0.5 GiB or less. So each job gets JOB_MIB (2 GiB), and a budget under
# FLOOR_MIB (4 GiB) cannot build the final crate even with one job: warn.
#
# The budget is the smallest of: the cgroup v2 memory.max, the cgroup v1
# memory.limit_in_bytes, and MemAvailable. Paths can be overridden for tests:
# CARGO_JOBS_CGROUP_ROOT, CARGO_JOBS_MEMINFO, CARGO_JOBS_NPROC.
set -euo pipefail

JOB_MIB="${CARGO_JOBS_PER_JOB_MIB:-2048}"
FLOOR_MIB=4096
CG="${CARGO_JOBS_CGROUP_ROOT:-/sys/fs/cgroup}"
MEMINFO="${CARGO_JOBS_MEMINFO:-/proc/meminfo}"
CPUS="${CARGO_JOBS_NPROC:-$(nproc 2>/dev/null || echo 1)}"

budget_mib=""
consider() { # value in bytes, or "max"/empty (no limit)
    local b="$1"
    [[ "$b" =~ ^[0-9]+$ ]] || return 0
    # cgroup v1 reports "no limit" as a huge number; treat above 1 PiB as none.
    (( b > 1125899906842624 )) && return 0
    local m=$(( b / 1048576 ))
    if [[ -z "$budget_mib" ]] || (( m < budget_mib )); then budget_mib=$m; fi
}
[[ -r "$CG/memory.max" ]] && consider "$(tr -d '[:space:]' < "$CG/memory.max")"
[[ -r "$CG/memory/memory.limit_in_bytes" ]] && consider "$(tr -d '[:space:]' < "$CG/memory/memory.limit_in_bytes")"
if [[ -r "$MEMINFO" ]]; then
    avail_kib="$(awk '/^MemAvailable:/ {print $2}' "$MEMINFO")"
    [[ -n "$avail_kib" ]] && consider "$(( avail_kib * 1024 ))"
fi

if [[ -z "$budget_mib" ]]; then
    echo "$CPUS"
    exit 0
fi
if (( budget_mib < FLOOR_MIB )); then
    echo "cargo-jobs: only ${budget_mib} MiB of memory; the final crate alone needs about 3.5 GiB, so the build may be killed" >&2
fi
jobs=$(( budget_mib / JOB_MIB ))
(( jobs < 1 )) && jobs=1
(( jobs > CPUS )) && jobs=$CPUS
echo "cargo-jobs: ${jobs} jobs (${budget_mib} MiB, ${CPUS} CPUs)" >&2
echo "$jobs"
