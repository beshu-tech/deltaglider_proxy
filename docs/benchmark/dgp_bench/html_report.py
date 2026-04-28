from __future__ import annotations

import argparse
import json
import re
import tarfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any


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


def load_bundle(path: Path) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]]]:
    """Return (summary, artifacts, raw_prom_by_mode_conc)."""
    if path.is_file() and path.suffix == ".tgz":
        return _load_from_tgz(path)
    if path.is_dir():
        return _load_from_dir(path)
    raise SystemExit(f"Expected directory or .tgz bundle, got {path}")


def _load_from_dir(root: Path) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]]]:
    summary = json.loads((root / "summary.json").read_text())
    artifacts = json.loads((root / "artifacts.json").read_text())
    prom = _gather_prom(root)
    return summary, artifacts, prom


def _load_from_tgz(tgz: Path) -> tuple[dict[str, Any], list[dict[str, Any]], dict[str, dict[str, dict[str, Any]]]]:
    root_name = None
    with tarfile.open(tgz, "r:gz") as tf:
        for m in tf.getmembers():
            if m.name.endswith("/summary.json"):
                root_name = Path(m.name).parts[0]
                break
        if not root_name:
            raise SystemExit("Could not locate summary.json inside archive")
        def read_member(rel: str) -> str:
            m = tf.extractfile(f"{root_name}/{rel}")
            if m is None:
                raise SystemExit(f"missing {rel} in archive")
            return m.read().decode()

        summary = json.loads(read_member("summary.json"))
        artifacts = json.loads(read_member("artifacts.json"))
        prom: dict[str, dict[str, dict[str, Any]]] = {}
        for m in tf.getmembers():
            if not m.isfile():
                continue
            parts = Path(m.name).parts
            if len(parts) < 4 or parts[0] != root_name:
                continue
            mode, conc, fname = parts[1], parts[2], parts[3]
            if not fname.endswith(".prom"):
                continue
            body = tf.extractfile(m).read().decode("utf-8", "replace")
            kind = "before" if fname.startswith("before_") else "after" if fname.startswith("after_") else "unknown"
            prom.setdefault(mode, {}).setdefault(conc, {})[kind] = body
        return summary, artifacts, prom


def _gather_prom(root: Path) -> dict[str, dict[str, dict[str, Any]]]:
    prom: dict[str, dict[str, dict[str, Any]]] = {}
    for p in root.glob("*/*/before_*.prom"):
        parts = p.parts[-3:]
        mode, conc, fname = parts
        kind = "before"
        prom.setdefault(mode, {}).setdefault(conc, {})[kind] = p.read_text()
    for p in root.glob("*/*/after_*.prom"):
        parts = p.parts[-3:]
        mode, conc, fname = parts
        kind = "after"
        prom.setdefault(mode, {}).setdefault(conc, {})[kind] = p.read_text()
    return prom


def compute_mode_metrics(prom_trees: dict[str, dict[str, dict[str, Any]]]) -> dict[str, ModeMetrics]:
    out: dict[str, ModeMetrics] = {}
    for mode, by_conc in prom_trees.items():
        # Primary concurrency is usually c1
        conc = "c1" if "c1" in by_conc else sorted(by_conc.keys())[0]
        blob_before = by_conc.get(conc, {}).get("before", "")
        blob_after = by_conc.get(conc, {}).get("after", "")
        before_saved = parse_prom_counter(blob_before, "deltaglider_delta_bytes_saved_total") or 0.0
        after_saved = parse_prom_counter(blob_after, "deltaglider_delta_bytes_saved_total") or 0.0
        before_enc = parse_prom_counter(blob_before, "deltaglider_delta_encode_duration_seconds_count") or 0.0
        after_enc = parse_prom_counter(blob_after, "deltaglider_delta_encode_duration_seconds_count") or 0.0
        out[mode] = ModeMetrics(
            mode=mode,
            delta_saved_increment=max(0.0, after_saved - before_saved),
            encode_count_delta=max(0.0, after_enc - before_enc),
        )
    return out


def logical_bytes(artifacts: list[dict[str, Any]], summary: dict[str, Any]) -> int:
    if artifacts:
        return int(sum(int(a["bytes"]) for a in artifacts))
    modes = summary.get("modes") or {}
    first = next(iter(modes.values()))
    c = next(iter(first.values()))
    return int(c["put"]["bytes"])


def render_html(
    run_id: str,
    summary: dict[str, Any],
    artifacts: list[dict[str, Any]],
    mode_metrics: dict[str, ModeMetrics],
    logical_b: int,
) -> str:
    modes_order = ["passthrough", "compression", "encryption", "compression_encryption"]
    chart_labels = []
    put_mbps = []
    cold_mbps = []
    warm_mbps = []

    storage_rows: list[dict[str, Any]] = []

    for mode in modes_order:
        by_conc = (summary.get("modes") or {}).get(mode) or {}
        conc_key = "c1" if "c1" in by_conc else sorted(by_conc.keys())[0]
        phases = by_conc[conc_key]
        label = mode.replace("_", " ")
        chart_labels.append(label)
        put_mbps.append(round(phases["put"]["mb_s"], 2))
        cold_mbps.append(round(phases["cold_get"]["mb_s"], 2))
        warm_mbps.append(round(phases["warm_get"]["mb_s"], 2))

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

    artifacts_preview = json.dumps(artifacts[:12], indent=2)

    conclusions: list[str] = []
    # Use compression row specifically for savings statement
    comp = mode_metrics.get("compression")
    if comp and comp.delta_saved_increment <= 0 and (comp.encode_count_delta <= 0):
        conclusions.append(
            "<strong>No delta storage win detected.</strong> Prometheus shows zero delta encode events and no increase "
            "in <code>deltaglider_delta_bytes_saved_total</code> during compression phases. Typical causes: ISO contents "
            "did not beat <code>max_delta_ratio</code> (passthrough fallback), or the running proxy build predates "
            "<code>.iso</code> eligibility in <code>FileRouter</code>."
        )
    elif comp and comp.delta_saved_increment > 0:
        conclusions.append(
            f"<strong>Compression reduced stored bytes.</strong> Approximately <strong>{comp.delta_saved_increment / 1e6:.2f} MB</strong> "
            f"saved vs logical uploads (<strong>{comp.delta_saved_increment / logical_b * 100:.2f}%</strong> of logical bytes)."
        )

    passthrough_put = summary["modes"]["passthrough"]["c1"]["put"]["mb_s"]
    compression_put = summary["modes"]["compression"]["c1"]["put"]["mb_s"]
    enc_put = summary["modes"]["encryption"]["c1"]["put"]["mb_s"]
    conclusions.append(
        f"PUT throughput drops from <strong>{passthrough_put:.1f} MB/s</strong> (passthrough) to "
        f"<strong>{compression_put:.1f} MB/s</strong> (compression) — delta encode is CPU-heavy vs streaming writes."
    )
    conclusions.append(
        f"The encrypted backend bucket measured <strong>{enc_put:.1f} MB/s</strong> PUT vs passthrough — AES-GCM adds "
        "measurable CPU cost while preserving logical bytes on read."
    )

    conclusions_html = "\n".join(f"<p>{c}</p>" for c in conclusions)

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
        meta_dataset = f"<p><strong>Artifacts ({len(names)}):</strong> <code>{html_escape(', '.join(names))}</code></p>"

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

    <div class="grid">
      <div class="card">
        <h3>Logical payload</h3>
        <div class="metric">{logical_b / 1e9:.3f} GB</div>
        <div class="note">Sum of upstream artifact sizes (client-visible bytes).</div>
      </div>
      <div class="card">
        <h3>Best PUT</h3>
        <div class="metric">{max(put_mbps):.1f} MB/s</div>
        <div class="note">Highest PUT throughput among modes (chart below).</div>
      </div>
      <div class="card">
        <h3>Delta savings (compression phase)</h3>
        <div class="metric">{storage_rows[1]["savings_pct"]:.2f}%</div>
        <div class="note">Prometheus Δ during <code>compression</code> mode only.</div>
      </div>
    </div>

    {meta_dataset}

    <div class="chart-wrap">
      <canvas id="throughput"></canvas>
    </div>

    <h2>Throughput by mode</h2>
    <p class="note">MB/s computed by the benchmark harness from timed PUT/GET operations.</p>

    <div class="chart-wrap">
      <canvas id="storage"></canvas>
    </div>

    <h2>Storage view (logical vs implied stored)</h2>
    <p class="note">
      <strong>Implied stored</strong> = logical bytes − Δ<code>deltaglider_delta_bytes_saved_total</code> for that mode.
      Encryption modes still verify SHA-256 against logical bytes on GET; ciphertext size on disk is not exported here — use
      <code>/_/stats?bucket=…</code> from an authenticated admin session if you need exact ciphertext footprint.
    </p>

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

    <h2>Conclusions</h2>
    {conclusions_html}

    <h2>Artifact manifest (trimmed)</h2>
    <pre>{html_escape(artifacts_preview)}</pre>

    <p class="note">
      Generated locally — commit under <code>docs/benchmark/results/</code> if you want this pinned alongside the tarball.
    </p>
  </div>
  <script>
    const throughput = {chart_json};
    const storage = {storage_chart_json};
    Chart.defaults.color = '#cbd5e1';
    Chart.defaults.borderColor = 'rgba(148,163,184,0.25)';
    new Chart(document.getElementById('throughput'), {{
      type: 'bar',
      data: throughput,
      options: {{
        responsive: true,
        plugins: {{
          legend: {{ position: 'bottom' }},
          title: {{ display: true, text: 'Throughput (MB/s)', color: '#e5e7eb', font: {{ size: 15 }} }},
        }},
        scales: {{
          x: {{ stacked: false }},
          y: {{ beginAtZero: true, title: {{ display: true, text: 'MB/s' }} }},
        }},
      }},
    }});
    new Chart(document.getElementById('storage'), {{
      type: 'bar',
      data: storage,
      options: {{
        responsive: true,
        plugins: {{
          legend: {{ position: 'bottom' }},
          title: {{ display: true, text: 'Storage (GB)', color: '#e5e7eb', font: {{ size: 15 }} }},
        }},
        scales: {{
          x: {{ stacked: false }},
          y: {{ beginAtZero: true }},
        }},
      }},
    }});
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
            f"<td>{r['logical_gb']:.6f}</td>"
            f"<td>{r['delta_saved_gb']:.8f}</td>"
            f"<td>{r['implied_stored_gb']:.6f}</td>"
            f"<td>{r['savings_pct']:.4f}</td>"
            f"<td>{r['delta_encode_events']}</td>"
            "</tr>"
        )
    return "\n".join(parts)


def html_escape(s: str) -> str:
    return (
        s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
        .replace("'", "&#39;")
    )


def generate_html_report(bundle: str | Path, out: str | Path) -> Path:
    bundle_path = Path(bundle)
    summary, artifacts, prom = load_bundle(bundle_path)
    run_id = summary.get("run_id", bundle_path.stem)
    mode_metrics = compute_mode_metrics(prom)
    logical_b = logical_bytes(artifacts, summary)
    html = render_html(run_id, summary, artifacts, mode_metrics, logical_b)
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
