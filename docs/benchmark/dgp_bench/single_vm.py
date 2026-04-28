from __future__ import annotations

import subprocess
from pathlib import Path

from .hcloud_lifecycle import client


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
    matches = client().servers.get_all(label_selector=f"app=dgp-compression-tax-bench,run={run_id},role=single")
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
    subprocess.run(
        ["ssh", "-o", "StrictHostKeyChecking=no", "-o", "BatchMode=yes", f"root@{host}", command],
        check=True,
    )


def _copy_package(host: str) -> None:
    subprocess.run(
        "tar --no-xattrs --exclude='__pycache__' -czf /tmp/dgp-benchmark-docs.tgz -C docs benchmark",
        shell=True,
        check=True,
    )
    subprocess.run(
        ["scp", "-o", "StrictHostKeyChecking=no", "/tmp/dgp-benchmark-docs.tgz", f"root@{host}:/root/"],
        check=True,
    )


def _download_results(host: str, run_id: str) -> None:
    local_dir = Path("docs/benchmark/results")
    local_dir.mkdir(parents=True, exist_ok=True)
    remote = f"/root/dgp-benchmark/results/{run_id}.tgz"
    _ssh(host, f"cd /root/dgp-benchmark/results && tar -czf {run_id}.tgz {run_id}")
    subprocess.run(
        ["scp", "-o", "StrictHostKeyChecking=no", f"root@{host}:{remote}", str(local_dir / f"{run_id}.tgz")],
        check=True,
    )


def _remote_script(args) -> str:
    return f"""set -euo pipefail
rm -rf /root/dgp-benchmark
mkdir -p /root/dgp-benchmark /root/dgp-single/plain /root/dgp-single/encrypted
chmod -R 0777 /root/dgp-single
tar -xzf /root/dgp-benchmark-docs.tgz -C /root/dgp-benchmark
cd /root/dgp-benchmark
python3 -m venv .venv
. .venv/bin/activate
pip install -q -r benchmark/requirements.txt

cat >/root/dgp-single/deltaglider_proxy.yaml <<'YAML'
access:
  access_key_id: {ACCESS_KEY}
  secret_access_key: {SECRET_KEY}
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
    bench-encryption:
      backend: encrypted
      compression: false
    bench-compression-encryption:
      backend: encrypted
      compression: true
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
  --restart-command 'docker restart dgp-bench >/dev/null' \\
  --results /root/dgp-benchmark/results
"""
