from __future__ import annotations

import csv
import json
from dataclasses import asdict
from pathlib import Path
from typing import Any

from .model import OpResult
from .util import percentile


def summarize_ops(rows: list[OpResult]) -> dict[str, Any]:
    ok = [r for r in rows if r.ok]
    durations = [r.wall_s for r in ok]
    total_bytes = sum(r.bytes for r in ok)
    total_s = sum(durations)
    return {
        "ops": len(rows),
        "ok": len(ok),
        "failed": len(rows) - len(ok),
        "bytes": total_bytes,
        "total_s": total_s,
        "mb_s": (total_bytes / 1_000_000 / total_s) if total_s else 0.0,
        "p50_ms": percentile(durations, 0.50) * 1000,
        "p95_ms": percentile(durations, 0.95) * 1000,
        "p99_ms": percentile(durations, 0.99) * 1000,
    }


def write_rows(path: Path, rows: list[OpResult]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(asdict(rows[0]).keys()) if rows else ["empty"])
        writer.writeheader()
        for row in rows:
            writer.writerow(asdict(row))


def write_markdown_report(path: Path, summary: dict[str, Any]) -> None:
    lines = ["# DeltaGlider compression/encryption tax report", ""]
    lines.append(f"Run ID: `{summary['run_id']}`")
    lines.append("")
    lines.append("| Mode | Conc. | PUT MB/s | Cold GET MB/s | Warm GET MB/s | Failed ops |")
    lines.append("|---|---:|---:|---:|---:|---:|")
    for mode, by_conc in summary["modes"].items():
        for conc, phases in by_conc.items():
            failed = phases["put"]["failed"] + phases["cold_get"]["failed"] + phases["warm_get"]["failed"]
            lines.append(
                f"| {mode} | {conc.removeprefix('c')} | "
                f"{phases['put']['mb_s']:.2f} | {phases['cold_get']['mb_s']:.2f} | "
                f"{phases['warm_get']['mb_s']:.2f} | {failed} |"
            )
    lines.append("")
    lines.append("Interpretation must compare these rows against the passthrough mode for the same concurrency.")
    path.write_text("\n".join(lines) + "\n")


def write_summary(path: Path, summary: dict[str, Any]) -> None:
    path.write_text(json.dumps(summary, indent=2) + "\n")
