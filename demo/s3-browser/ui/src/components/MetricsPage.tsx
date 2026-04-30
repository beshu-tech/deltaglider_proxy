/**
 * MetricsPage — the admin Dashboard.
 *
 * Rewritten on a 12-column Grafana-style grid so panels scale with
 * the viewport instead of living inside a 860px straitjacket.
 * Every widget is a Panel on a DashboardGrid; density collapses
 * automatically on narrower containers.
 *
 * Data flow unchanged:
 *   - /_/metrics scraped every N seconds (cadence toggle)
 *   - /_/stats fetched every 60s (expensive scan)
 *   - /_/api/admin/config fetched once on mount
 *
 * The Prometheus parser, histogram helpers, and Snapshot ring all
 * stayed byte-for-byte identical — the rewrite is JSX-only. See
 * dashboard/README or the design notes in dashboard/Panel.tsx for
 * the grid semantics.
 */
import { useState, useEffect, useRef, useCallback } from 'react';
import { Typography, Spin, Progress } from 'antd';
import { useColors } from '../ThemeContext';
import { formatBytes } from '../utils';
import AnalyticsSection from './AnalyticsSection';
import { useAdminConfig } from '../queries/config';
import {
  AreaChart, Area, BarChart, Bar, Cell,
  XAxis, YAxis, Tooltip as RTooltip, ResponsiveContainer,
} from 'recharts';
import DashboardGrid from './dashboard/DashboardGrid';
import Panel from './dashboard/Panel';
import StatValue from './dashboard/StatValue';
import DashboardToolbar, { type RefreshCadence } from './dashboard/DashboardToolbar';
import { CHART_PALETTE, STATUS_COLORS, chartTooltipStyle, axisTickStyle, fmtDuration, fmtNum, fmtPct } from './dashboard/chartDefaults';
import './dashboard/dashboard.css';

const { Text } = Typography;

/* ═══════════════════════════════════════════════════════════
   Prometheus parser
   ═══════════════════════════════════════════════════════════ */

interface ParsedMetric {
  name: string;
  help: string;
  type: string;
  samples: { labels: Record<string, string>; value: number }[];
}

function parsePrometheus(text: string): Map<string, ParsedMetric> {
  const metrics = new Map<string, ParsedMetric>();
  let current: ParsedMetric | null = null;
  const finalize = () => { if (current && current.samples.length > 0) metrics.set(current.name, current); current = null; };
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
      } else { const sp = line.indexOf(' '); name = line.slice(0, sp); valueStr = line.slice(sp + 1); }
      const value = parseFloat(valueStr);
      if (current && (name === current.name || name.startsWith(current.name + '_'))) {
        current.samples.push({ labels, value });
      } else {
        const existing = metrics.get(name);
        if (existing) existing.samples.push({ labels, value });
        else metrics.set(name, { name, help: '', type: 'untyped', samples: [{ labels, value }] });
      }
    }
  }
  finalize();
  return metrics;
}

/* ═══════════════════════════════════════════════════════════
   Metric access helpers
   ═══════════════════════════════════════════════════════════ */

function val(m: Map<string, ParsedMetric>, name: string): number {
  const metric = m.get(name);
  if (!metric?.samples.length) return 0;
  const simple = metric.samples.find(s => Object.keys(s.labels).length === 0);
  return simple?.value ?? metric.samples[0].value;
}

function histStats(m: Map<string, ParsedMetric>, name: string) {
  const metric = m.get(name);
  if (!metric) return { sum: 0, count: 0, avg: 0 };
  const nonBucket = metric.samples.filter(s => !('le' in s.labels));
  const sum = nonBucket[0]?.value ?? 0, count = nonBucket[1]?.value ?? 0;
  return { sum, count, avg: count > 0 ? sum / count : 0 };
}

function histBuckets(m: Map<string, ParsedMetric>, name: string): { le: string; count: number }[] {
  const metric = m.get(name);
  if (!metric) return [];
  return metric.samples
    .filter(s => 'le' in s.labels && s.labels.le !== '+Inf')
    .map(s => ({ le: s.labels.le, count: s.value }));
}

function histDifferential(buckets: { le: string; count: number }[]): { range: string; count: number }[] {
  const result: { range: string; count: number }[] = [];
  let prev = 0;
  for (const b of buckets) {
    const diff = b.count - prev;
    if (diff > 0) result.push({ range: b.le, count: diff });
    prev = b.count;
  }
  return result;
}

function labeledValues(m: Map<string, ParsedMetric>, name: string, labelKey: string): Record<string, number> {
  const metric = m.get(name);
  if (!metric) return {};
  const result: Record<string, number> = {};
  for (const s of metric.samples) { const k = s.labels[labelKey] || 'unknown'; result[k] = (result[k] ?? 0) + s.value; }
  return result;
}

function multiLabelValues(m: Map<string, ParsedMetric>, name: string): { labels: Record<string, string>; value: number }[] {
  const metric = m.get(name);
  if (!metric) return [];
  return metric.samples.map(s => ({ labels: s.labels, value: s.value }));
}

/* ═══════════════════════════════════════════════════════════
   Snapshot ring
   ═══════════════════════════════════════════════════════════ */

interface Snapshot {
  t: string;
  cacheHits: number;
  cacheMisses: number;
  cacheUtil: number;
  httpTotal: number;
  avgLatency: number;
}

const MAX_HISTORY = 60;

interface StatsData {
  total_objects: number;
  total_original_size: number;
  total_stored_size: number;
  savings_percentage: number;
  truncated: boolean;
}

interface Props { onBack: () => void; embedded?: boolean; }

export default function MetricsPage({ onBack, embedded }: Props) {
  const colors = useColors();
  const [metricsMap, setMetricsMap] = useState<Map<string, ParsedMetric>>(new Map());
  const [stats, setStats] = useState<StatsData | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [cadence, setCadence] = useState<RefreshCadence>('5s');
  const [history, setHistory] = useState<Snapshot[]>([]);
  const [activeView, setActiveView] = useState<'monitoring' | 'analytics'>(() => {
    const saved = localStorage.getItem('dg-metrics-view');
    return saved === 'analytics' ? 'analytics' : 'monitoring';
  });
  // Admin config is shared across panels; the cached read deduplicates
  // with whoever else mounted recently (CredentialsModePanel, etc.).
  const { data: adminConfigData } = useAdminConfig();
  const adminConfig = adminConfigData ?? null;
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const prevRef = useRef<{ hits: number; misses: number; http: number; latencySum: number; latencyCount: number } | null>(null);

  const tt = chartTooltipStyle(colors);

  // Stats refresh — separate cadence (60s) because it's expensive.
  const statsTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const fetchStats = useCallback(async () => {
    try {
      const res = await fetch('/_/stats', { credentials: 'include' });
      if (res.ok) setStats(await res.json());
    } catch { /* non-blocking */ }
  }, []);

  useEffect(() => {
    fetchStats();
    statsTimerRef.current = setInterval(fetchStats, 60_000);
    return () => { if (statsTimerRef.current) clearInterval(statsTimerRef.current); };
  }, [fetchStats]);

  const fetchMetrics = useCallback(async () => {
    setRefreshing(true);
    try {
      const metricsRes = await fetch('/_/metrics', { credentials: 'include' });
      if (!metricsRes.ok) throw new Error(`HTTP ${metricsRes.status}`);
      const parsed = parsePrometheus(await metricsRes.text());
      setMetricsMap(parsed);

      const hits = val(parsed, 'deltaglider_cache_hits_total');
      const misses = val(parsed, 'deltaglider_cache_misses_total');
      const httpReqs = parsed.get('deltaglider_http_requests_total')?.samples.reduce((a, s) => a + s.value, 0) ?? 0;
      const latencyHist = histStats(parsed, 'deltaglider_http_request_duration_seconds');

      const snap: Snapshot = {
        t: new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }),
        cacheHits: prevRef.current ? Math.max(0, hits - prevRef.current.hits) : 0,
        cacheMisses: prevRef.current ? Math.max(0, misses - prevRef.current.misses) : 0,
        cacheUtil: val(parsed, 'deltaglider_cache_utilization_ratio') * 100,
        httpTotal: prevRef.current ? Math.max(0, httpReqs - prevRef.current.http) : 0,
        avgLatency: prevRef.current && latencyHist.count > prevRef.current.latencyCount
          ? ((latencyHist.sum - prevRef.current.latencySum) / (latencyHist.count - prevRef.current.latencyCount)) * 1000
          : 0,
      };
      prevRef.current = { hits, misses, http: httpReqs, latencySum: latencyHist.sum, latencyCount: latencyHist.count };
      setHistory(prev => [...prev, snap].slice(-MAX_HISTORY));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to fetch');
    } finally { setLoading(false); setRefreshing(false); }
  }, []);

  useEffect(() => { fetchMetrics(); }, [fetchMetrics]);
  useEffect(() => {
    if (cadence === 'off') return;
    const ms = cadence === '30s' ? 30_000 : 5_000;
    intervalRef.current = setInterval(fetchMetrics, ms);
    return () => { if (intervalRef.current) clearInterval(intervalRef.current); };
  }, [cadence, fetchMetrics]);

  if (loading) return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin description="Loading metrics..." /></div>;

  const m = metricsMap;

  // ── Derived values (same as before) ──
  const cacheUsed = val(m, 'deltaglider_cache_size_bytes'), cacheMax = val(m, 'deltaglider_cache_max_bytes');
  const cacheUtil = val(m, 'deltaglider_cache_utilization_ratio'), cacheMissRate = val(m, 'deltaglider_cache_miss_rate_ratio');
  const cacheEntries = val(m, 'deltaglider_cache_entries');
  const cacheHits = val(m, 'deltaglider_cache_hits_total'), cacheMisses = val(m, 'deltaglider_cache_misses_total');
  const cacheTotal = cacheHits + cacheMisses;

  const encodeStats = histStats(m, 'deltaglider_delta_encode_duration_seconds');
  const decodeStats = histStats(m, 'deltaglider_delta_decode_duration_seconds');
  const compressionHist = histStats(m, 'deltaglider_delta_compression_ratio');
  const decisions = labeledValues(m, 'deltaglider_delta_decisions_total', 'decision');
  const codecAvail = val(m, 'deltaglider_codec_semaphore_available');

  const compressionBuckets = histDifferential(histBuckets(m, 'deltaglider_delta_compression_ratio'))
    .map(b => ({ range: `${(parseFloat(b.range) * 100).toFixed(0)}%`, count: b.count }));

  const httpByOp: Record<string, number> = {};
  const httpByStatus: Record<string, number> = {};
  const httpSamples = multiLabelValues(m, 'deltaglider_http_requests_total');
  for (const s of httpSamples) {
    const op = s.labels.operation || 'unknown';
    httpByOp[op] = (httpByOp[op] ?? 0) + s.value;
    const status = s.labels.status?.[0] + 'xx' || 'unknown';
    httpByStatus[status] = (httpByStatus[status] ?? 0) + s.value;
  }
  const httpChartData = Object.entries(httpByOp).map(([name, value]) => ({ name, value })).sort((a, b) => b.value - a.value);
  const totalHttp = httpChartData.reduce((a, d) => a + d.value, 0);
  const errorRate = totalHttp > 0 ? ((httpByStatus['4xx'] ?? 0) + (httpByStatus['5xx'] ?? 0)) / totalHttp : 0;

  const latencyStats = histStats(m, 'deltaglider_http_request_duration_seconds');
  const reqSizeStats = histStats(m, 'deltaglider_http_request_size_bytes');
  const resSizeStats = histStats(m, 'deltaglider_http_response_size_bytes');

  const latencyBuckets = histDifferential(histBuckets(m, 'deltaglider_http_request_duration_seconds'))
    .map(b => {
      const v = parseFloat(b.range);
      return { range: v < 1 ? `${(v * 1000).toFixed(0)}ms` : `${v}s`, count: b.count };
    });

  const peakRss = val(m, 'process_peak_rss_bytes');
  const uptime = val(m, 'process_start_time_seconds');
  const uptimeStr = uptime > 0
    ? (() => { const s = Math.floor(Date.now() / 1000 - uptime); if (s < 60) return `${s}s`; if (s < 3600) return `${Math.floor(s / 60)}m`; const h = Math.floor(s / 3600); return `${h}h ${Math.floor((s % 3600) / 60)}m`; })()
    : '—';

  const buildMetric = m.get('deltaglider_build_info');
  const buildVersion = buildMetric?.samples[0]?.labels.version || '?';
  const backendType = buildMetric?.samples[0]?.labels.backend_type || '?';

  const authAttempts = m.get('deltaglider_auth_attempts_total')?.samples.reduce((a, s) => a + s.value, 0) ?? 0;
  const authFailures = m.get('deltaglider_auth_failures_total')?.samples ?? [];
  const totalAuthFails = authFailures.reduce((a, s) => a + s.value, 0);

  // Status-code stacked bar data — single row, four segments.
  const statusStacked = [{ name: 'HTTP', ...httpByStatus }];

  return (
    <div className="animate-fade-in" style={{ width: '100%', padding: 'clamp(14px, 1.8vw, 24px) clamp(12px, 1.6vw, 20px)' }}>
      <DashboardToolbar
        title="Proxy Dashboard"
        meta={<>v{buildVersion} · {backendType} backend · up {uptimeStr}</>}
        view={activeView}
        onView={(v) => { setActiveView(v); localStorage.setItem('dg-metrics-view', v); }}
        range="5m"
        onRange={() => {}}
        cadence={cadence}
        onCadence={setCadence}
        onManualRefresh={fetchMetrics}
        loading={refreshing}
      />

      {error && (
        <div style={{ padding: '10px 14px', marginBottom: 12, background: colors.BG_CARD, border: `1px solid ${colors.ACCENT_RED}`, borderRadius: 8 }}>
          <Text style={{ color: colors.ACCENT_RED, fontSize: 13 }}>Failed to load metrics: {error}</Text>
        </div>
      )}

      {/* Back button on non-embedded mount lives at the top, minimal. */}
      {!embedded && (
        <div style={{ marginBottom: 8 }}>
          <button
            onClick={onBack}
            style={{
              background: 'transparent', border: 'none', cursor: 'pointer',
              color: colors.TEXT_SECONDARY, fontSize: 12, padding: '4px 0',
              fontFamily: 'var(--font-ui)',
            }}
          >
            ← Back
          </button>
        </div>
      )}

      {activeView === 'analytics' ? (
        <AnalyticsSection config={adminConfig} />
      ) : (
        <DashboardGrid>
          {/* ── Row 1: KPI strip (4× stat, 3 cols each) ───────────── */}
          <Panel
            title="Objects stored"
            subtitle={stats?.truncated ? 'Sampled first 1,000 objects' : undefined}
            colSpan={3}
          >
            <StatValue
              value={stats ? `${fmtNum(stats.total_objects)}${stats.truncated ? '+' : ''}` : '—'}
              hint={stats ? `${formatBytes(stats.total_original_size)}${stats.truncated ? '+' : ''} original data` : 'Loading…'}
            />
          </Panel>
          <Panel
            title="Storage savings"
            colSpan={3}
            accent={stats && stats.savings_percentage > 10 ? 'green' : undefined}
          >
            <StatValue
              value={stats && stats.total_original_size > 0 ? stats.savings_percentage.toFixed(1) : '—'}
              unit="%"
              tone={stats && stats.savings_percentage > 10 ? 'good' : 'neutral'}
              hint={stats && stats.total_original_size > 0
                ? `${formatBytes(stats.total_original_size - stats.total_stored_size)} saved · ${formatBytes(stats.total_stored_size)} on disk`
                : 'No data yet'}
            />
          </Panel>
          <Panel title="Total requests" colSpan={3}>
            <StatValue
              value={fmtNum(totalHttp)}
              hint={`Avg latency ${fmtDuration(latencyStats.avg)}${errorRate > 0.05 ? ` · ${fmtPct(errorRate)} errors` : ''}`}
              tone={errorRate > 0.05 ? 'bad' : 'neutral'}
            />
          </Panel>
          <Panel title="Peak memory" colSpan={3}>
            <StatValue
              value={formatBytes(peakRss)}
              hint="Process RSS high-water mark"
            />
          </Panel>

          {/* ── Row 2: HTTP time-series + error rate gauge ────────── */}
          <Panel
            title="Request rate + latency"
            subtitle="Per 5-second interval (in-memory window)"
            colSpan={8}
            rowSpan={2}
            empty={history.length < 2 ? { title: 'Waiting for data', hint: 'The first data point lands a few seconds after opening the dashboard.' } : undefined}
          >
            {history.length >= 2 && (
              <div style={{ flex: 1, minHeight: 0 }}>
                <ResponsiveContainer width="100%" height="100%">
                  <AreaChart data={history} margin={{ top: 8, right: 8, bottom: 0, left: -20 }}>
                    <XAxis dataKey="t" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} minTickGap={40} />
                    <YAxis yAxisId="left" tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={40} />
                    <YAxis yAxisId="right" orientation="right" tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={40} />
                    <RTooltip {...tt} />
                    <Area yAxisId="left" type="monotone" dataKey="httpTotal" stroke={CHART_PALETTE[1]} fill={`${CHART_PALETTE[1]}33`} strokeWidth={2} name="Requests" />
                    <Area yAxisId="right" type="monotone" dataKey="avgLatency" stroke={CHART_PALETTE[3]} fill={`${CHART_PALETTE[3]}22`} strokeWidth={2} name="Avg latency (ms)" />
                  </AreaChart>
                </ResponsiveContainer>
              </div>
            )}
          </Panel>
          <Panel
            title="Error rate"
            subtitle="Share of 4xx + 5xx responses"
            colSpan={4}
            rowSpan={2}
            accent={errorRate > 0.05 ? 'red' : errorRate > 0.01 ? 'amber' : 'green'}
          >
            <StatValue
              value={totalHttp > 0 ? fmtPct(errorRate) : '—'}
              tone={errorRate > 0.05 ? 'bad' : errorRate > 0.01 ? 'warn' : 'good'}
              hint={`${fmtNum((httpByStatus['4xx'] ?? 0) + (httpByStatus['5xx'] ?? 0))} errors of ${fmtNum(totalHttp)} requests`}
            />
            {totalHttp > 0 && Object.keys(httpByStatus).length > 0 && (
              <div style={{ marginTop: 'auto' }}>
                <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: '0.06em', textTransform: 'uppercase', color: colors.TEXT_MUTED, marginBottom: 6 }}>Status breakdown</div>
                <ResponsiveContainer width="100%" height={36}>
                  <BarChart data={statusStacked} layout="vertical" margin={{ top: 0, right: 0, bottom: 0, left: 0 }}>
                    <XAxis type="number" hide />
                    <YAxis type="category" dataKey="name" hide />
                    <RTooltip {...tt} />
                    {['2xx', '3xx', '4xx', '5xx'].map(status => (
                      <Bar key={status} dataKey={status} stackId="s" fill={STATUS_COLORS[status]} radius={status === '2xx' ? [4, 0, 0, 4] : status === '5xx' ? [0, 4, 4, 0] : 0} />
                    ))}
                  </BarChart>
                </ResponsiveContainer>
                <div style={{ display: 'flex', gap: 10, marginTop: 6, flexWrap: 'wrap' }}>
                  {['2xx', '3xx', '4xx', '5xx'].filter(s => httpByStatus[s] > 0).map(s => (
                    <span key={s} style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 11, fontFamily: 'var(--font-mono)', color: colors.TEXT_SECONDARY }}>
                      <span style={{ width: 8, height: 8, borderRadius: 2, background: STATUS_COLORS[s] }} />
                      {s}: {fmtNum(httpByStatus[s])}
                    </span>
                  ))}
                </div>
              </div>
            )}
          </Panel>

          {/* ── Row 3: Cache stats ────────────────────────────────── */}
          <Panel
            title="Cache utilization"
            subtitle={`${formatBytes(cacheUsed)} of ${formatBytes(cacheMax)}`}
            colSpan={4}
            accent={cacheUtil > 0.9 ? 'red' : cacheUtil > 0.7 ? 'amber' : undefined}
          >
            <StatValue
              value={fmtPct(cacheUtil)}
              tone={cacheUtil > 0.9 ? 'bad' : cacheUtil > 0.7 ? 'warn' : 'neutral'}
              hint={cacheUtil > 0.9 ? 'Nearly full — consider raising cache_size_mb' : 'Reference baselines held in memory'}
            >
              <Progress
                percent={Math.round(cacheUtil * 100)}
                size="small"
                strokeColor={cacheUtil > 0.9 ? colors.ACCENT_RED : cacheUtil > 0.7 ? colors.ACCENT_AMBER : colors.ACCENT_GREEN}
                showInfo={false}
                style={{ marginTop: 10 }}
              />
            </StatValue>
          </Panel>
          <Panel
            title="Cache hit rate"
            subtitle={`${fmtNum(cacheHits)} hits · ${fmtNum(cacheMisses)} misses`}
            colSpan={4}
            accent={cacheMissRate > 0.5 ? 'red' : cacheMissRate > 0.2 ? 'amber' : 'green'}
          >
            <StatValue
              value={cacheTotal > 0 ? fmtPct(1 - cacheMissRate) : '—'}
              tone={cacheMissRate > 0.5 ? 'bad' : cacheMissRate > 0.2 ? 'warn' : 'good'}
              hint={cacheMissRate > 0.5 ? 'More than half of lookups miss — cache undersized' : 'Each miss forces a full backend read'}
            />
          </Panel>
          <Panel
            title="Cache entries"
            subtitle={cacheMax > 0 ? `${formatBytes(cacheMax / Math.max(cacheEntries, 1))} avg per entry` : 'Cache disabled'}
            colSpan={4}
          >
            <StatValue value={fmtNum(cacheEntries)} hint="Active reference baselines" />
          </Panel>

          {/* Row 4: Cache hits vs misses time-series (full width) */}
          <Panel
            title="Cache hits vs misses"
            subtitle="Per 5-second interval, stacked"
            colSpan={12}
            rowSpan={2}
            empty={history.length < 2 ? { title: 'Warming up' } : undefined}
          >
            {history.length >= 2 && (
              <div style={{ flex: 1, minHeight: 0 }}>
                <ResponsiveContainer width="100%" height="100%">
                  <AreaChart data={history} margin={{ top: 8, right: 8, bottom: 0, left: -20 }}>
                    <XAxis dataKey="t" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} minTickGap={60} />
                    <YAxis tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={40} allowDecimals={false} />
                    <RTooltip {...tt} />
                    <Area type="monotone" dataKey="cacheHits" stackId="1" stroke={CHART_PALETTE[0]} fill={`${CHART_PALETTE[0]}55`} strokeWidth={2} name="Hits" />
                    <Area type="monotone" dataKey="cacheMisses" stackId="1" stroke={CHART_PALETTE[4]} fill={`${CHART_PALETTE[4]}55`} strokeWidth={2} name="Misses" />
                  </AreaChart>
                </ResponsiveContainer>
              </div>
            )}
          </Panel>

          {/* ── Row 5: Delta codec stats (4× 3-col) ───────────────── */}
          <Panel title="Avg encode" subtitle={`${fmtNum(encodeStats.count)} total encodes`} colSpan={3}>
            <StatValue value={encodeStats.count > 0 ? fmtDuration(encodeStats.avg) : '—'} hint="xdelta3 encode wall time" />
          </Panel>
          <Panel title="Avg decode" subtitle={`${fmtNum(decodeStats.count)} total decodes`} colSpan={3}>
            <StatValue value={decodeStats.count > 0 ? fmtDuration(decodeStats.avg) : '—'} hint="xdelta3 decode wall time" />
          </Panel>
          <Panel
            title="Avg compression"
            subtitle={compressionHist.count > 0 ? `Across ${fmtNum(compressionHist.count)} deltas` : 'No deltas yet'}
            colSpan={3}
            accent={compressionHist.avg > 0 && compressionHist.avg < 0.5 ? 'green' : undefined}
          >
            <StatValue
              value={compressionHist.count > 0 ? fmtPct(compressionHist.avg) : '—'}
              tone={compressionHist.avg > 0 && compressionHist.avg < 0.5 ? 'good' : 'neutral'}
              hint="Lower = better (more savings)"
            />
          </Panel>
          <Panel
            title="Codec slots"
            subtitle="xdelta3 concurrency permits"
            colSpan={3}
            accent={codecAvail === 0 ? 'red' : undefined}
          >
            <StatValue
              value={fmtNum(codecAvail)}
              unit="free"
              tone={codecAvail === 0 ? 'bad' : 'neutral'}
              hint={codecAvail === 0 ? 'Saturated — requests may 503' : 'Headroom for new encode/decode ops'}
            />
          </Panel>

          {/* ── Row 6: Compression histogram + storage decisions ──── */}
          <Panel
            title="Compression ratio distribution"
            subtitle="Lower bucket = better savings"
            colSpan={6}
            rowSpan={2}
            empty={compressionBuckets.length === 0 || compressionHist.count === 0 ? { title: 'No deltas yet', hint: 'Upload versioned archives into the same prefix to see compression ratios.' } : undefined}
          >
            {compressionBuckets.length > 0 && compressionHist.count > 0 && (
              <div style={{ flex: 1, minHeight: 0 }}>
                <ResponsiveContainer width="100%" height="100%">
                  <BarChart data={compressionBuckets} margin={{ top: 8, right: 8, bottom: 0, left: -24 }}>
                    <XAxis dataKey="range" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} />
                    <YAxis tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={40} allowDecimals={false} />
                    <RTooltip {...tt} />
                    <Bar dataKey="count" name="Deltas" radius={[4, 4, 0, 0]} fill={CHART_PALETTE[2]} />
                  </BarChart>
                </ResponsiveContainer>
              </div>
            )}
          </Panel>
          <Panel
            title="Storage decisions"
            subtitle="Delta vs passthrough vs reference"
            colSpan={6}
            rowSpan={2}
            empty={Object.keys(decisions).length === 0 ? { title: 'No decisions recorded' } : undefined}
          >
            {Object.keys(decisions).length > 0 && (
              <div style={{ flex: 1, minHeight: 0 }}>
                <ResponsiveContainer width="100%" height="100%">
                  <BarChart
                    data={Object.entries(decisions).sort(([, a], [, b]) => b - a).map(([name, value]) => ({ name, value }))}
                    layout="vertical"
                    margin={{ top: 8, right: 16, bottom: 0, left: 8 }}
                  >
                    <XAxis type="number" hide />
                    <YAxis type="category" dataKey="name" width={96} tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} />
                    <RTooltip {...tt} />
                    <Bar dataKey="value" name="Count" radius={[0, 4, 4, 0]}>
                      {Object.keys(decisions).sort((a, b) => decisions[b] - decisions[a]).map((_, i) => (
                        <Cell key={i} fill={CHART_PALETTE[i % CHART_PALETTE.length]} />
                      ))}
                    </Bar>
                  </BarChart>
                </ResponsiveContainer>
              </div>
            )}
          </Panel>

          {/* ── Row 7: HTTP detail ────────────────────────────────── */}
          <Panel
            title="Latency distribution"
            subtitle="Request duration buckets"
            colSpan={6}
            rowSpan={2}
            empty={latencyBuckets.length === 0 || latencyStats.count === 0 ? { title: 'No requests recorded' } : undefined}
          >
            {latencyBuckets.length > 0 && latencyStats.count > 0 && (
              <div style={{ flex: 1, minHeight: 0 }}>
                <ResponsiveContainer width="100%" height="100%">
                  <BarChart data={latencyBuckets} margin={{ top: 8, right: 8, bottom: 0, left: -24 }}>
                    <XAxis dataKey="range" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} />
                    <YAxis tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={40} allowDecimals={false} />
                    <RTooltip {...tt} />
                    <Bar dataKey="count" name="Requests" radius={[4, 4, 0, 0]} fill={CHART_PALETTE[1]} />
                  </BarChart>
                </ResponsiveContainer>
              </div>
            )}
          </Panel>
          <Panel
            title="Requests by operation"
            subtitle="Top S3 operations by count"
            colSpan={6}
            rowSpan={2}
            empty={httpChartData.length === 0 ? { title: 'No requests yet' } : undefined}
          >
            {httpChartData.length > 0 && (
              <div style={{ flex: 1, minHeight: 0 }}>
                <ResponsiveContainer width="100%" height="100%">
                  <BarChart data={httpChartData} layout="vertical" margin={{ top: 8, right: 16, bottom: 0, left: 8 }}>
                    <XAxis type="number" hide />
                    <YAxis type="category" dataKey="name" width={112} tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} />
                    <RTooltip {...tt} />
                    <Bar dataKey="value" name="Requests" radius={[0, 4, 4, 0]}>
                      {httpChartData.map((_, i) => <Cell key={i} fill={CHART_PALETTE[i % CHART_PALETTE.length]} />)}
                    </Bar>
                  </BarChart>
                </ResponsiveContainer>
              </div>
            )}
          </Panel>

          {/* ── Row 8: HTTP sizes (2× 6-col stat) ─────────────────── */}
          <Panel title="Avg upload size" subtitle={`${fmtNum(reqSizeStats.count)} uploads with Content-Length`} colSpan={6}>
            <StatValue value={reqSizeStats.count > 0 ? formatBytes(reqSizeStats.avg) : '—'} hint="Request body bytes" />
          </Panel>
          <Panel title="Avg download size" subtitle={`${fmtNum(resSizeStats.count)} responses with Content-Length`} colSpan={6}>
            <StatValue value={resSizeStats.count > 0 ? formatBytes(resSizeStats.avg) : '—'} hint="Response body bytes" />
          </Panel>

          {/* ── Row 9: Authentication (conditional) ───────────────── */}
          {authAttempts > 0 && (
            <Panel
              title="Authentication"
              subtitle="SigV4 verification outcomes since process start"
              colSpan={12}
              accent={totalAuthFails > 0 ? 'red' : 'green'}
            >
              <div style={{ display: 'flex', gap: 24, flexWrap: 'wrap', alignItems: 'center' }}>
                <div style={{ minWidth: 160 }}>
                  <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: '0.06em', textTransform: 'uppercase', color: colors.TEXT_MUTED, marginBottom: 4 }}>Authenticated</div>
                  <div style={{ fontSize: 24, fontWeight: 700, color: colors.ACCENT_GREEN, fontFamily: 'var(--font-ui)' }}>
                    {fmtNum(authAttempts - totalAuthFails)}
                  </div>
                </div>
                <div style={{ minWidth: 160 }}>
                  <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: '0.06em', textTransform: 'uppercase', color: colors.TEXT_MUTED, marginBottom: 4 }}>Rejected</div>
                  <div style={{ fontSize: 24, fontWeight: 700, color: totalAuthFails > 0 ? colors.ACCENT_RED : colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)' }}>
                    {fmtNum(totalAuthFails)}
                  </div>
                </div>
                {authFailures.length > 0 && totalAuthFails > 0 && (
                  <div style={{ flex: 1, minWidth: 200 }}>
                    <div style={{ fontSize: 10, fontWeight: 700, letterSpacing: '0.06em', textTransform: 'uppercase', color: colors.TEXT_MUTED, marginBottom: 6 }}>Failure reasons</div>
                    <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                      {authFailures.filter(s => s.value > 0).map(s => (
                        <span
                          key={s.labels.reason}
                          style={{
                            padding: '4px 10px', borderRadius: 8, fontSize: 12,
                            fontFamily: 'var(--font-mono)',
                            background: `${colors.ACCENT_RED}22`,
                            color: colors.ACCENT_RED,
                            border: `1px solid ${colors.ACCENT_RED}44`,
                          }}
                        >
                          {s.labels.reason}: {fmtNum(s.value)}
                        </span>
                      ))}
                    </div>
                  </div>
                )}
              </div>
            </Panel>
          )}
        </DashboardGrid>
      )}
    </div>
  );
}
