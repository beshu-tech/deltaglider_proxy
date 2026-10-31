/**
 * Pure Prometheus text helpers for the dashboard (no React).
 *
 * A histogram of the proxy is often a VEC: one series per label set
 * (`deltaglider_http_request_duration_seconds{method,operation}`). Every
 * helper here sums the series first. Reading only the first series gave
 * a wrong average, and concatenating the series gave the latency chart
 * the x labels "5ms 10ms 5ms 10ms 25ms …" (UX harvest D42).
 */

interface Sample {
  /** The sample name as written: `x`, `x_bucket`, `x_sum`, `x_count`. */
  name: string;
  labels: Record<string, string>;
  value: number;
}

export interface ParsedMetric {
  name: string;
  help: string;
  type: string;
  samples: Sample[];
}

export function parsePrometheus(text: string): Map<string, ParsedMetric> {
  const metrics = new Map<string, ParsedMetric>();
  let current: ParsedMetric | null = null;
  const finalize = () => {
    if (current && current.samples.length > 0) metrics.set(current.name, current);
    current = null;
  };
  for (const line of text.split('\n')) {
    if (line.startsWith('# HELP ')) {
      finalize();
      const rest = line.slice(7), sp = rest.indexOf(' ');
      current = { name: rest.slice(0, sp), help: rest.slice(sp + 1), type: 'untyped', samples: [] };
    } else if (line.startsWith('# TYPE ')) {
      const rest = line.slice(7), sp = rest.indexOf(' ');
      if (current) current.type = rest.slice(sp + 1);
    } else if (line && !line.startsWith('#')) {
      const braceIdx = line.indexOf('{');
      let name: string, valueStr: string;
      const labels: Record<string, string> = {};
      if (braceIdx >= 0) {
        name = line.slice(0, braceIdx);
        const closeIdx = line.indexOf('}', braceIdx);
        for (const m of line.slice(braceIdx + 1, closeIdx).matchAll(/(\w+)="([^"]*)"/g)) labels[m[1]] = m[2];
        valueStr = line.slice(closeIdx + 2);
      } else {
        const sp = line.indexOf(' ');
        name = line.slice(0, sp);
        valueStr = line.slice(sp + 1);
      }
      const value = parseFloat(valueStr);
      if (current && (name === current.name || name.startsWith(current.name + '_'))) {
        current.samples.push({ name, labels, value });
      } else {
        const existing = metrics.get(name);
        if (existing) existing.samples.push({ name, labels, value });
        else metrics.set(name, { name, help: '', type: 'untyped', samples: [{ name, labels, value }] });
      }
    }
  }
  finalize();
  return metrics;
}

/** The value of a gauge or counter: the unlabelled sample, else the first. */
export function val(m: Map<string, ParsedMetric>, name: string): number {
  const metric = m.get(name);
  if (!metric?.samples.length) return 0;
  const simple = metric.samples.find((s) => Object.keys(s.labels).length === 0);
  return simple?.value ?? metric.samples[0].value;
}

/** Sum, count and average of a histogram, over all its series. */
export function histStats(m: Map<string, ParsedMetric>, name: string): { sum: number; count: number; avg: number } {
  const metric = m.get(name);
  if (!metric) return { sum: 0, count: 0, avg: 0 };
  let sum = 0, count = 0;
  for (const s of metric.samples) {
    if (s.name === `${name}_sum`) sum += s.value;
    else if (s.name === `${name}_count`) count += s.value;
  }
  return { sum, count, avg: count > 0 ? sum / count : 0 };
}

/** One bar of a histogram chart: a labelled range and how many fall in it. */
interface HistogramBar {
  range: string;
  count: number;
}

/**
 * Pure: the bars of a histogram, one per bucket RANGE, over all series.
 * Cumulative `le` counts are summed per bound, sorted, and differenced.
 * Labels are ranges ("<5 ms", "5–10 ms", "≥ 10 s"), never a repeated
 * bound. Empty buckets before the first and after the last count are
 * dropped; empty buckets between them stay, so the shape is true.
 */
export function histogramBars(
  m: Map<string, ParsedMetric>,
  name: string,
  fmt: (bound: number) => string,
): HistogramBar[] {
  const metric = m.get(name);
  if (!metric) return [];
  const byBound = new Map<number, number>();
  for (const s of metric.samples) {
    if (s.name !== `${name}_bucket` || !('le' in s.labels)) continue;
    const bound = s.labels.le === '+Inf' ? Infinity : parseFloat(s.labels.le);
    if (Number.isNaN(bound)) continue;
    byBound.set(bound, (byBound.get(bound) ?? 0) + s.value);
  }
  const bounds = [...byBound.keys()].sort((a, b) => a - b);
  const bars: HistogramBar[] = [];
  let prevCount = 0;
  let prevBound: number | null = null;
  for (const bound of bounds) {
    const cum = byBound.get(bound) ?? 0;
    const count = Math.max(0, cum - prevCount);
    const range =
      prevBound === null
        ? `<${fmt(bound)}`
        : bound === Infinity
          ? `≥${fmt(prevBound)}`
          : rangeLabel(fmt(prevBound), fmt(bound));
    bars.push({ range, count });
    prevCount = cum;
    prevBound = bound;
  }
  const first = bars.findIndex((b) => b.count > 0);
  if (first < 0) return [];
  let last = bars.length - 1;
  while (bars[last].count === 0) last--;
  return bars.slice(first, last + 1);
}

/** "5 ms" + "10 ms" → "5–10 ms"; "500 ms" + "1 s" → "500 ms–1 s". */
function rangeLabel(lo: string, hi: string): string {
  const unit = (s: string) => /[^\d.]+$/.exec(s)?.[0] ?? '';
  const u = unit(lo);
  if (u && u === unit(hi)) return `${lo.slice(0, -u.length)}–${hi}`;
  return `${lo}–${hi}`;
}

/** A latency bound in seconds as "5 ms" / "2.5 s". */
export function fmtLatencyBound(seconds: number): string {
  if (seconds < 1) return `${+(seconds * 1000).toFixed(1)} ms`;
  return `${+seconds.toFixed(2)} s`;
}

/** A compression ratio bound (0–1) as "5%". */
export function fmtRatioBound(ratio: number): string {
  return `${+(ratio * 100).toFixed(1)}%`;
}

/** Pure: label → value pairs of a labelled counter, summed per label, zeros dropped, largest first. */
export function nonZeroByLabel(m: Map<string, ParsedMetric>, name: string, labelKey: string): { name: string; value: number }[] {
  const metric = m.get(name);
  if (!metric) return [];
  const result: Record<string, number> = {};
  for (const s of metric.samples) {
    const k = s.labels[labelKey] || 'unknown';
    result[k] = (result[k] ?? 0) + s.value;
  }
  return Object.entries(result)
    .filter(([, v]) => v > 0)
    .map(([k, v]) => ({ name: k, value: v }))
    .sort((a, b) => b.value - a.value);
}
