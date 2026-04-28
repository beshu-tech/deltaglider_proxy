from __future__ import annotations

import json
import subprocess
import time
import urllib.request
from pathlib import Path
from typing import Any

from .config import utc_now
from .prom_summary import summarize_prometheus


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


def run_resource_command(command: str) -> Any | None:
    cmd = (command or "").strip()
    if not cmd:
        return None
    try:
        proc = subprocess.run(
            cmd,
            shell=True,
            text=True,
            capture_output=True,
            timeout=60,
            check=False,
        )
        raw = (proc.stdout or "").strip()
        if raw:
            try:
                return json.loads(raw)
            except json.JSONDecodeError:
                return {
                    "error": "resource_command stdout was not JSON",
                    "stdout": raw[:8000],
                    "stderr": (proc.stderr or "")[:8000],
                    "returncode": proc.returncode,
                }
        return {
            "error": "resource_command produced empty stdout",
            "stderr": (proc.stderr or "")[:8000],
            "returncode": proc.returncode,
        }
    except Exception as e:
        return {"error": str(e)}


def collect_resource_sample(args, prom_text: str = "") -> dict[str, Any]:
    health_url = getattr(args, "health_url", None) or None
    health = fetch_json(health_url) if health_url else None
    prom_summary = summarize_prometheus(prom_text) if prom_text else {}
    if not prom_text and getattr(args, "metrics_url", None):
        prom_summary = summarize_prometheus(fetch_text(args.metrics_url))
    host_resources = run_resource_command(getattr(args, "resource_command", None) or "")
    return {
        "timestamp": utc_now(),
        "health": health,
        "prom_summary": prom_summary,
        "host_resources": host_resources,
    }


def snapshot_proxy(args, name: str, out_dir: Path) -> dict[str, str]:
    out_dir.mkdir(parents=True, exist_ok=True)
    prom_text = ""
    if args.metrics_url:
        prom_text = fetch_text(args.metrics_url)
        (out_dir / f"{name}.prom").write_text(prom_text)

    sample = collect_resource_sample(args, prom_text)

    data = {
        "timestamp": sample["timestamp"],
        "health": sample["health"],
        "stats": fetch_json(args.stats_url) if args.stats_url else None,
        "metrics_path": str(out_dir / f"{name}.prom") if args.metrics_url else None,
        "prom_summary": sample["prom_summary"],
        "host_resources": sample["host_resources"],
    }
    (out_dir / f"{name}.json").write_text(json.dumps(data, indent=2) + "\n")
    return {k: str(v) for k, v in data.items() if v is not None}


def restart_proxy_command(command: str | None, args: Any) -> None:
    """Run optional shell (e.g. docker restart) then wait for ``health_url`` if configured."""
    cmd = (command or "").strip()
    if not cmd:
        return
    subprocess.run(cmd, shell=True, check=True)
    health_url = getattr(args, "health_url", None)
    if health_url:
        timeout_s = float(getattr(args, "restart_timeout", 120.0) or 120.0)
        deadline = time.time() + timeout_s
        while time.time() < deadline:
            try:
                with urllib.request.urlopen(health_url, timeout=3) as resp:
                    if resp.status == 200:
                        return
            except Exception:
                time.sleep(1)
        raise RuntimeError("proxy did not become healthy after restart")


def maybe_restart_proxy(args: Any) -> None:
    restart_proxy_command(getattr(args, "restart_command", None), args)
