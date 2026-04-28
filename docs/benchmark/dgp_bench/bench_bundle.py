"""Load benchmark result bundles (directory or .tgz) and derive Prometheus-derived metrics."""

from __future__ import annotations

import json
import tarfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .bench_report_format import parse_percent
from .bench_summary import pick_primary_concurrency, snap_mode_rollup_row
from .config import MODE_BAND_COLORS, MODE_ORDER, MODE_SHORT_LABELS


def parse_prom_counter(blob: str, metric: str) -> float | None:
    """Return last numeric token on an unlabeled PROM exposition line."""
    for raw in blob.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        token = line.split()[0]
        if token == metric:
            try:
                return float(line.split()[1])
            except (IndexError, ValueError):
                return None
    return None


@dataclass(frozen=True)
class ModeMetrics:
    mode: str
    delta_saved_increment: float
    encode_count_delta: float
    # Raw Prom delta before max(0,…); negative usually means counter reset or scrape mismatch.
    delta_saved_raw: float
    prom_delta_clamped: bool


def load_bundle(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]], dict[str, Any] | None]:
    """Return (summary, artifacts, raw_prom_by_mode_conc, resource_timeseries)."""
    if path.is_file() and path.suffix == ".tgz":
        return _load_from_tgz(path)
    if path.is_dir():
        return _load_from_dir(path)
    raise SystemExit(f"Expected directory or .tgz bundle, got {path}")


def _load_from_dir(root: Path) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]], dict[str, Any] | None]:
    summary = json.loads((root / "summary.json").read_text())
    artifacts = json.loads((root / "artifacts.json").read_text())
    prom_root = root
    if summary.get("multi_trial") and summary.get("primary_trial_dir"):
        prom_root = root / str(summary["primary_trial_dir"])
    prom = _gather_prom(prom_root)
    ts_path = root / str(summary.get("resource_timeseries_path") or "resource_timeseries.json")
    timeseries = json.loads(ts_path.read_text()) if ts_path.exists() else None
    return summary, artifacts, prom, timeseries


def _load_from_tgz(tgz: Path) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]], dict[str, Any] | None]:
    root_name = None
    with tarfile.open(tgz, "r:gz") as tf:
        for m in tf.getmembers():
            if m.name.endswith("/summary.json"):
                root_name = Path(m.name).parts[0]
                break
        if not root_name:
            raise SystemExit("Could not locate summary.json inside archive")

        def read_member(rel: str) -> str:
            member = tf.extractfile(f"{root_name}/{rel}")
            if member is None:
                raise SystemExit(f"missing {rel} in archive")
            return member.read().decode()

        summary = json.loads(read_member("summary.json"))
        artifacts = json.loads(read_member("artifacts.json"))
        primary = summary.get("primary_trial_dir") if summary.get("multi_trial") else None
        prom_candidates: dict[tuple[str, str, str], list[tuple[tarfile.TarInfo, str]]] = defaultdict(list)
        for m in tf.getmembers():
            if not m.isfile():
                continue
            parts = Path(m.name).parts
            if parts[0] != root_name:
                continue
            if primary:
                if len(parts) < 5 or parts[1] != primary:
                    continue
                mode, conc, fname = parts[2], parts[3], parts[4]
            else:
                if len(parts) < 4:
                    continue
                mode, conc, fname = parts[1], parts[2], parts[3]
            if not fname.endswith(".prom"):
                continue
            kind = "before" if fname.startswith("before_") else "after" if fname.startswith("after_") else ""
            if not kind:
                continue
            prom_candidates[(mode, conc, kind)].append((m, m.name))

        prom: dict[str, dict[str, dict[str, Any]]] = {}
        for (mode, conc, kind), items in prom_candidates.items():
            best_member, _best_name = max(items, key=lambda it: (float(it[0].mtime or 0.0), it[1]))
            raw = tf.extractfile(best_member)
            body = raw.read().decode("utf-8", "replace") if raw is not None else ""
            prom.setdefault(mode, {}).setdefault(conc, {})[kind] = body
        timeseries = None
        ts_rel = str(summary.get("resource_timeseries_path") or "resource_timeseries.json")
        try:
            ts_member = tf.extractfile(f"{root_name}/{ts_rel}")
            if ts_member is not None:
                timeseries = json.loads(ts_member.read().decode())
        except Exception:
            timeseries = None
        return summary, artifacts, prom, timeseries


def _pick_prom_blob(paths: list[Path]) -> str:
    """If multiple snapshots exist for the same mode/conc/kind, use the newest file by mtime (tie-break: path)."""
    if not paths:
        return ""
    chosen = max(paths, key=lambda x: (x.stat().st_mtime_ns, str(x)))
    return chosen.read_text()


def _gather_prom(root: Path) -> dict[str, dict[str, dict[str, Any]]]:
    before_idx: dict[tuple[str, str], list[Path]] = {}
    after_idx: dict[tuple[str, str], list[Path]] = {}
    for p in root.glob("*/*/before_*.prom"):
        mode, conc, _fname = p.parts[-3:]
        before_idx.setdefault((mode, conc), []).append(p)
    for p in root.glob("*/*/after_*.prom"):
        mode, conc, _fname = p.parts[-3:]
        after_idx.setdefault((mode, conc), []).append(p)

    prom: dict[str, dict[str, dict[str, Any]]] = {}
    for (mode, conc), paths in before_idx.items():
        prom.setdefault(mode, {}).setdefault(conc, {})["before"] = _pick_prom_blob(paths)
    for (mode, conc), paths in after_idx.items():
        prom.setdefault(mode, {}).setdefault(conc, {})["after"] = _pick_prom_blob(paths)
    return prom


def compute_mode_metrics(prom_trees: dict[str, dict[str, dict[str, Any]]]) -> dict[str, ModeMetrics]:
    out: dict[str, ModeMetrics] = {}
    for mode, by_conc in prom_trees.items():
        conc = pick_primary_concurrency(by_conc)
        blob_before = by_conc.get(conc, {}).get("before", "")
        blob_after = by_conc.get(conc, {}).get("after", "")
        before_saved = parse_prom_counter(blob_before, "deltaglider_delta_bytes_saved_total") or 0.0
        after_saved = parse_prom_counter(blob_after, "deltaglider_delta_bytes_saved_total") or 0.0
        before_enc = parse_prom_counter(blob_before, "deltaglider_delta_encode_duration_seconds_count") or 0.0
        after_enc = parse_prom_counter(blob_after, "deltaglider_delta_encode_duration_seconds_count") or 0.0
        raw_saved = after_saved - before_saved
        raw_enc = after_enc - before_enc
        out[mode] = ModeMetrics(
            mode=mode,
            delta_saved_increment=max(0.0, raw_saved),
            encode_count_delta=max(0.0, raw_enc),
            delta_saved_raw=raw_saved,
            prom_delta_clamped=raw_saved < 0.0,
        )
    return out


def logical_bytes(artifacts: list[dict[str, Any]], summary: dict[str, Any]) -> int:
    if artifacts:
        return int(sum(int(a["bytes"]) for a in artifacts))
    modes = summary.get("modes") or {}
    for mode in MODE_ORDER:
        by_conc = modes.get(mode)
        if not by_conc:
            continue
        ck = pick_primary_concurrency(by_conc)
        put = (by_conc.get(ck) or {}).get("put") or {}
        b = put.get("bytes")
        if b is not None:
            return int(b)
    raise ValueError("summary.json missing modes/bytes needed for logical payload")


def compute_mode_spans(markers: list[dict[str, Any]], concurrency: int = 1) -> list[dict[str, Any]]:
    """[t0,t1] per benchmark mode from before_snapshot → after_snapshot (one run = all modes in sequence)."""
    t_start: dict[str, float] = {}
    t_end: dict[str, float] = {}
    ordered = sorted(markers, key=lambda x: float(x.get("elapsed_s") or 0.0))
    for m in ordered:
        try:
            conc = int(m.get("concurrency") or 1)
        except (TypeError, ValueError):
            conc = 1
        if conc != concurrency:
            continue
        mode = str(m.get("mode") or "")
        if not mode:
            continue
        ev = m.get("event")
        el = m.get("elapsed_s")
        if el is None:
            continue
        try:
            ef = float(el)
        except (TypeError, ValueError):
            continue
        if ev == "before_snapshot":
            t_start.setdefault(mode, ef)
        elif ev == "after_snapshot":
            t_end[mode] = ef
    out: list[dict[str, Any]] = []
    for mode in MODE_ORDER:
        if mode not in t_start or mode not in t_end:
            continue
        t0, t1 = t_start[mode], t_end[mode]
        if t1 < t0:
            continue
        out.append(
            {
                "mode": mode,
                "t0": t0,
                "t1": t1,
                "color": MODE_BAND_COLORS.get(mode, "rgba(148, 163, 184, 0.12)"),
                "label": mode.replace("_", " ").title(),
            }
        )
    return out


def compute_mode_docker_cpu_timeseries_rollup(
    resource_timeseries: dict[str, Any] | None,
    concurrency: int = 1,
) -> dict[str, Any]:
    """Mean/max Docker CPU%% per mode from resource samples between before_snapshot and after_snapshot.

    Uses the same mode windows as ``compute_mode_spans`` (whole PUT/cold/warm segment per mode).
    """
    empty: dict[str, Any] = {"labels": [], "docker_cpu_mean": [], "docker_cpu_max": [], "ok": False}
    if not resource_timeseries:
        return empty
    samples = resource_timeseries.get("samples") or []
    markers = resource_timeseries.get("markers") or []
    if not samples or not markers:
        return empty
    spans = compute_mode_spans(markers, concurrency)
    if not spans:
        return empty
    span_by_mode = {str(s["mode"]): (float(s["t0"]), float(s["t1"])) for s in spans}
    labels: list[str] = []
    means: list[float | None] = []
    maxs: list[float | None] = []
    for mode in MODE_ORDER:
        if mode not in span_by_mode:
            continue
        t0, t1 = span_by_mode[mode]
        cpus: list[float] = []
        for samp in samples:
            el = samp.get("elapsed_s")
            if el is None:
                continue
            try:
                ef = float(el)
            except (TypeError, ValueError):
                continue
            if ef < t0 or ef > t1:
                continue
            hr = samp.get("host_resources") or {}
            dk = hr.get("docker_stats") if isinstance(hr, dict) else {}
            p = parse_percent((dk or {}).get("cpu_percent"))
            if p is not None:
                cpus.append(p)
        labels.append(MODE_SHORT_LABELS[mode])
        if cpus:
            means.append(round(sum(cpus) / len(cpus), 4))
            maxs.append(round(max(cpus), 4))
        else:
            means.append(None)
            maxs.append(None)
    if not labels:
        return empty
    return {"labels": labels, "docker_cpu_mean": means, "docker_cpu_max": maxs, "ok": True}


def build_rollup_comparison(rollup: dict[str, Any] | None) -> dict[str, Any]:
    """Per-mode snapshot metrics (after_*.json) for side-by-side bars — same table as footprint, chart form."""
    empty = {"labels": [], "rss_mb": [], "docker_cpu": [], "plain_mb": [], "encrypted_mb": []}
    if not rollup:
        return empty
    modes = rollup.get("modes") or {}
    labels: list[str] = []
    rss_mb: list[float | None] = []
    docker_cpu: list[float | None] = []
    plain_mb: list[float | None] = []
    encrypted_mb: list[float | None] = []
    for mode in MODE_ORDER:
        if mode not in modes:
            continue
        snap = snap_mode_rollup_row(modes.get(mode))
        if not snap:
            continue
        labels.append(MODE_SHORT_LABELS[mode])
        ps = snap.get("process_peak_rss_bytes_prom")
        h = snap.get("health") or {}
        if ps is None and isinstance(h, dict):
            ps = h.get("peak_rss_bytes")
        try:
            rss_mb.append(round(float(ps) / 1e6, 4) if ps is not None else None)
        except (TypeError, ValueError):
            rss_mb.append(None)
        hr = snap.get("host_resources") or {}
        dk = hr.get("docker_stats") if isinstance(hr, dict) else {}
        docker_cpu.append(parse_percent((dk or {}).get("cpu_percent")))
        disk = hr.get("disk_backend_bytes_approx") or {} if isinstance(hr, dict) else {}
        pb, eb = disk.get("plain_du_sb"), disk.get("encrypted_du_sb")
        try:
            plain_mb.append(round(float(pb) / 1e6, 4) if pb is not None else None)
        except (TypeError, ValueError):
            plain_mb.append(None)
        try:
            encrypted_mb.append(round(float(eb) / 1e6, 4) if eb is not None else None)
        except (TypeError, ValueError):
            encrypted_mb.append(None)
    return {
        "labels": labels,
        "rss_mb": rss_mb,
        "docker_cpu": docker_cpu,
        "plain_mb": plain_mb,
        "encrypted_mb": encrypted_mb,
    }
