"""Human-readable / HTML formatting helpers for benchmark reports (no I/O)."""

from __future__ import annotations

import re
from typing import Any


def html_escape(s: str) -> str:
    return (
        s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&#39;")
    )


def fmt_bytes(n: float | int | None) -> str:
    if n is None:
        return "—"
    n = float(n)
    if n < 1_000_000:
        return f"{n / 1_000:.1f} KB"
    if n < 1_000_000_000:
        return f"{n / 1_000_000:.1f} MB"
    return f"{n / 1_000_000_000:.2f} GB"


def fmt_gb(gb: float) -> str:
    if gb < 1.0:
        return f"{gb * 1000:.1f} MB"
    return f"{gb:.2f} GB"


def fmt_agg_cell(x: Any, digits: int = 3) -> str:
    if x is None:
        return "—"
    try:
        return f"{float(x):.{digits}f}"
    except (TypeError, ValueError):
        return "—"


def parse_percent(value: Any) -> float | None:
    if value is None:
        return None
    s = str(value).strip().replace("%", "")
    try:
        return float(s)
    except ValueError:
        return None


def docker_used_bytes_to_chart_mb(used_bytes: float) -> float:
    """Chart Y axis is labeled MB; treat MiB≈MB for readability."""
    return used_bytes / (1024.0 * 1024.0)


def parse_docker_mem_used_mib(value: Any) -> float | None:
    """Parse left side of docker stats MemUsage (e.g. '420 MiB / 16 GiB' or '420MiB / 16GiB')."""
    if not value:
        return None
    used = str(value).split("/", 1)[0].strip()
    parts = used.split()
    amount_raw: str | None = None
    unit: str | None = None
    if len(parts) == 2:
        amount_raw, unit = parts[0], parts[1]
    else:
        m = re.fullmatch(r"([\d.]+)\s*(MiB|GiB|KiB|TiB|MB|GB|KB|TB|B)", used.replace(" ", ""), flags=re.I)
        if not m:
            return None
        amount_raw, unit = m.group(1), m.group(2)
    try:
        amount = float(amount_raw)
    except ValueError:
        return None
    assert unit is not None
    u = unit.lower()
    if u in {"mib"}:
        return docker_used_bytes_to_chart_mb(amount * 1024.0 * 1024.0)
    if u in {"gib"}:
        return docker_used_bytes_to_chart_mb(amount * 1024.0 * 1024.0 * 1024.0)
    if u in {"kib"}:
        return docker_used_bytes_to_chart_mb(amount * 1024.0)
    if u in {"tib"}:
        return docker_used_bytes_to_chart_mb(amount * 1024.0**4)
    if u == "mb":
        return amount
    if u == "gb":
        return amount * 1000.0
    if u == "kb":
        return amount / 1000.0
    if u == "tb":
        return amount * 1_000_000.0
    if u == "b":
        return amount / 1_000_000.0
    return None
