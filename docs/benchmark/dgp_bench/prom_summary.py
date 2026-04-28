from __future__ import annotations


def summarize_prometheus(text: str) -> dict[str, float]:
    """
    Extract scalar Prometheus samples (no labels in the metric name token)
    plus histogram _sum / _count lines (also unlabeled).

    Labeled counter/histogram bucket lines are skipped here on purpose — Grafana
    still has the raw *.prom export for full fidelity.
    """
    out: dict[str, float] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) < 2:
            continue
        name = parts[0]
        if "{" in name:
            continue
        try:
            val = float(parts[1])
        except ValueError:
            continue
        out[name] = val
    return out
