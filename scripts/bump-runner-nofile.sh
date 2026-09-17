#!/usr/bin/env bash
# Bump the open-file limit (nofile) on the self-hosted CI runners.
#
# WHY: the runners are Ryzen LXC containers (the `k3s` label is a historical
# alias — no Kubernetes). LXC inherits a low default nofile (~1024). The CI
# integration job runs ~35 test binaries in parallel, each with its own proxy +
# MinIO sockets, so the aggregate fd count crosses 1024 and fails as
# `ConnectError("tcp open error", Os code 24 "Too many open files")`.
# See docs/dev/ci-infra.md → "Open-file limit (nofile)".
#
# RUN THIS ON THE RYZEN HOST (not in CI, not on a dev laptop). It is idempotent.
#
# Usage:
#   sudo scripts/bump-runner-nofile.sh                 # auto-detect LXC containers
#   sudo scripts/bump-runner-nofile.sh ct1 ct2 ...     # explicit container names
#
# Supports plain LXC (/var/lib/lxc), Incus/LXD (`incus`/`lxc` CLI), and a
# fallback that edits the in-container actions-runner systemd unit.
set -euo pipefail

NOFILE="${NOFILE:-1048576}"

log() { printf '==> %s\n' "$*"; }
warn() { printf '!!  %s\n' "$*" >&2; }

[[ $EUID -eq 0 ]] || { warn "must run as root (sudo)"; exit 1; }

# ── 1. Discover containers ────────────────────────────────────────────────
containers=("$@")
if [[ ${#containers[@]} -eq 0 ]]; then
  if command -v lxc-ls >/dev/null 2>&1; then
    mapfile -t containers < <(lxc-ls -1 2>/dev/null || true)
  elif command -v incus >/dev/null 2>&1; then
    mapfile -t containers < <(incus list -c n --format csv 2>/dev/null || true)
  elif command -v lxc >/dev/null 2>&1; then
    mapfile -t containers < <(lxc list -c n --format csv 2>/dev/null || true)
  fi
fi
[[ ${#containers[@]} -gt 0 ]] || { warn "no containers found — pass names explicitly"; exit 1; }
log "Target containers: ${containers[*]}  (nofile=${NOFILE})"

# ── 2. Per-container limit bump ───────────────────────────────────────────
for ct in "${containers[@]}"; do
  [[ -n "$ct" ]] || continue
  log "Container: $ct"

  cfg="/var/lib/lxc/$ct/config"
  if [[ -f "$cfg" ]]; then
    # Plain LXC: set lxc.prlimit.nofile (idempotent: drop any prior line first).
    sed -i '/^lxc\.prlimit\.nofile/d' "$cfg"
    printf 'lxc.prlimit.nofile = %s\n' "$NOFILE" >> "$cfg"
    log "  set lxc.prlimit.nofile in $cfg (restart container to apply)"
  elif command -v incus >/dev/null 2>&1; then
    incus config set "$ct" limits.kernel.nofile "$NOFILE" \
      && log "  set incus limits.kernel.nofile=$NOFILE (restart container to apply)"
  elif command -v lxc >/dev/null 2>&1; then
    lxc config set "$ct" limits.kernel.nofile "$NOFILE" \
      && log "  set lxd limits.kernel.nofile=$NOFILE (restart container to apply)"
  else
    warn "  no LXC/Incus/LXD config path for $ct — bump the in-container runner unit manually:"
    warn "    systemctl edit actions-runner  →  [Service]\\n    LimitNOFILE=$NOFILE"
  fi

  # Also pin the in-container actions-runner systemd unit if we can reach it.
  # (Belt-and-suspenders: the host prlimit caps the ceiling, the unit raises the
  # soft limit for the runner process itself.)
  runner_exec="lxc-attach -n $ct --"
  command -v incus >/dev/null 2>&1 && runner_exec="incus exec $ct --"
  if $runner_exec true >/dev/null 2>&1; then
    $runner_exec bash -lc '
      set -e
      if systemctl list-unit-files 2>/dev/null | grep -q "^actions-runner"; then
        mkdir -p /etc/systemd/system/actions-runner.service.d
        printf "[Service]\nLimitNOFILE='"$NOFILE"'\n" \
          > /etc/systemd/system/actions-runner.service.d/nofile.conf
        systemctl daemon-reload
        systemctl restart actions-runner || true
        echo "    in-container actions-runner LimitNOFILE pinned + restarted"
      else
        echo "    (no actions-runner systemd unit inside; skipping in-container unit)"
      fi
    ' || warn "  could not pin in-container runner unit for $ct"
  fi
done

log "Done. Restart any container whose host-level limit changed:"
log "  lxc-stop -n <ct> && lxc-start -n <ct>   (or: incus restart <ct>)"
log "Verify from a CI job step:  ulimit -Sn; ulimit -Hn; cat /proc/sys/fs/file-max"
