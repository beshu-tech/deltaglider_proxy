from __future__ import annotations

import json
from pathlib import Path
from typing import Any


def build_resources_rollup(result_dir: Path) -> dict[str, Any]:
    """Aggregate health + prom RSS + host snapshot from each mode's final `after_*.json`."""
    modes: dict[str, Any] = {}
    for path in sorted(result_dir.glob("*/*/after_*.json")):
        mode = path.parts[-3]
        conc = path.parts[-2]
        try:
            data = json.loads(path.read_text())
        except Exception as e:
            modes.setdefault(mode, {})[conc] = {"error": str(e)}
            continue
        ps = data.get("prom_summary") or {}
        modes.setdefault(mode, {})[conc] = {
            "health": data.get("health"),
            "process_peak_rss_bytes_prom": ps.get("process_peak_rss_bytes"),
            "cache_size_bytes_prom": ps.get("deltaglider_cache_size_bytes"),
            "delta_encode_seconds_sum": ps.get("deltaglider_delta_encode_duration_seconds_sum"),
            "delta_decode_seconds_sum": ps.get("deltaglider_delta_decode_duration_seconds_sum"),
            "host_resources": data.get("host_resources"),
        }
    return {"modes": modes}
