from __future__ import annotations

import json
import time
import urllib.request
from pathlib import Path
from typing import Any

from .config import utc_now


def fetch_json(url: str) -> Any | None:
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def fetch_text(url: str) -> str:
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            return resp.read().decode("utf-8", "replace")
    except Exception as e:
        return f"# fetch error: {e}\n"


def snapshot_proxy(args, name: str, out_dir: Path) -> dict[str, str]:
    out_dir.mkdir(parents=True, exist_ok=True)
    data = {
        "timestamp": utc_now(),
        "stats": fetch_json(args.stats_url) if args.stats_url else None,
        "metrics_path": str(out_dir / f"{name}.prom"),
    }
    if args.metrics_url:
        (out_dir / f"{name}.prom").write_text(fetch_text(args.metrics_url))
    (out_dir / f"{name}.json").write_text(json.dumps(data, indent=2) + "\n")
    return {k: str(v) for k, v in data.items() if v is not None}


def maybe_restart_proxy(args) -> None:
    if not args.restart_command:
        return
    import subprocess

    subprocess.run(args.restart_command, shell=True, check=True)
    if args.health_url:
        deadline = time.time() + args.restart_timeout
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(args.health_url, timeout=3) as resp:
                    if resp.status == 200:
                        return
            except Exception:
                time.sleep(1)
        raise RuntimeError("proxy did not become healthy after restart")
