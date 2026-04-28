from __future__ import annotations

import csv
import json
from dataclasses import asdict
from pathlib import Path
from typing import Any

from .config import MODE_ORDER
from .model import OpResult
from .util import percentile


def summarize_ops(rows: list[OpResult], *, phase_wall_s: float | None = None) -> dict[str, Any]:
    """Aggregates per-op results for one phase.

    ``mb_s`` is ``sum(bytes_ok) / sum(op_wall_seconds) / 1e6`` — useful as an average per-op
    rate when ops do not overlap; with concurrency > 1 it is **not** wall-clock phase throughput.

    When ``phase_wall_s`` is set (wall time around the whole phase), ``mb_s_wall`` is
    ``sum(bytes_ok) / phase_wall_s / 1e6`` — comparable across concurrency levels.
    """
    ok = [r for r in rows if r.ok]
    durations = [r.wall_s for r in ok]
    total_bytes = sum(r.bytes for r in ok)
    total_op_s = sum(durations)
    out: dict[str, Any] = {
        "ops": len(rows),
        "ok": len(ok),
        "failed": len(rows) - len(ok),
        "bytes": total_bytes,
        # Sum of successful op wall times (not phase wall clock); used with ``mb_s`` aggregate.
        "total_s": total_op_s,
        "throughput_aggregate": "sum_bytes_over_sum_op_wall_seconds",
        "mb_s": (total_bytes / 1_000_000 / total_op_s) if total_op_s else 0.0,
        "latency_sample_n": len(ok),
        "p50_ms": percentile(durations, 0.50) * 1000,
        "p95_ms": percentile(durations, 0.95) * 1000,
        "p99_ms": percentile(durations, 0.99) * 1000,
    }
    if phase_wall_s is not None and phase_wall_s > 0:
        out["phase_wall_s"] = phase_wall_s
        out["mb_s_wall"] = total_bytes / 1_000_000 / phase_wall_s
    else:
        out["phase_wall_s"] = None
        out["mb_s_wall"] = None
    return out


def write_rows(path: Path, rows: list[OpResult]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=list(asdict(rows[0]).keys()) if rows else ["empty"])
        writer.writeheader()
        for row in rows:
            writer.writerow(asdict(row))


def _mb_s_wall(phases: dict[str, Any], phase: str) -> float | None:
    p = phases.get(phase) or {}
    w = p.get("mb_s_wall")
    if w is not None:
        try:
            return float(w)
        except (TypeError, ValueError):
            pass
    return None


def write_markdown_report(path: Path, summary: dict[str, Any]) -> None:
    lines = ["# DeltaGlider compression/encryption tax report", ""]
    lines.append(f"Run ID: `{summary['run_id']}`")
    lines.append("")
    lines.append(
        "| Mode | Conc. | PUT MB/s (wall) | Cold GET MB/s (wall) | Warm GET MB/s (wall) | Failed ops |"
    )
    lines.append("|---|---:|---:|---:|---:|---:|")
    for mode in MODE_ORDER:
        by_conc = summary["modes"].get(mode)
        if not by_conc:
            continue
        for conc, phases in by_conc.items():
            failed = phases["put"]["failed"] + phases["cold_get"]["failed"] + phases["warm_get"]["failed"]

            def cell(phase: str) -> str:
                w = _mb_s_wall(phases, phase)
                if w is not None:
                    return f"{w:.2f}"
                p = phases.get(phase) or {}
                return f"{float(p.get('mb_s', 0.0)):.2f}"

            lines.append(
                f"| {mode} | {conc.removeprefix('c')} | "
                f"{cell('put')} | {cell('cold_get')} | {cell('warm_get')} | {failed} |"
            )
    lines.append("")
    lines.append(
        "Throughput uses **wall-clock phase** rates (`mb_s_wall`) when present — comparable across modes. "
        "Compare each row to **passthrough** for the same phase; relative slowdown ≈ "
        "`100 * (1 - mode_mb_s_wall / passthrough_mb_s_wall)` (%)."
    )
    path.write_text("\n".join(lines) + "\n")


def write_summary(path: Path, summary: dict[str, Any]) -> None:
    path.write_text(json.dumps(summary, indent=2) + "\n")
