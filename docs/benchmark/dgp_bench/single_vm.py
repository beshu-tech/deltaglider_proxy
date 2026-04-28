from __future__ import annotations

import os
import subprocess
from pathlib import Path

from .config import APP
from .hcloud_lifecycle import client

# Avoid hung sessions; keep proxy responsive on long benchmark runs.
# Ephemeral VMs reuse floating IPs; avoid stale keys in ~/.ssh/known_hosts for the same address.
_SSH_BASE = (
    "ssh",
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=30",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=8",
)
_SCP_BASE = (
    "scp",
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "ConnectTimeout=30",
    "-o",
    "ServerAliveInterval=30",
    "-o",
    "ServerAliveCountMax=8",
)


ACCESS_KEY = "bench"
SECRET_KEY = "bench-secret"
ENCRYPTION_KEY = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"


def smoke(args) -> int:
    server = _single_server(args.run_id)
    host = _server_ip(server)
    print(f"single VM: {server.name} {host}")
    _copy_package(host)
    _ssh(host, _remote_script(args))
    _download_results(host, args.run_id)
    print(f"results: docs/benchmark/results/{args.run_id}")
    return 0


def _single_server(run_id: str):
    matches = client().servers.get_all(label_selector=f"app={APP},run={run_id},role=single")
    if not matches:
        raise SystemExit(f"no single VM found for run {run_id!r}; create one with up --single-vm")
    if len(matches) > 1:
        raise SystemExit(f"expected one single VM for run {run_id!r}, found {len(matches)}")
    return matches[0]


def _server_ip(server) -> str:
    if not server.public_net or not server.public_net.ipv4:
        raise SystemExit(f"server {server.name} has no public IPv4")
    return server.public_net.ipv4.ip


def _ssh(host: str, command: str) -> None:
    subprocess.run([*_SSH_BASE, f"root@{host}", command], check=True)


def _copy_package(host: str) -> None:
    env = os.environ.copy()
    env.setdefault("COPYFILE_DISABLE", "1")
    subprocess.run(
        ["tar", "--no-xattrs", "--exclude=__pycache__", "-czf", "/tmp/dgp-benchmark-docs.tgz", "-C", "docs", "benchmark"],
        env=env,
        check=True,
    )
    subprocess.run(
        [*_SCP_BASE, "/tmp/dgp-benchmark-docs.tgz", f"root@{host}:/root/"],
        check=True,
    )


def _download_results(host: str, run_id: str) -> None:
    local_dir = Path("docs/benchmark/results")
    local_dir.mkdir(parents=True, exist_ok=True)
    remote = f"/root/dgp-benchmark/results/{run_id}.tgz"
    _ssh(host, f"cd /root/dgp-benchmark/results && tar -czf {run_id}.tgz {run_id}")
    subprocess.run(
        [*_SCP_BASE, f"root@{host}:{remote}", str(local_dir / f"{run_id}.tgz")],
        check=True,
    )


def _remote_script(args) -> str:
    rcmd = getattr(args, "resource_command", "") or (
        "bash /root/dgp-benchmark/benchmark/scripts/benchmark_resources_linux.sh"
    )
    rcmd_sh = rcmd.replace("'", "'\"'\"'")
    trials = int(getattr(args, "trials", 1) or 1)
    clean_raw = (getattr(args, "clean_command", "") or "").strip()
    clean_sh = clean_raw.replace("'", "'\"'\"'")
    opt_clean = f"  --clean-command '{clean_sh}' \\\n" if clean_raw else ""
    opt_allow_clean = "  --allow-clean-failure \\\n" if getattr(args, "allow_clean_failure", False) else ""
    opt_skip_verify = "  --skip-compression-verify \\\n" if getattr(args, "skip_compression_verify", False) else ""
    pgap = float(getattr(args, "phase_gap_seconds", 0.0) or 0.0)
    opt_phase_gap = f"  --phase-gap-seconds {pgap} \\\n" if pgap > 0 else ""
    opt_restart = (
        ""
        if getattr(args, "no_proxy_restart", False)
        else "  --restart-command 'docker restart dgp-bench >/dev/null' \\\n"
    )
    opt_restart_between = (
        ""
        if getattr(args, "no_restart_between_modes", False)
        else "  --restart-between-modes-command 'docker restart dgp-bench >/dev/null' \\\n"
    )
    return f"""set -euo pipefail
rm -rf /root/dgp-benchmark
mkdir -p /root/dgp-benchmark /root/dgp-single/plain /root/dgp-single/encrypted
chmod -R 0777 /root/dgp-single
tar -xzf /root/dgp-benchmark-docs.tgz -C /root/dgp-benchmark
cd /root/dgp-benchmark
if command -v apt-get >/dev/null 2>&1; then
  for _w in $(seq 1 120); do
    if ! pgrep -x apt-get >/dev/null 2>&1 && ! pgrep -x dpkg >/dev/null 2>&1; then
      break
    fi
    sleep 2
  done
  DEBIAN_FRONTEND=noninteractive apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq python3-venv python3-pip
fi
python3 -m venv .venv
. .venv/bin/activate
pip install -q -r benchmark/requirements.txt

cat >/root/dgp-single/deltaglider_proxy.yaml <<'YAML'
access:
  access_key_id: {ACCESS_KEY}
  secret_access_key: {SECRET_KEY}
advanced:
  # Only passthrough when encoded delta >= full object size (never cap below global default for benchmark).
  max_delta_ratio: 1.0
storage:
  default_backend: plain
  backends:
    - name: plain
      type: filesystem
      path: /data/plain
    - name: encrypted
      type: filesystem
      path: /data/encrypted
      encryption:
        mode: aes256-gcm-proxy
        key: "{ENCRYPTION_KEY}"
  buckets:
    bench-passthrough:
      backend: plain
      compression: false
    bench-compression:
      backend: plain
      compression: true
      max_delta_ratio: 1.0
    bench-encryption:
      backend: encrypted
      compression: false
    bench-compression-encryption:
      backend: encrypted
      compression: true
      max_delta_ratio: 1.0
YAML

docker rm -f dgp-bench >/dev/null 2>&1 || true
docker pull -q beshultd/deltaglider_proxy:latest
docker run -d --name dgp-bench \\
  -p 9000:9000 \\
  -e DGP_CONFIG=/data/deltaglider_proxy.yaml \\
  -e DGP_MAX_OBJECT_SIZE=1073741824 \\
  -v /root/dgp-single:/data \\
  beshultd/deltaglider_proxy:latest >/tmp/dgp-bench-container

for i in $(seq 1 60); do
  curl -fsS http://127.0.0.1:9000/_/health >/dev/null && break
  sleep 1
done
curl -fsS http://127.0.0.1:9000/_/health >/dev/null

python benchmark/bench_production_tax.py artifacts \\
  --artifact-count {args.artifact_count} \\
  --artifact-source {args.artifact_source} \\
  --artifact-extension {args.artifact_extension} \\
  --alpine-branch {args.alpine_branch} \\
  --alpine-arch {args.alpine_arch} \\
  --alpine-flavor {args.alpine_flavor} \\
  --data-dir /root/dgp-benchmark/data

export DGP_BENCH_ACCESS_KEY={ACCESS_KEY}
export DGP_BENCH_SECRET_KEY={SECRET_KEY}

python benchmark/bench_production_tax.py run \\
  --run-id {args.run_id} \\
  --proxy-endpoint http://127.0.0.1:9000 \\
  --data-dir /root/dgp-benchmark/data \\
  --reuse-artifacts \\
  --artifact-count {args.artifact_count} \\
  --artifact-source {args.artifact_source} \\
  --artifact-extension {args.artifact_extension} \\
  --alpine-branch {args.alpine_branch} \\
  --alpine-arch {args.alpine_arch} \\
  --alpine-flavor {args.alpine_flavor} \\
  --concurrency {args.concurrency} \\
  --metrics-url http://127.0.0.1:9000/_/metrics \\
  --stats-url http://127.0.0.1:9000/_/stats \\
  --health-url http://127.0.0.1:9000/_/health \\
  --resource-sample-interval 2 \\
  --resource-command '{rcmd_sh}' \\
{opt_restart}{opt_restart_between}  --trials {trials} \\
{opt_allow_clean}{opt_clean}{opt_skip_verify}{opt_phase_gap}  --results /root/dgp-benchmark/results
"""
