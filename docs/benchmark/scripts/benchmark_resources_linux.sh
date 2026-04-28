#!/usr/bin/env bash
# Emit JSON with Linux host + Docker + approximate backend disk footprint for benchmarks.
# Env:
#   DGP_BENCH_DATA_ROOT   default /root/dgp-single
#   DGP_BENCH_DOCKER_NAME default dgp-bench
set -euo pipefail
ROOT="${DGP_BENCH_DATA_ROOT:-/root/dgp-single}"
export DGP_BENCH_DATA_ROOT="$ROOT"
export DGP_BENCH_DOCKER_NAME="${DGP_BENCH_DOCKER_NAME:-dgp-bench}"
exec python3 - <<'PY'
from __future__ import annotations

import json
import os
import shutil
import subprocess
from pathlib import Path


def loadavg() -> str:
    p = Path("/proc/loadavg")
    return p.read_text().strip() if p.exists() else ""


def meminfo_kb(keys: list[str]) -> dict[str, int | None]:
    out: dict[str, int | None] = {k: None for k in keys}
    p = Path("/proc/meminfo")
    if not p.exists():
        return out
    for line in p.read_text().splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[0].endswith(":"):
            name = parts[0][:-1]
            if name in out:
                try:
                    out[name] = int(parts[1])
                except ValueError:
                    pass
    return out


def tree_walk_sum(root: Path) -> int:
    total = 0
    if not root.exists():
        return 0
    for fp in root.rglob("*"):
        if fp.is_file():
            try:
                total += fp.stat().st_size
            except OSError:
                pass
    return total


def du_sb(path: Path) -> int:
    du = shutil.which("du")
    if not du or not path.exists():
        return 0
    try:
        out = subprocess.check_output(["du", "-sb", str(path)], text=True, stderr=subprocess.DEVNULL)
        return int(out.split()[0])
    except Exception:
        return 0


def docker_stats(name: str) -> dict[str, str]:
    if not shutil.which("docker"):
        return {}
    try:
        subprocess.check_call(["docker", "inspect", name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except Exception:
        return {}
    fmt = '{"cpu_percent":"{{.CPUPerc}}","mem_usage":"{{.MemUsage}}","mem_percent":"{{.MemPerc}}","block_io":"{{.BlockIO}}","net_io":"{{.NetIO}}"}'
    try:
        line = subprocess.check_output(["docker", "stats", "--no-stream", "--format", fmt, name], text=True)
        return json.loads(line.strip())
    except Exception as e:
        return {"error": str(e)}


root = Path(os.environ.get("DGP_BENCH_DATA_ROOT", "/root/dgp-single"))
container = os.environ.get("DGP_BENCH_DOCKER_NAME", "dgp-bench")
plain = root / "plain"
enc = root / "encrypted"

doc = {
    "schema": "dgp-bench-resources/v1",
    "data_root": str(root),
    "loadavg": loadavg(),
    "meminfo_kb": meminfo_kb(["MemTotal", "MemAvailable", "SwapTotal", "SwapFree"]),
    "disk_backend_bytes_approx": {
        "plain_tree_walk_bytes": tree_walk_sum(plain),
        "encrypted_tree_walk_bytes": tree_walk_sum(enc),
        "plain_du_sb": du_sb(plain),
        "encrypted_du_sb": du_sb(enc),
    },
    "docker_stats": docker_stats(container),
}
print(json.dumps(doc, indent=2))
PY
