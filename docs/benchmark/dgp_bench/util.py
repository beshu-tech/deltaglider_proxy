from __future__ import annotations

import hashlib
import math
import os
import socket
import subprocess
from pathlib import Path
from typing import Any

from .config import utc_now


def percentile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    idx = min(len(ordered) - 1, max(0, math.ceil((len(ordered) - 1) * q)))
    return ordered[idx]


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def run_text(cmd: list[str]) -> str:
    try:
        return subprocess.check_output(cmd, text=True, stderr=subprocess.STDOUT).strip()
    except Exception as e:
        return f"ERROR: {e}"


def capture_local_environment() -> dict[str, Any]:
    return {
        "timestamp": utc_now(),
        "hostname": socket.gethostname(),
        "nproc": os.cpu_count(),
        "uname": run_text(["uname", "-a"]),
        "lscpu": run_text(["lscpu"]),
        "meminfo": Path("/proc/meminfo").read_text() if Path("/proc/meminfo").exists() else "",
    }
