"""Shared shapes and rollups for benchmark JSON summaries (runner + reports)."""

from __future__ import annotations

from typing import Any

# Phases recorded under each mode × concurrency in summary.json / CSV pipeline.
PHASE_NAMES: tuple[str, ...] = ("put", "cold_get", "warm_get")

# Fields rolled up per phase in summarize_ops / trial aggregates.
SUMMARY_PHASE_SCALAR_FIELDS: tuple[str, ...] = ("mb_s", "p50_ms", "p95_ms", "p99_ms")


def pick_primary_concurrency(by_conc: dict[str, Any]) -> str:
    """Prefer c1 for charts/tables; otherwise lowest sorted key."""
    return "c1" if "c1" in by_conc else sorted(by_conc.keys())[0]


def snap_mode_rollup_row(modes_block: dict[str, Any] | None) -> dict[str, Any]:
    """Single concurrency row from resources_rollup.modes[mode]."""
    if not modes_block:
        return {}
    return modes_block.get("c1") or modes_block[pick_primary_concurrency(modes_block)]


def count_phase_failures(summary: dict[str, Any]) -> int:
    """Sum failed op counts across all modes × concurrencies × phases."""
    return sum(
        phases[phase]["failed"]
        for by_conc in summary["modes"].values()
        for phases in by_conc.values()
        for phase in PHASE_NAMES
    )
