from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from .bench_bundle import (
    ModeMetrics,
    build_rollup_comparison,
    compute_mode_docker_cpu_timeseries_rollup,
    compute_mode_metrics,
    compute_mode_spans,
    load_bundle,
    logical_bytes,
)
from .bench_report_format import (
    fmt_agg_cell,
    fmt_bytes,
    fmt_gb,
    html_escape,
    parse_docker_mem_used_mib,
    parse_percent,
)
from .bench_summary import PHASE_NAMES, pick_primary_concurrency, snap_mode_rollup_row
from .config import MODE_ORDER, REPORT_STORAGE_HINTS, REPORT_THROUGHPUT_HINTS


def _phase_marker_label(mode: str, phase: str, concurrency: int = 1) -> str:
    m = mode.replace("_", " ")
    p = phase.replace("_", " ").upper() if phase else ""
    if concurrency and concurrency != 1:
        return f"{m} · {p} · c{concurrency}"
    return f"{m} · {p}"


def _phase_markers_visual(markers: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Markers for vertical lines + tooltips (phase_start only)."""
    out: list[dict[str, Any]] = []
    for m in markers:
        if m.get("event") != "phase_start":
            continue
        mode = str(m.get("mode") or "")
        phase = str(m.get("phase") or "")
        try:
            conc = int(m.get("concurrency") or 1)
        except (TypeError, ValueError):
            conc = 1
        el = m.get("elapsed_s")
        if el is None:
            continue
        try:
            ef = float(el)
        except (TypeError, ValueError):
            continue
        label = _phase_marker_label(mode, phase, conc)
        out.append({"elapsed_s": ef, "mode": mode, "phase": phase, "concurrency": conc, "label": label})
    out.sort(key=lambda x: x["elapsed_s"])
    return out


def _format_phase_markers(markers: list[dict[str, Any]]) -> str:
    """Human-readable schedule: when each mode/phase started (from runner markers)."""
    rows: list[str] = []
    for m in markers:
        if m.get("event") != "phase_start":
            continue
        mode = m.get("mode") or ""
        phase = m.get("phase") or ""
        try:
            conc = int(m.get("concurrency") or 1)
        except (TypeError, ValueError):
            conc = 1
        el = m.get("elapsed_s")
        if el is None:
            continue
        label = _phase_marker_label(str(mode), str(phase), conc)
        rows.append(
            "<tr>"
            f"<td>{html_escape(str(el))}</td>"
            f"<td><code>{html_escape(str(mode))}</code></td>"
            f"<td><code>{html_escape(str(phase))}</code></td>"
            f"<td>{html_escape(label)}</td>"
            "</tr>"
        )
    if not rows:
        return ""
    return (
        "<table><thead><tr>"
        "<th>Elapsed s</th><th>Mode</th><th>Phase</th><th>Label (same as chart hover)</th>"
        "</tr></thead>"
        f"<tbody>{''.join(rows)}</tbody></table>"
        "<p class=\"note\">Same timestamps as the <strong>amber/blue vertical lines</strong> on the graphs above.</p>"
    )


def _extract_resource_series(timeseries: dict[str, Any] | None) -> dict[str, Any] | None:
    if not timeseries:
        return None
    samples = timeseries.get("samples") or []
    if not samples:
        return None
    labels: list[float] = []
    cpu_pct: list[float | None] = []
    rss_mb: list[float | None] = []
    docker_mem_mb: list[float | None] = []
    disk_plain_mb: list[float | None] = []
    disk_encrypted_mb: list[float | None] = []
    disk_total_mb: list[float | None] = []
    for s in samples:
        elapsed = s.get("elapsed_s")
        if elapsed is None:
            continue
        try:
            labels.append(round(float(elapsed), 1))
        except ValueError:
            continue
        prom = s.get("prom_summary") or {}
        health = s.get("health") or {}
        hr = s.get("host_resources") or {}
        dk = hr.get("docker_stats") if isinstance(hr, dict) else {}
        disk = hr.get("disk_backend_bytes_approx") if isinstance(hr, dict) else {}

        cpu_pct.append(parse_percent((dk or {}).get("cpu_percent")))
        rss_b = (prom or {}).get("process_peak_rss_bytes")
        if rss_b is None and isinstance(health, dict):
            rss_b = health.get("peak_rss_bytes")
        try:
            rss_mb.append((float(rss_b) / 1_000_000.0) if rss_b is not None else None)
        except (TypeError, ValueError):
            rss_mb.append(None)
        docker_mem_mb.append(parse_docker_mem_used_mib((dk or {}).get("mem_usage")))

        plain_b = (disk or {}).get("plain_du_sb")
        enc_b = (disk or {}).get("encrypted_du_sb")
        try:
            plain_mb = float(plain_b) / 1_000_000.0 if plain_b is not None else None
        except (TypeError, ValueError):
            plain_mb = None
        try:
            enc_mb = float(enc_b) / 1_000_000.0 if enc_b is not None else None
        except (TypeError, ValueError):
            enc_mb = None
        disk_plain_mb.append(plain_mb)
        disk_encrypted_mb.append(enc_mb)
        if plain_mb is None and enc_mb is None:
            disk_total_mb.append(None)
        else:
            disk_total_mb.append((plain_mb or 0.0) + (enc_mb or 0.0))

    if not labels:
        return None
    return {
        "labels": labels,
        "cpu_pct": cpu_pct,
        "rss_mb": rss_mb,
        "docker_mem_mb": docker_mem_mb,
        "disk_plain_mb": disk_plain_mb,
        "disk_encrypted_mb": disk_encrypted_mb,
        "disk_total_mb": disk_total_mb,
        "markers": timeseries.get("markers") or [],
    }


def _format_resources_rollup(rollup: dict[str, Any] | None) -> str:
    if not rollup:
        return (
            "<p class=\"note\">No resource rollup embedded in <code>summary.json</code>. "
            "Re-run with <code>--health-url …/_/health</code>, Prometheus metrics, and optionally "
            "<code>--resource-command</code> (see <code>scripts/benchmark_resources_linux.sh</code>).</p>"
        )
    modes = rollup.get("modes") or {}
    if not modes:
        return "<p class=\"note\">Empty <code>resources_rollup</code>.</p>"
    rows = []
    for mode in MODE_ORDER:
        if mode not in modes:
            continue
        snap = snap_mode_rollup_row(modes.get(mode))
        if not snap:
            continue
        h = snap.get("health") or {}
        rss_h = h.get("peak_rss_bytes") if isinstance(h, dict) else None
        rss_p = snap.get("process_peak_rss_bytes_prom")
        hr = snap.get("host_resources") or {}
        dk = hr.get("docker_stats") or {} if isinstance(hr, dict) else {}
        disk = hr.get("disk_backend_bytes_approx") or {} if isinstance(hr, dict) else {}
        cpu = dk.get("cpu_percent", "—") if isinstance(dk, dict) else "—"
        memu = dk.get("mem_usage", "—") if isinstance(dk, dict) else "—"
        plain_b = disk.get("plain_du_sb")
        enc_b = disk.get("encrypted_du_sb")
        rows.append(
            "<tr>"
            f"<td><code>{html_escape(mode)}</code></td>"
            f"<td>{fmt_bytes(rss_h)}</td>"
            f"<td>{fmt_bytes(rss_p)}</td>"
            f"<td>{html_escape(str(cpu))}</td>"
            f"<td>{html_escape(str(memu))}</td>"
            f"<td>{fmt_bytes(plain_b)}</td>"
            f"<td>{fmt_bytes(enc_b)}</td>"
            "</tr>"
        )
    return (
        "<table>"
        "<thead><tr>"
        "<th>Mode</th><th>RSS <code>/_/health</code></th><th>RSS prom</th>"
        "<th>Docker CPU%</th><th>Docker mem</th><th><code>du</code> plain</th><th><code>du</code> encrypted</th>"
        "</tr></thead>"
        f"<tbody>{''.join(rows)}</tbody>"
        "</table>"
        "<p class=\"note\">RSS from health/Prometheus is process-wide on the proxy. "
        "<code>du</code> totals include every bucket using that filesystem backend root.</p>"
    )


def _format_trial_aggregate_section(
    trial_aggregate: dict[str, Any] | None,
    summary: dict[str, Any],
) -> tuple[str, str]:
    """Returns (banner_html, cross_trial_table_html)."""
    banner = ""
    if summary.get("multi_trial"):
        tc = summary.get("trial_count")
        pdir = summary.get("primary_trial_dir") or "trial_001"
        banner = (
            f'<p class="doc-meta"><strong>Multi-trial bundle:</strong> {html_escape(str(tc))} runs; throughput/storage '
            f"charts use <code>{html_escape(str(pdir))}</code> only.</p>"
        )
    if not trial_aggregate:
        return banner, ""
    n = int(trial_aggregate.get("trial_count") or 0)
    if n < 2:
        return banner, ""
    modes_block = trial_aggregate.get("modes") or {}
    rows: list[str] = []
    for mode in MODE_ORDER:
        if mode not in modes_block:
            continue
        by_c = modes_block[mode]
        conc_key = pick_primary_concurrency(by_c)
        for phase in PHASE_NAMES:
            st = ((by_c.get(conc_key) or {}).get(phase) or {}).get("mb_s")
            if not isinstance(st, dict) or not st.get("n"):
                continue
            rows.append(
                "<tr>"
                f"<td><code>{html_escape(mode)}</code></td>"
                f"<td><code>{html_escape(str(conc_key))}</code></td>"
                f"<td><code>{html_escape(phase)}</code></td>"
                f"<td>{st['n']}</td>"
                f"<td>{fmt_agg_cell(st.get('mean'))}</td>"
                f"<td>{fmt_agg_cell(st.get('p50'))}</td>"
                f"<td>{fmt_agg_cell(st.get('p95'))}</td>"
                f"<td>{fmt_agg_cell(st.get('min'))}</td>"
                f"<td>{fmt_agg_cell(st.get('max'))}</td>"
                "</tr>"
            )
    if not rows:
        return banner, ""
    dirs = trial_aggregate.get("trial_directories") or []
    note = (
        f"<p class=\"note\">Cross-trial stats: each cell is the distribution of <strong>one number per trial</strong> "
        f"(that trial&rsquo;s aggregate <code>mb_s</code> for the phase). Trials: <code>{html_escape(', '.join(dirs))}</code>. "
        "This is <strong>not</strong> the same as within-run op percentiles in <code>summary.json</code>.</p>"
    )
    table = (
        '<h2 id="cross-trial">Cross-trial throughput (MB/s)</h2>'
        f"{note}"
        "<table><thead><tr>"
        "<th>Mode</th><th>Conc</th><th>Phase</th><th>n</th>"
        "<th>mean</th><th>p50</th><th>p95</th><th>min</th><th>max</th>"
        "</tr></thead><tbody>"
        f"{''.join(rows)}"
        "</tbody></table>"
    )
    return banner, table


def _chart_mb_s(phase: dict[str, Any]) -> float:
    """Prefer wall-clock phase throughput when ``phase_wall_s`` was recorded."""
    w = phase.get("mb_s_wall")
    if w is not None:
        try:
            return round(float(w), 2)
        except (TypeError, ValueError):
            pass
    return round(float(phase.get("mb_s", 0.0)), 2)


def render_html(
    run_id: str,
    summary: dict[str, Any],
    artifacts: list[dict[str, Any]],
    mode_metrics: dict[str, ModeMetrics],
    logical_b: int,
    resources_rollup: dict[str, Any] | None = None,
    resource_timeseries: dict[str, Any] | None = None,
    trial_aggregate: dict[str, Any] | None = None,
) -> str:
    chart_labels = []
    put_mbps = []
    cold_mbps = []
    warm_mbps = []

    storage_rows: list[dict[str, Any]] = []

    for mode in MODE_ORDER:
        by_conc = (summary.get("modes") or {}).get(mode) or {}
        conc_key = pick_primary_concurrency(by_conc)
        phases = by_conc[conc_key]
        label = mode.replace("_", " ")
        chart_labels.append(label)
        put_mbps.append(_chart_mb_s(phases["put"]))
        cold_mbps.append(_chart_mb_s(phases["cold_get"]))
        warm_mbps.append(_chart_mb_s(phases["warm_get"]))

        mm = mode_metrics.get(mode)
        saved = mm.delta_saved_increment if mm else 0.0
        enc_n = mm.encode_count_delta if mm else 0.0

        # Logical GET verifies SHA against original client bytes; encryption still serves plaintext.
        implied_stored = logical_b - saved

        storage_rows.append(
            {
                "mode": mode,
                "label": label.title(),
                "logical_gb": logical_b / 1e9,
                "delta_saved_gb": saved / 1e9,
                "implied_stored_gb": implied_stored / 1e9,
                "savings_pct": (saved / logical_b * 100.0) if logical_b else 0.0,
                "delta_encode_events": int(enc_n),
            }
        )

    comp_savings_pct = next((r["savings_pct"] for r in storage_rows if r["mode"] == "compression"), None)
    comp_savings_display = f"{comp_savings_pct:.2f}%" if comp_savings_pct is not None else "—"

    throughput_hints_list = [REPORT_THROUGHPUT_HINTS.get(m, "") for m in MODE_ORDER]
    storage_hints_list = [REPORT_STORAGE_HINTS.get(r["mode"], "") for r in storage_rows]

    artifacts_preview = json.dumps(artifacts[:12], indent=2)

    conclusions: list[str] = []
    # Use compression row specifically for savings statement
    comp = mode_metrics.get("compression")
    if comp and comp.delta_saved_increment <= 0 and (comp.encode_count_delta <= 0):
        conclusions.append(
            "<strong>No delta storage win detected in Prometheus for the compression segment.</strong> "
            "<code>.iso</code> is delta-eligible by default (<code>FileRouter</code>). Zero Δ here usually means every "
            "PUT fell back to passthrough (often the <strong>first object in a deltaspace</strong> loses on "
            "<code>max_delta_ratio</code>), or snapshots/scrapes missed encode counts — check proxy logs for "
            "<code>Delta computed</code> / <code>delta_decisions_total</code>."
        )
    elif comp and comp.delta_saved_increment > 0:
        conclusions.append(
            f"<strong>Compression reduced stored bytes.</strong> Approximately <strong>{comp.delta_saved_increment / 1e6:.2f} MB</strong> "
            f"saved vs logical uploads (<strong>{comp.delta_saved_increment / logical_b * 100:.2f}%</strong> of logical bytes)."
        )

    if any(mm is not None and mm.prom_delta_clamped for mm in mode_metrics.values()):
        conclusions.append(
            "<strong>Prometheus Δ clamped.</strong> At least one mode had <code>after &lt; before</code> on "
            "<code>deltaglider_delta_bytes_saved_total</code> (counter reset, mismatched scrape, or overlapping snapshots). "
            "Treat Δ-saved figures as unreliable for that mode."
        )

    modes_d = summary.get("modes") or {}

    def _put_wall_mb(mode_name: str) -> float | None:
        mc = modes_d.get(mode_name)
        if not mc:
            return None
        ck = pick_primary_concurrency(mc)
        ph = mc.get(ck, {}).get("put")
        if not ph:
            return None
        return _chart_mb_s(ph)

    pt = _put_wall_mb("passthrough")
    cp = _put_wall_mb("compression")
    ep = _put_wall_mb("encryption")
    if pt is not None and cp is not None:
        conclusions.append(
            f"PUT throughput (wall-clock MB/s, primary concurrency) drops from <strong>{pt:.1f} MB/s</strong> (passthrough) to "
            f"<strong>{cp:.1f} MB/s</strong> (compression) — delta encode is CPU-heavy vs streaming writes."
        )
    if ep is not None and pt is not None:
        conclusions.append(
            f"The encrypted backend bucket measured <strong>{ep:.1f} MB/s</strong> PUT vs "
            f"<strong>{pt:.1f} MB/s</strong> passthrough — AES-GCM adds measurable CPU cost while preserving logical bytes on read."
        )

    conclusions_html = "\n".join(f"<p>{c}</p>" for c in conclusions)

    multi_banner_html, cross_trial_section_html = _format_trial_aggregate_section(trial_aggregate, summary)

    rollup_section = _format_resources_rollup(resources_rollup)
    resource_series = _extract_resource_series(resource_timeseries)
    markers_src = (resource_timeseries or {}).get("markers") or []
    mode_spans = compute_mode_spans(markers_src)
    mode_spans_json = json.dumps(mode_spans)
    rollup_compare = build_rollup_comparison(resources_rollup)
    ts_cpu = compute_mode_docker_cpu_timeseries_rollup(resource_timeseries)
    rollup_compare["docker_cpu_timeseries"] = False
    if ts_cpu.get("ok") and rollup_compare.get("labels") == ts_cpu.get("labels"):
        rollup_compare["docker_cpu_mean"] = ts_cpu["docker_cpu_mean"]
        rollup_compare["docker_cpu_max"] = ts_cpu["docker_cpu_max"]
        rollup_compare["docker_cpu_timeseries"] = True
    rollup_compare_json = json.dumps(rollup_compare)
    phase_visual_list = _phase_markers_visual(markers_src)
    phase_visual_json = json.dumps(phase_visual_list)
    throughput_hints_json = json.dumps(throughput_hints_list)
    storage_hints_json = json.dumps(storage_hints_list)
    sample_interval_s = (resource_timeseries or {}).get("sample_interval_s")
    sample_interval_json = json.dumps(float(sample_interval_s) if sample_interval_s is not None else 2.0)

    chart_json = json.dumps(
        {
            "labels": chart_labels,
            "datasets": [
                {"label": "PUT", "data": put_mbps, "backgroundColor": "rgba(59, 130, 246, 0.65)"},
                {"label": "Cold GET", "data": cold_mbps, "backgroundColor": "rgba(16, 185, 129, 0.65)"},
                {"label": "Warm GET", "data": warm_mbps, "backgroundColor": "rgba(251, 146, 60, 0.75)"},
            ],
        }
    )

    logical_gb = round(storage_rows[0]["logical_gb"], 6) if storage_rows else 0.0
    storage_chart_json = json.dumps(
        {
            "labels": [r["label"] for r in storage_rows],
            "datasets": [
                {
                    "label": "Logical uploaded (client-visible)",
                    "data": [logical_gb for _ in storage_rows],
                    "backgroundColor": "rgba(148, 163, 184, 0.55)",
                },
                {
                    "label": "Implied stored on backend (logical − Δ saved)",
                    "data": [round(r["implied_stored_gb"], 6) for r in storage_rows],
                    "backgroundColor": "rgba(59, 130, 246, 0.68)",
                },
            ],
        }
    )

    meta_dataset = ""
    if artifacts:
        names = [a["name"] for a in artifacts]
        joined = html_escape(", ".join(names))
        if len(names) <= 6:
            meta_dataset = f'<p id="artifacts"><strong>Artifacts ({len(names)}):</strong> <code>{joined}</code></p>'
        else:
            preview = html_escape(", ".join(names[:6]))
            meta_dataset = (
                f'<details class="details-inline" id="artifacts"><summary><strong>Artifacts ({len(names)})</strong> '
                f'<span class="muted"> — {preview}, …</span></summary>'
                f'<p class="artifact-names"><code>{joined}</code></p></details>'
            )

    best_i = max(range(len(put_mbps)), key=lambda i: put_mbps[i]) if put_mbps else 0
    worst_i = min(range(len(put_mbps)), key=lambda i: put_mbps[i]) if put_mbps else 0
    best_mode = chart_labels[best_i] if chart_labels else "n/a"
    worst_mode = chart_labels[worst_i] if chart_labels else "n/a"
    best_put = put_mbps[best_i] if put_mbps else 0.0
    worst_put = put_mbps[worst_i] if put_mbps else 0.0
    put_spread = (best_put / worst_put) if worst_put else 0.0

    sample_note = ""
    if sample_interval_s is not None:
        sample_note = f"<p class=\"doc-meta\"><strong>Sampling interval:</strong> <code>{html_escape(str(sample_interval_s))}s</code> between resource snapshots (whole run).</p>"

    throughput_note_fragments = [
        "MB/s from timed PUT/GET per mode. <strong>Hover</strong> for per-series values and a short scenario gloss."
    ]
    md0 = summary.get("modes") or {}
    if "passthrough" in md0:
        ck0 = pick_primary_concurrency(md0["passthrough"])
        pt_put0 = md0["passthrough"].get(ck0, {}).get("put") or {}
        nlat = pt_put0.get("latency_sample_n")
        if nlat:
            throughput_note_fragments.append(
                "Bars use <strong>wall-clock</strong> <code>mb_s_wall</code>; latency percentiles are order statistics over "
                f"<strong>N={html_escape(str(int(nlat)))}</strong> successful ops per phase (same concurrency)."
            )
    throughput_note_html = '<p class="note">' + " ".join(throughput_note_fragments) + "</p>"

    has_rollup_compare = bool(rollup_compare.get("labels"))
    rollup_compare_html = ""
    if has_rollup_compare:
        rollup_compare_html = (
            '<h2 id="resources-compare">Resource footprint by test mode (side-by-side)</h2>'
            "<div class=\"doc-panel doc-panel-tight\">"
            "<p>End-of-mode snapshots from <code>resources_rollup.json</code> (same basis as the table above): "
            "passthrough vs compression vs encryption vs both. The <strong>timeseries</strong> section below is "
            "one continuous run with coloured bands for mode windows.</p>"
            "<p class=\"muted doc-meta\">Docker CPU compare chart uses <strong>mean and maximum</strong> of "
            "<code>docker stats</code> samples between each mode's <code>before_snapshot</code> and <code>after_snapshot</code> "
            "when <code>resource_timeseries.json</code> is present; otherwise it falls back to the idle "
            "end-of-mode snapshot only (misleading for workload CPU).</p>"
            "</div>"
            "<p class=\"chart-title\">Peak proxy RSS (MB)</p>"
            "<div class=\"chart-wrap chart-short\"><canvas id=\"resCmpRss\"></canvas></div>"
            "<p class=\"chart-title\">Docker CPU % — mean &amp; max per mode window</p>"
            "<div class=\"chart-wrap chart-short\"><canvas id=\"resCmpCpu\"></canvas></div>"
            "<p class=\"chart-title\">Backend disk (<code>du -sb</code>) — plain vs encrypted</p>"
            "<div class=\"chart-wrap chart-short\"><canvas id=\"resCmpDisk\"></canvas></div>"
        )

    resource_section_html = (
        "<p class=\"note\">No whole-run resource timeseries found. Re-run benchmark with "
        "<code>--resource-sample-interval</code> (default 2s), plus <code>--health-url</code> and "
        "optionally <code>--resource-command</code> to capture host/container signals.</p>"
    )
    resource_series_json = "{}"
    if resource_series:
        resource_series_json = json.dumps(resource_series)
        phase_table = _format_phase_markers(markers_src)
        phase_block = (
            phase_table
            if phase_table
            else "<p class=\"note\">No <code>phase_start</code> markers in this bundle (older run). "
            "See <code>resource_timeseries.json</code> or re-run with a current benchmark runner.</p>"
        )
        if phase_table:
            phase_block = (
                '<details class="phase-schedule"><summary>Phase schedule table '
                "(same markers as vertical dashes)</summary>"
                f"{phase_table}</details>"
            )
        resource_section_html = (
            "<div class=\"doc-panel\">"
            "<h3>How to read the resource timeseries</h3>"
            "<ol class=\"compact-ol\">"
            "<li><strong>X axis</strong> — elapsed seconds from harness start (<code>t=0</code> after artifact prep).</li>"
            "<li><strong>Shaded bands</strong> — benchmark mode windows in run order (passthrough → … → comp+encrypt).</li>"
            "<li><strong>Vertical dashes</strong> — phase starts (PUT / cold GET / warm GET); amber/blue alternate.</li>"
            "<li><strong>Hover</strong> — sample values; footer lists nearby phase markers and dominant phase at <code>t</code>.</li>"
            "</ol>"
            "<details class=\"doc-details\"><summary>Signal sources (CPU / RAM / disk)</summary>"
            "<ul class=\"tight-ul\">"
            "<li><strong>CPU</strong> — <code>docker stats</code> CPU%; compare chart uses mean/max over each mode band when timeseries exists.</li>"
            "<li><strong>RAM</strong> — proxy RSS from health/Prometheus vs cgroup memory (<code>MemUsage</code>).</li>"
            "<li><strong>Disk</strong> — <code>du -sb</code> on plain vs encrypted backend roots (jumps on bucket/mode changes).</li>"
            "</ul></details>"
            f"{sample_note}"
            "</div>"
            "<p class=\"chart-title\">CPU over time</p>"
            "<div class=\"chart-wrap\"><canvas id=\"resCpu\"></canvas></div>"
            "<p class=\"chart-title\">RAM over time</p>"
            "<div class=\"chart-wrap\"><canvas id=\"resRam\"></canvas></div>"
            "<p class=\"chart-title\">Disk footprint over time</p>"
            "<div class=\"chart-wrap\"><canvas id=\"resDisk\"></canvas></div>"
            "<h3>Phase reference</h3>"
            + phase_block
        )

    toc_parts = [
        '<nav class="toc" aria-label="Report sections">',
        '<span class="toc-label">Jump to</span>',
        '<a href="#sec-throughput">Throughput</a>',
        '<a href="#sec-storage">Storage</a>',
        '<a href="#sec-footprint">Footprint</a>',
    ]
    if has_rollup_compare:
        toc_parts.append('<a href="#resources-compare">Mode comparison</a>')
    toc_parts.append('<a href="#sec-timeseries">Timeseries</a>')
    if cross_trial_section_html:
        toc_parts.append('<a href="#cross-trial">Cross-trial</a>')
    toc_parts.extend(
        [
            '<a href="#conclusions">Conclusions</a>',
            '<a href="#manifest">Manifest</a>',
            "</nav>",
        ]
    )
    toc_html = "\n    ".join(toc_parts)

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>DeltaGlider benchmark — {html_escape(run_id)}</title>
  <script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.6/dist/chart.umd.min.js"></script>
  <style>
    :root {{
      --bg: #0b1220;
      --panel: rgba(255,255,255,0.04);
      --text: #e5e7eb;
      --muted: #94a3b8;
      --accent: #38bdf8;
      --border: rgba(148,163,184,0.22);
      font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial;
    }}
    body {{
      margin: 0;
      background: radial-gradient(1200px 600px at 20% -10%, rgba(56,189,248,0.14), transparent),
                  radial-gradient(900px 500px at 90% 0%, rgba(99,102,241,0.12), transparent),
                  var(--bg);
      color: var(--text);
      line-height: 1.55;
    }}
    .wrap {{ max-width: 1160px; margin: 0 auto; padding: 36px 22px 80px; }}
    h1 {{ font-size: 2rem; letter-spacing: -0.03em; margin: 0 0 8px; }}
    .sub {{ color: var(--muted); margin: 0 0 26px; }}
    .grid {{ display: grid; gap: 18px; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); margin: 22px 0 28px; }}
    .card {{
      border: 1px solid var(--border);
      border-radius: 14px;
      padding: 16px 18px;
      background: var(--panel);
      backdrop-filter: blur(8px);
    }}
    .card h3 {{ margin: 0 0 8px; font-size: 0.82rem; letter-spacing: 0.08em; text-transform: uppercase; color: var(--muted); }}
    .metric {{ font-size: 1.65rem; font-weight: 750; letter-spacing: -0.02em; }}
    canvas {{ width: 100% !important; height: 380px !important; }}
    .chart-wrap.chart-short canvas {{ height: 280px !important; }}
    .chart-wrap {{
      border: 1px solid var(--border);
      border-radius: 16px;
      padding: 16px 18px 8px;
      background: var(--panel);
      margin: 18px 0 26px;
    }}
    h2 {{ font-size: 1.25rem; margin: 32px 0 12px; letter-spacing: -0.02em; }}
    table {{ width: 100%; border-collapse: collapse; font-size: 0.92rem; }}
    th, td {{ border-bottom: 1px solid var(--border); padding: 10px 8px; text-align: left; }}
    th {{ color: var(--muted); font-weight: 650; font-size: 0.78rem; text-transform: uppercase; letter-spacing: 0.06em; }}
    tr:last-child td {{ border-bottom: none; }}
    code {{ font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; font-size: 0.86em; }}
    .note {{ color: var(--muted); font-size: 0.9rem; }}
    pre {{
      background: rgba(2,6,23,0.55);
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 14px;
      overflow: auto;
      font-size: 0.78rem;
      max-height: 260px;
    }}
    .pill {{
      display: inline-block;
      padding: 4px 10px;
      border-radius: 999px;
      border: 1px solid var(--border);
      font-size: 0.78rem;
      color: var(--muted);
      margin-right: 8px;
    }}
    .doc-panel {{
      border: 1px solid var(--border);
      border-radius: 14px;
      padding: 18px 20px 14px;
      background: rgba(2, 6, 23, 0.42);
      margin: 0 0 18px;
    }}
    .doc-panel h3 {{ margin-top: 0; font-size: 1.05rem; letter-spacing: -0.02em; }}
    .doc-panel ol {{ margin: 8px 0 0 1.15rem; padding: 0; }}
    .doc-panel li {{ margin: 10px 0; }}
    .doc-meta {{ margin: 14px 0 0; font-size: 0.88rem; color: var(--muted); }}
    .chart-title {{
      font-size: 0.95rem;
      font-weight: 650;
      letter-spacing: -0.02em;
      margin: 18px 0 8px;
      color: var(--text);
    }}
    .toc {{
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      gap: 10px 12px;
      padding: 12px 16px;
      margin: 0 0 22px;
      border: 1px solid rgba(56, 189, 248, 0.38);
      border-radius: 12px;
      background: rgba(2, 6, 23, 0.58);
      box-shadow: 0 0 0 1px rgba(56, 189, 248, 0.12), 0 8px 28px rgba(0, 0, 0, 0.35);
      font-size: 0.88rem;
    }}
    .toc-label {{
      color: var(--muted);
      font-weight: 650;
      margin-right: 4px;
      letter-spacing: 0.04em;
      text-transform: uppercase;
      font-size: 0.72rem;
    }}
    .toc a {{
      color: var(--accent);
      font-weight: 650;
      text-decoration: none;
      border-bottom: 1px solid transparent;
    }}
    .toc a:hover {{
      color: #7dd3fc;
      border-bottom-color: rgba(125, 211, 252, 0.65);
    }}
    .muted {{ color: var(--muted); font-weight: 400; }}
    .details-inline, .phase-schedule, .doc-details, .manifest-details {{
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 12px 16px 14px;
      margin: 0 0 18px;
      background: rgba(2, 6, 23, 0.35);
    }}
    .phase-schedule table {{ margin-top: 12px; }}
    details summary {{
      cursor: pointer;
      font-weight: 650;
      color: var(--text);
    }}
    .artifact-names {{ margin: 10px 0 0; word-break: break-word; font-size: 0.88rem; }}
    .compact-ol li {{ margin: 6px 0; }}
    .doc-panel .compact-ol {{ margin-top: 6px; }}
    .tight-ul {{ margin: 8px 0 0 1.1rem; padding: 0; }}
    .tight-ul li {{ margin: 6px 0; }}
    .doc-panel-tight p {{ margin: 0; }}
    .doc-details {{ margin-top: 12px !important; }}
    .manifest-details pre {{ margin-top: 12px; }}
    h2[id] {{ scroll-margin-top: 16px; }}
  </style>
</head>
<body>
  <div class="wrap">
    <span class="pill">Production tax benchmark</span>
    <span class="pill">{html_escape(run_id)}</span>
    <h1>DeltaGlider compression / encryption report</h1>
    <p class="sub">
      Interactive summary from <code>summary.json</code>, <code>artifacts.json</code>, and per-mode Prometheus snapshots.
      Storage savings use <strong>Δ deltaglider_delta_bytes_saved_total</strong> between each mode&rsquo;s <code>before_*.prom</code>
      and <code>after_*.prom</code> (isolated to that mode&rsquo;s phase within one proxy process).
    </p>
    {toc_html}
    {multi_banner_html}

    <div class="grid">
      <div class="card">
        <h3>Logical payload</h3>
        <div class="metric">{fmt_bytes(logical_b)}</div>
        <div class="note">Sum of upstream artifact sizes (client-visible bytes).</div>
      </div>
      <div class="card">
        <h3>PUT spread</h3>
        <div class="metric">{put_spread:.2f}x</div>
        <div class="note">Best: <code>{html_escape(best_mode)}</code> {best_put:.1f} MB/s; worst: <code>{html_escape(worst_mode)}</code> {worst_put:.1f} MB/s.</div>
      </div>
      <div class="card">
        <h3>Delta savings (compression phase)</h3>
        <div class="metric">{comp_savings_display}</div>
        <div class="note">Prometheus Δ during <code>compression</code> mode only.</div>
      </div>
    </div>

    {meta_dataset}

    <h2 id="sec-throughput">Throughput by mode</h2>
    {throughput_note_html}

    <div class="chart-wrap">
      <canvas id="throughput"></canvas>
    </div>

    <h2 id="sec-storage">Storage view (logical vs implied stored)</h2>
    <p class="note">
      <strong>Implied stored</strong> = logical bytes − Δ<code>deltaglider_delta_bytes_saved_total</code> for that mode.
      Encryption modes still verify SHA-256 against logical bytes on GET; ciphertext size on disk is not exported here — use
      <code>/_/stats?bucket=…</code> from an authenticated admin session if you need exact ciphertext footprint.
      <strong>Hover</strong> the chart — tooltip includes what each bar stack represents for that mode.
    </p>

    <div class="chart-wrap">
      <canvas id="storage"></canvas>
    </div>

    <table>
      <thead>
        <tr>
          <th>Mode</th>
          <th>Logical GB</th>
          <th>Δ saved GB (Prom)</th>
          <th>Implied stored GB</th>
          <th>Savings %</th>
          <th>Δ encode events</th>
        </tr>
      </thead>
      <tbody>
        {_storage_table_rows(storage_rows)}
      </tbody>
    </table>

    <h2 id="sec-footprint">CPU / RAM / disk footprint</h2>
    <p class="note">
      Pulls together Grafana-adjacent signals: JSON from <code>/_/health</code>, scalar gauges scraped from Prometheus text,
      and optional host JSON (<code>benchmark_resources_linux.sh</code>) for Docker stats + filesystem <code>du</code>.
      See <a href="../grafana-parity.md"><code>docs/benchmark/grafana-parity.md</code></a> for the full mapping.
    </p>
    {rollup_section}

    {rollup_compare_html}

    <h2 id="sec-timeseries">CPU / RAM / disk timeseries (whole run — all modes in one run)</h2>
    {resource_section_html}

    {cross_trial_section_html}

    <h2 id="conclusions">Conclusions</h2>
    {conclusions_html}

    <details id="manifest" class="manifest-details">
      <summary>Artifact manifest (trimmed JSON)</summary>
      <pre>{html_escape(artifacts_preview)}</pre>
    </details>

    <p class="note">
      Generated locally — commit under <code>docs/benchmark/results/</code> if you want this pinned alongside the tarball.
    </p>
  </div>
  <script>
    const throughput = {chart_json};
    const storage = {storage_chart_json};
    const resourceSeries = {resource_series_json};
    const throughputHints = {throughput_hints_json};
    const storageHints = {storage_hints_json};
    const phaseMarkersVisual = {phase_visual_json};
    const sampleInterval = {sample_interval_json};
    const rollupCompare = {rollup_compare_json};
    const modeSpans = {mode_spans_json};
    const cpuTsHint =
      'Mean/max of docker stats CPU%% samples between before_snapshot and after_snapshot for that mode (whole-run timeseries).';

    Chart.defaults.color = '#cbd5e1';
    Chart.defaults.borderColor = 'rgba(148,163,184,0.25)';
    Chart.defaults.font.family = "ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial";

    const tooltipBase = {{
      backgroundColor: 'rgba(15, 23, 42, 0.96)',
      padding: 12,
      borderColor: 'rgba(148, 163, 184, 0.38)',
      borderWidth: 1,
      titleSpacing: 6,
      bodySpacing: 6,
      footerSpacing: 8,
      displayColors: true,
    }};

    function buildLineXY(labels, ys) {{
      const out = [];
      if (!labels || !ys) return out;
      for (let i = 0; i < labels.length; i++) {{
        const y = ys[i];
        if (y === null || y === undefined || Number.isNaN(Number(y))) continue;
        out.push({{ x: Number(labels[i]), y: Number(y) }});
      }}
      return out;
    }}

    function phaseTooltipFooterLines(xVal) {{
      const iv = Number(sampleInterval) || 2;
      const markers = phaseMarkersVisual || [];
      const lines = [];
      const near = markers.filter((m) => Math.abs(m.elapsed_s - xVal) <= iv * 1.35);
      if (near.length) {{
        lines.push('Phase markers near this t:');
        near
          .slice()
          .sort((a, b) => a.elapsed_s - b.elapsed_s)
          .forEach((m) => lines.push('  • ' + m.label + ' @ t=' + String(m.elapsed_s) + 's'));
      }}
      const past = markers.filter((m) => m.elapsed_s <= xVal + 1e-6);
      past.sort((a, b) => b.elapsed_s - a.elapsed_s);
      if (past.length) {{
        lines.push(
          'Dominant phase at this t: ' + past[0].label + ' (started @ t=' + String(past[0].elapsed_s) + 's)'
        );
      }}
      return lines;
    }}

    function maxBarDataset(data) {{
      let m = 0;
      if (!data || !data.datasets) return 0;
      for (const ds of data.datasets) {{
        if (!ds.data) continue;
        for (const x of ds.data) {{
          const n = Number(x);
          if (Number.isFinite(n) && n > m) m = n;
        }}
      }}
      return m;
    }}

    function maxNumeric(arr) {{
      let m = 0;
      if (!arr) return 0;
      for (const x of arr) {{
        const n = Number(x);
        if (Number.isFinite(n) && n > m) m = n;
      }}
      return m;
    }}

    function maxOfArrays(...arrays) {{
      let m = 0;
      for (const arr of arrays) {{
        const x = maxNumeric(arr);
        if (x > m) m = x;
      }}
      return m;
    }}

    /** Pad ~15% then round to 1/2/5 × 10^n so small MB/s runs don't get a wastefully tall Y axis. */
    function niceCeil(v, pad) {{
      pad = pad !== undefined ? pad : 1.15;
      if (!Number.isFinite(v) || v <= 0) return undefined;
      const m = v * pad;
      const exp = Math.floor(Math.log10(m));
      const pow = Math.pow(10, exp);
      const frac = m / pow;
      const nice = frac <= 1 ? 1 : frac <= 2 ? 2 : frac <= 5 ? 5 : 10;
      return nice * pow;
    }}

    function axisMax(dataMax, fallback) {{
      const n = niceCeil(dataMax);
      return n !== undefined ? n : fallback;
    }}

    const benchModeBands = {{
      id: 'benchModeBands',
      beforeDatasetsDraw(chart) {{
        const spans = chart.options.plugins?.benchModeBands?.spans;
        if (!spans || !spans.length) return;
        const {{ ctx, chartArea, scales }} = chart;
        const xScale = scales.x;
        if (!xScale || xScale.type !== 'linear') return;
        spans.forEach((span) => {{
          const x0 = xScale.getPixelForValue(span.t0);
          const x1 = xScale.getPixelForValue(span.t1);
          const left = Math.min(x0, x1);
          const w = Math.abs(x1 - x0);
          if (w < 0.5) return;
          ctx.save();
          ctx.fillStyle = span.color || 'rgba(148, 163, 184, 0.12)';
          ctx.fillRect(left, chartArea.top, w, chartArea.bottom - chartArea.top);
          ctx.fillStyle = 'rgba(226, 232, 240, 0.9)';
          ctx.font = '600 11px system-ui,sans-serif';
          const txt = span.label || span.mode || '';
          const tx = Math.min(Math.max(left + 5, chartArea.left + 4), chartArea.right - 120);
          ctx.fillText(txt, tx, chartArea.top + 14);
          ctx.restore();
        }});
      }},
    }};
    Chart.register(benchModeBands);

    const benchPhaseLines = {{
      id: 'benchPhaseLines',
      afterDatasetsDraw(chart) {{
        const markers = chart.options.plugins?.benchPhaseLines?.markers;
        if (!markers || !markers.length) return;
        const {{ ctx, chartArea, scales }} = chart;
        const xScale = scales.x;
        if (!xScale || xScale.type !== 'linear') return;
        ctx.save();
        markers.forEach((m, i) => {{
          const px = xScale.getPixelForValue(m.elapsed_s);
          if (px < chartArea.left || px > chartArea.right) return;
          ctx.beginPath();
          ctx.strokeStyle = i % 2 === 0 ? 'rgba(251, 191, 36, 0.82)' : 'rgba(96, 165, 250, 0.82)';
          ctx.lineWidth = 1.5;
          ctx.setLineDash([6, 5]);
          ctx.moveTo(px, chartArea.top);
          ctx.lineTo(px, chartArea.bottom);
          ctx.stroke();
        }});
        ctx.setLineDash([]);
        ctx.restore();
      }},
    }};
    Chart.register(benchPhaseLines);

    const lineDatasetOpts = {{
      pointRadius: 0,
      pointHoverRadius: 6,
      pointHoverBorderWidth: 2,
      tension: 0.22,
      spanGaps: false,
    }};

    const lineInteraction = {{
      mode: 'nearest',
      intersect: false,
      axis: 'x',
    }};

    const tpYMax = axisMax(maxBarDataset(throughput), 10);
    const stYMax = axisMax(maxBarDataset(storage), 1);

    new Chart(document.getElementById('throughput'), {{
      type: 'bar',
      data: throughput,
      options: {{
        responsive: true,
        interaction: {{ mode: 'index', intersect: false }},
        plugins: {{
          tooltip: {{
            ...tooltipBase,
            callbacks: {{
              afterBody: (items) => {{
                if (!items || !items.length) return '';
                const idx = items[0].dataIndex;
                const g = throughputHints[idx];
                return g ? '\\n' + g : '';
              }},
            }},
          }},
          legend: {{ position: 'bottom' }},
          title: {{ display: true, text: 'Throughput (MB/s)', color: '#e5e7eb', font: {{ size: 15 }} }},
        }},
        scales: {{
          x: {{ stacked: false }},
          y: {{
            beginAtZero: true,
            suggestedMax: tpYMax,
            title: {{ display: true, text: 'MB/s' }},
          }},
        }},
      }},
    }});

    new Chart(document.getElementById('storage'), {{
      type: 'bar',
      data: storage,
      options: {{
        responsive: true,
        interaction: {{ mode: 'index', intersect: false }},
        plugins: {{
          tooltip: {{
            ...tooltipBase,
            callbacks: {{
              afterBody: (items) => {{
                if (!items || !items.length) return '';
                const idx = items[0].dataIndex;
                const g = storageHints[idx];
                return g ? '\\n' + g : '';
              }},
            }},
          }},
          legend: {{ position: 'bottom' }},
          title: {{ display: true, text: 'Storage (GB)', color: '#e5e7eb', font: {{ size: 15 }} }},
        }},
        scales: {{
          x: {{ stacked: false }},
          y: {{
            beginAtZero: true,
            suggestedMax: stYMax,
            title: {{ display: true, text: 'GB' }},
          }},
        }},
      }},
    }});

    const rollupSnapHint = 'End-of-mode snapshot from resources_rollup (same source as footprint table above).';
    if (rollupCompare.labels && rollupCompare.labels.length > 0) {{
      const rc = rollupCompare;
      const cmpTT = {{
        ...tooltipBase,
        callbacks: {{
          afterBody: () => '\\n' + rollupSnapHint,
        }},
      }};
      const cmpRssMax = axisMax(maxNumeric(rc.rss_mb), 128);
      const cpuUseTs = Boolean(rc.docker_cpu_timeseries && rc.docker_cpu_mean && rc.docker_cpu_mean.length > 0);
      const cmpCpuMax = cpuUseTs
        ? axisMax(Math.max(maxNumeric(rc.docker_cpu_mean), maxNumeric(rc.docker_cpu_max)), 150)
        : Math.min(100, axisMax(maxNumeric(rc.docker_cpu), 100));
      const cmpCpuTT = {{
        ...tooltipBase,
        callbacks: {{
          afterBody: () => '\\n' + (cpuUseTs ? cpuTsHint : rollupSnapHint),
        }},
      }};
      const cmpDiskBarMax = axisMax(Math.max(maxNumeric(rc.plain_mb), maxNumeric(rc.encrypted_mb)), 256);
      new Chart(document.getElementById('resCmpRss'), {{
        type: 'bar',
        data: {{
          labels: rc.labels,
          datasets: [{{
            label: 'Peak RSS (MB)',
            data: rc.rss_mb,
            backgroundColor: 'rgba(59, 130, 246, 0.72)',
          }}],
        }},
        options: {{
          responsive: true,
          interaction: {{ mode: 'index', intersect: false }},
          plugins: {{
            tooltip: cmpTT,
            legend: {{ display: false }},
            title: {{ display: true, text: 'Peak proxy RSS — compare modes', color: '#e5e7eb', font: {{ size: 14 }} }},
          }},
          scales: {{
            y: {{
              beginAtZero: true,
              suggestedMax: cmpRssMax,
              title: {{ display: true, text: 'MB' }},
            }},
          }},
        }},
      }});
      new Chart(document.getElementById('resCmpCpu'), {{
        type: 'bar',
        data: cpuUseTs
          ? {{
              labels: rc.labels,
              datasets: [
                {{
                  label: 'Docker CPU % mean',
                  data: rc.docker_cpu_mean,
                  backgroundColor: 'rgba(250, 204, 21, 0.62)',
                }},
                {{
                  label: 'Docker CPU % max',
                  data: rc.docker_cpu_max,
                  backgroundColor: 'rgba(251, 146, 60, 0.88)',
                }},
              ],
            }}
          : {{
              labels: rc.labels,
              datasets: [
                {{
                  label: 'Docker CPU % (idle snapshot)',
                  data: rc.docker_cpu,
                  backgroundColor: 'rgba(250, 204, 21, 0.72)',
                }},
              ],
            }},
        options: {{
          responsive: true,
          interaction: {{ mode: 'index', intersect: false }},
          plugins: {{
            tooltip: cmpCpuTT,
            legend: {{ display: cpuUseTs }},
            title: {{
              display: true,
              text: cpuUseTs ? 'Docker CPU % — mean & max during each mode window' : 'Docker CPU % — idle snapshot only',
              color: '#e5e7eb',
              font: {{ size: 14 }},
            }},
          }},
          scales: {{
            x: {{ stacked: false }},
            y: {{
              beginAtZero: true,
              suggestedMax: cmpCpuMax,
              title: {{ display: true, text: 'CPU %' }},
            }},
          }},
        }},
      }});
      new Chart(document.getElementById('resCmpDisk'), {{
        type: 'bar',
        data: {{
          labels: rc.labels,
          datasets: [
            {{
              label: 'Plain backend (MB)',
              data: rc.plain_mb,
              backgroundColor: 'rgba(34, 197, 94, 0.65)',
            }},
            {{
              label: 'Encrypted backend (MB)',
              data: rc.encrypted_mb,
              backgroundColor: 'rgba(239, 68, 68, 0.62)',
            }},
          ],
        }},
        options: {{
          responsive: true,
          interaction: {{ mode: 'index', intersect: false }},
          plugins: {{
            tooltip: cmpTT,
            legend: {{ position: 'bottom' }},
            title: {{ display: true, text: 'Backend du — compare modes', color: '#e5e7eb', font: {{ size: 14 }} }},
          }},
          scales: {{
            x: {{ stacked: false }},
            y: {{
              beginAtZero: true,
              suggestedMax: cmpDiskBarMax,
              title: {{ display: true, text: 'MB (du -sb)' }},
            }},
          }},
        }},
      }});
    }}

    if (resourceSeries.labels && resourceSeries.labels.length > 0) {{
      const labels = resourceSeries.labels;
      const mkFooter = (items) => {{
        if (!items || !items.length) return '';
        const x = items[0].parsed.x;
        const extra = phaseTooltipFooterLines(x);
        return extra.length ? '\\n' + extra.join('\\n') : '';
      }};
      const linePlugins = {{
        benchPhaseLines: {{ markers: phaseMarkersVisual }},
        benchModeBands: {{ spans: modeSpans }},
        tooltip: {{
          ...tooltipBase,
          callbacks: {{
            footer: mkFooter,
          }},
        }},
        legend: {{ position: 'bottom' }},
      }};

      const tsCpuMax = axisMax(maxNumeric(resourceSeries.cpu_pct), 150);
      const tsRamMax = axisMax(maxOfArrays(resourceSeries.rss_mb, resourceSeries.docker_mem_mb), 512);
      const tsDiskMax = axisMax(
        Math.max(
          maxNumeric(resourceSeries.disk_plain_mb),
          maxNumeric(resourceSeries.disk_encrypted_mb),
          maxNumeric(resourceSeries.disk_total_mb)
        ),
        256
      );

      new Chart(document.getElementById('resCpu'), {{
        type: 'line',
        data: {{
          datasets: [
            {{
              label: 'Docker CPU %',
              data: buildLineXY(labels, resourceSeries.cpu_pct),
              borderColor: 'rgba(250, 204, 21, 0.95)',
              backgroundColor: 'rgba(250, 204, 21, 0.22)',
              ...lineDatasetOpts,
            }},
          ],
        }},
        options: {{
          responsive: true,
          parsing: false,
          interaction: lineInteraction,
          plugins: {{
            ...linePlugins,
            title: {{ display: false }},
          }},
          scales: {{
            x: {{
              type: 'linear',
              title: {{ display: true, text: 'Elapsed seconds (t=0 at benchmark loop start)' }},
              grid: {{ color: 'rgba(148,163,184,0.08)' }},
            }},
            y: {{
              beginAtZero: true,
              suggestedMax: tsCpuMax,
              title: {{ display: true, text: 'CPU %' }},
              grid: {{ color: 'rgba(148,163,184,0.08)' }},
            }},
          }},
        }},
      }});

      new Chart(document.getElementById('resRam'), {{
        type: 'line',
        data: {{
          datasets: [
            {{
              label: 'Proxy RSS (MB)',
              data: buildLineXY(labels, resourceSeries.rss_mb),
              borderColor: 'rgba(59, 130, 246, 0.95)',
              backgroundColor: 'rgba(59, 130, 246, 0.22)',
              ...lineDatasetOpts,
            }},
            {{
              label: 'Docker used memory (MB)',
              data: buildLineXY(labels, resourceSeries.docker_mem_mb),
              borderColor: 'rgba(167, 139, 250, 0.95)',
              backgroundColor: 'rgba(167, 139, 250, 0.18)',
              ...lineDatasetOpts,
            }},
          ],
        }},
        options: {{
          responsive: true,
          parsing: false,
          interaction: lineInteraction,
          plugins: {{
            ...linePlugins,
            title: {{ display: false }},
          }},
          scales: {{
            x: {{
              type: 'linear',
              title: {{ display: true, text: 'Elapsed seconds (t=0 at benchmark loop start)' }},
              grid: {{ color: 'rgba(148,163,184,0.08)' }},
            }},
            y: {{
              beginAtZero: true,
              suggestedMax: tsRamMax,
              title: {{ display: true, text: 'MB (MiB-scale proxy RSS vs Docker usage)' }},
              grid: {{ color: 'rgba(148,163,184,0.08)' }},
            }},
          }},
        }},
      }});

      new Chart(document.getElementById('resDisk'), {{
        type: 'line',
        data: {{
          datasets: [
            {{
              label: 'Disk plain backend (MB)',
              data: buildLineXY(labels, resourceSeries.disk_plain_mb),
              borderColor: 'rgba(34, 197, 94, 0.95)',
              backgroundColor: 'rgba(34, 197, 94, 0.18)',
              ...lineDatasetOpts,
            }},
            {{
              label: 'Disk encrypted backend (MB)',
              data: buildLineXY(labels, resourceSeries.disk_encrypted_mb),
              borderColor: 'rgba(239, 68, 68, 0.95)',
              backgroundColor: 'rgba(239, 68, 68, 0.18)',
              ...lineDatasetOpts,
            }},
            {{
              label: 'Disk total (MB)',
              data: buildLineXY(labels, resourceSeries.disk_total_mb),
              borderColor: 'rgba(148, 163, 184, 0.95)',
              backgroundColor: 'rgba(148, 163, 184, 0.15)',
              ...lineDatasetOpts,
            }},
          ],
        }},
        options: {{
          responsive: true,
          parsing: false,
          interaction: lineInteraction,
          plugins: {{
            ...linePlugins,
            title: {{ display: false }},
          }},
          scales: {{
            x: {{
              type: 'linear',
              title: {{ display: true, text: 'Elapsed seconds (t=0 at benchmark loop start)' }},
              grid: {{ color: 'rgba(148,163,184,0.08)' }},
            }},
            y: {{
              beginAtZero: true,
              suggestedMax: tsDiskMax,
              title: {{ display: true, text: 'MB (du -sb per backend root)' }},
              grid: {{ color: 'rgba(148,163,184,0.08)' }},
            }},
          }},
        }},
      }});
    }}
  </script>
</body>
</html>
"""


def _storage_table_rows(rows: list[dict[str, Any]]) -> str:
    parts = []
    for r in rows:
        parts.append(
            "<tr>"
            f"<td><code>{html_escape(r['mode'])}</code></td>"
            f"<td>{fmt_gb(r['logical_gb'])}</td>"
            f"<td>{fmt_gb(r['delta_saved_gb'])}</td>"
            f"<td>{fmt_gb(r['implied_stored_gb'])}</td>"
            f"<td>{r['savings_pct']:.1f}</td>"
            f"<td>{r['delta_encode_events']}</td>"
            "</tr>"
        )
    return "\n".join(parts)


def generate_html_report(bundle: str | Path, out: str | Path) -> Path:
    bundle_path = Path(bundle)
    summary, artifacts, prom, resource_timeseries = load_bundle(bundle_path)
    run_id = summary.get("run_id", bundle_path.stem)
    mode_metrics = compute_mode_metrics(prom)
    logical_b = logical_bytes(artifacts, summary)
    rollup = summary.get("resources_rollup")
    agg_path = bundle_path / "aggregate.json"
    trial_aggregate = json.loads(agg_path.read_text()) if agg_path.exists() else None
    html = render_html(run_id, summary, artifacts, mode_metrics, logical_b, rollup, resource_timeseries, trial_aggregate)
    out_path = Path(out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(html, encoding="utf-8")
    return out_path


def html_report_main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description="Render HTML report from benchmark bundle")
    p.add_argument("--bundle", required=True, help="Path to extracted result dir or .tgz")
    p.add_argument("--out", required=True, help="Output report.html path")
    args = p.parse_args(argv)
    out = generate_html_report(args.bundle, args.out)
    print(f"wrote {out}")
    return 0
