"""Aggregate metrics across independent benchmark trials (percentiles over runs)."""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any

from .bench_summary import PHASE_NAMES, SUMMARY_PHASE_SCALAR_FIELDS
from .config import MODE_ORDER
from .util import percentile


def _num_vals(summary: dict[str, Any], mode: str, conc: str, phase: str, field: str) -> float | None:
    try:
        v = summary["modes"][mode][conc][phase][field]
        if v is None:
            return None
        x = float(v)
        if math.isnan(x):
            return None
        return x
    except (KeyError, TypeError, ValueError):
        return None


def _stat(values: list[float]) -> dict[str, Any]:
    v = [x for x in values if x is not None and not math.isnan(x)]
    if not v:
        return {"n": 0, "mean": None, "p05": None, "p50": None, "p95": None, "min": None, "max": None, "values": []}
    return {
        "n": len(v),
        "mean": sum(v) / len(v),
        "p05": percentile(v, 0.05),
        "p50": percentile(v, 0.50),
        "p95": percentile(v, 0.95),
        "min": min(v),
        "max": max(v),
        "values": v,
    }


def collect_trial_summaries(parent_dir: Path) -> tuple[list[str], list[dict[str, Any]]]:
    """Return (trial_dir_names, summary dicts) sorted by trial folder name."""
    trials = sorted(p for p in parent_dir.iterdir() if p.is_dir() and p.name.startswith("trial_"))
    out: list[dict[str, Any]] = []
    names: list[str] = []
    for p in trials:
        sp = p / "summary.json"
        if not sp.exists():
            continue
        names.append(p.name)
        out.append(json.loads(sp.read_text()))
    return names, out


def build_trial_aggregate(parent_dir: Path, trials_expected: int | None = None) -> dict[str, Any]:
    """
    Load trial_*/summary.json under parent_dir and compute distribution of run-level metrics
    (throughput mb/s and latency percentiles from each trial's summarize_ops).
    """
    trial_names, summaries = collect_trial_summaries(parent_dir)
    completed = len(summaries)
    expected = trials_expected if trials_expected is not None else completed
    if trials_expected is not None and completed != trials_expected:
        raise ValueError(
            f"trial aggregate mismatch: expected {trials_expected} trial summaries, found {completed} "
            f"(trial_directories with summary.json: {trial_names})"
        )

    base_meta = {
        "trial_count_completed": completed,
        "trial_count_expected": expected,
        "trial_directories": trial_names,
    }

    if completed < 2:
        return {
            "schema": "dgp-bench-trial-aggregate/v1",
            "trial_count": completed,
            **base_meta,
            "note": "Need at least 2 trials for cross-trial percentiles.",
            "modes": {},
        }

    modes_out: dict[str, Any] = {}
    for mode in MODE_ORDER:
        modes_out.setdefault(mode, {})
        # assume c1 primary; merge all conc keys from first summary
        conc_keys = set()
        for s in summaries:
            m = s.get("modes") or {}
            if mode in m:
                conc_keys.update(m[mode].keys())
        for conc in sorted(conc_keys):
            modes_out[mode][conc] = {}
            for phase in PHASE_NAMES:
                modes_out[mode][conc][phase] = {}
                for field in SUMMARY_PHASE_SCALAR_FIELDS:
                    vals = [_num_vals(s, mode, conc, phase, field) for s in summaries]
                    vals_f = [float(x) for x in vals if x is not None]
                    modes_out[mode][conc][phase][field] = _stat(vals_f)

    return {
        "schema": "dgp-bench-trial-aggregate/v1",
        "trial_count": completed,
        **base_meta,
        "modes": modes_out,
    }


def write_aggregate(parent_dir: Path, data: dict[str, Any]) -> None:
    (parent_dir / "aggregate.json").write_text(json.dumps(data, indent=2) + "\n")


def _md_float(x: float | None, digits: int = 4) -> str:
    if x is None:
        return "—"
    return f"{x:.{digits}f}"


def write_aggregate_markdown(parent_dir: Path, data: dict[str, Any]) -> None:
    lines = [
        "# Cross-trial aggregate",
        "",
        f"Trials: **{data.get('trial_count', 0)}** (`trial_*` directories)",
        "",
        "Run-level distributions: each cell aggregates **one number per trial** (that trial's `mb_s` or latency percentile from `summarize_ops`).",
        "",
        "| Mode | Conc | Phase | Metric | n | mean | p05 | p50 | p95 | min | max |",
        "|---|---:|---|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    modes = data.get("modes") or {}
    for mode in MODE_ORDER:
        if mode not in modes:
            continue
        for conc, by_ph in modes[mode].items():
            for phase, metrics in by_ph.items():
                if not isinstance(metrics, dict):
                    continue
                for field, st in metrics.items():
                    if not isinstance(st, dict) or not st.get("n"):
                        continue
                    lines.append(
                        "| "
                        f"{mode} | {conc} | {phase} | {field} | {st['n']} | "
                        f"{_md_float(st.get('mean'))} | {_md_float(st.get('p05'))} | {_md_float(st.get('p50'))} | "
                        f"{_md_float(st.get('p95'))} | {_md_float(st.get('min'))} | {_md_float(st.get('max'))} |"
                    )
    lines.append("")
    (parent_dir / "aggregate.md").write_text("\n".join(lines) + "\n")
