"""Post-run checks that Prometheus shows delta activity for benchmark modes where compression is on."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

from .bench_bundle import parse_prom_counter

# Default benchmark buckets run these modes with delta compression enabled (see README / single_vm YAML).
EXPECTED_DELTA_MODES = frozenset({"compression", "compression_encryption"})


def verify_compression_modes_recorded_delta(result_dir: Path, summary: dict[str, Any], args: Any) -> None:
    """
    After a successful run, require that each **completed** compression segment increased either
    ``deltaglider_delta_bytes_saved_total`` or ``deltaglider_delta_encode_duration_seconds_count``
    between before/after scrapes. Otherwise exit via ``SystemExit`` (misconfigured ratio, no metrics, etc.).
    """
    if getattr(args, "skip_compression_verify", False):
        return
    if os.environ.get("DGP_BENCH_SKIP_COMPRESSION_VERIFY", "").strip().lower() in ("1", "true", "yes"):
        return

    if not getattr(args, "metrics_url", None):
        raise SystemExit(
            "compression verification needs Prometheus snapshots from --metrics-url. "
            "Set --metrics-url or pass --skip-compression-verify / DGP_BENCH_SKIP_COMPRESSION_VERIFY=1."
        )

    modes_block = summary.get("modes") or {}
    expected_here = sorted(EXPECTED_DELTA_MODES & set(modes_block.keys()))
    if not expected_here:
        return

    errors: list[str] = []
    for mode in expected_here:
        by_conc = modes_block.get(mode) or {}
        for ck in sorted(by_conc.keys(), key=lambda x: (len(x), x)):
            mode_dir = result_dir / mode / ck
            before_name = f"before_{mode}_{ck}.prom"
            after_name = f"after_{mode}_{ck}.prom"
            bf, af = mode_dir / before_name, mode_dir / after_name
            if not bf.is_file() or not af.is_file():
                errors.append(f"{mode}/{ck}: missing {before_name!r} or {after_name!r} under {mode_dir}")
                continue
            btxt = bf.read_text(encoding="utf-8", errors="replace")
            atxt = af.read_text(encoding="utf-8", errors="replace")
            saved_b = parse_prom_counter(btxt, "deltaglider_delta_bytes_saved_total") or 0.0
            saved_a = parse_prom_counter(atxt, "deltaglider_delta_bytes_saved_total") or 0.0
            enc_b = parse_prom_counter(btxt, "deltaglider_delta_encode_duration_seconds_count") or 0.0
            enc_a = parse_prom_counter(atxt, "deltaglider_delta_encode_duration_seconds_count") or 0.0
            d_saved = saved_a - saved_b
            d_enc = enc_a - enc_b
            if d_saved <= 0.0 and d_enc <= 0.0:
                errors.append(
                    f"{mode}/{ck}: no delta progress (Δ bytes_saved={d_saved}, Δ encode_count={d_enc}); "
                    "expected compression with delta-eligible artifacts — check max_delta_ratio, metrics scrapes, PUT order."
                )

    if errors:
        raise SystemExit(
            "compression verification failed — compression modes must show Prometheus delta counters advancing:\n"
            + "\n".join(f"  - {e}" for e in errors)
        )


