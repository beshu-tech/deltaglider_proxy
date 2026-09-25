#!/usr/bin/env bash
# Run a command and log memory and free disk every 10 s.
#
# WHY: when a runner runs out of memory, the kernel SIGKILLs the build
# (exit 137) or the whole runner, and the log shows no reason. These lines
# stream to the step log until the kill, so the last one shows which limit was
# hit: the cgroup limit (cgroup near max), the host (host_available near 0),
# or the disk (disk_free near 0).
#
# Usage: scripts/ci-memwatch.sh <command> [args...]
set -euo pipefail

# cgroup v2 first, then v1.
if [[ -r /sys/fs/cgroup/memory.current ]]; then
  cur_file=/sys/fs/cgroup/memory.current
  max_file=/sys/fs/cgroup/memory.max
  peak_file=/sys/fs/cgroup/memory.peak
else
  cur_file=/sys/fs/cgroup/memory/memory.usage_in_bytes
  max_file=/sys/fs/cgroup/memory/memory.limit_in_bytes
  peak_file=/sys/fs/cgroup/memory/memory.max_usage_in_bytes
fi

mib() { local v; v=$(cat "$1" 2>/dev/null || echo "?"); [[ $v =~ ^[0-9]+$ ]] && echo "$((v / 1048576))MiB" || echo "$v"; }
host() { awk -v k="$1:" '$1 == k { printf "%dMiB", $2 / 1024 }' /proc/meminfo; }

echo "[memwatch] cgroup_max=$(mib "$max_file") host_total=$(host MemTotal)"
(
  while :; do
    echo "[memwatch] $(date -u +%T) cgroup=$(mib "$cur_file") host_available=$(host MemAvailable) disk_free=$(df -m --output=avail . | tail -1 | tr -d ' ')MiB"
    sleep 10
  done
) &
sampler=$!
trap 'kill "$sampler" 2>/dev/null || true; echo "[memwatch] cgroup_peak=$(mib "$peak_file")"' EXIT

"$@"
