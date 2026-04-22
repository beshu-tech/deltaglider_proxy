import { useState, useEffect, useRef, useCallback } from 'react';
import { Typography, Space, Button, Spin, Switch, Progress, Tag } from 'antd';
import { ArrowLeftOutlined, ReloadOutlined, InfoCircleOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { formatBytes } from '../utils';
import { useCardStyles } from './shared-styles';
import AnalyticsSection from './AnalyticsSection';
import { getAdminConfig } from '../adminApi';
import type { AdminConfig } from '../adminApi';
import {
  AreaChart, Area, BarChart, Bar, PieChart, Pie, Cell,
  XAxis, YAxis, Tooltip as RTooltip, ResponsiveContainer,
} from 'recharts';

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

/** Get histogram bucket boundaries and cumulative counts */
function histBuckets(m: Map<string, ParsedMetric>, name: string): { le: string; count: number }[] {
  const metric = m.get(name);
  if (!metric) return [];
  return metric.samples
    .filter(s => 'le' in s.labels && s.labels.le !== '+Inf')
    .map(s => ({ le: s.labels.le, count: s.value }));
}

/** Convert cumulative histogram buckets to differential (per-bucket) counts */
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
   Formatters
   ═══════════════════════════════════════════════════════════ */

function fmtDuration(s: number): string {
  if (s === 0) return '—';
  if (s < 0.001) return `${(s * 1e6).toFixed(0)}us`;
  if (s < 1) return `${(s * 1000).toFixed(1)}ms`;
  return `${s.toFixed(2)}s`;
}
function fmtPct(ratio: number): string { return `${(ratio * 100).toFixed(1)}%`; }
function fmtNum(n: number): string { return n.toLocaleString(undefined, { maximumFractionDigits: 0 }); }

/* ═══════════════════════════════════════════════════════════
   Reusable components
   ═══════════════════════════════════════════════════════════ */

function StatCard({ label, value, description, color, warn, children }: {
  label: string; value: string; description: string; color?: string; warn?: string;
  children?: React.ReactNode;
}) {
  const { BG_CARD, BORDER, TEXT_MUTED, TEXT_PRIMARY } = useColors();
  return (
    <div style={{ background: BG_CARD, border: `1px solid ${BORDER}`, borderRadius: 12, padding: 16, flex: '1 1 180px', minWidth: 160 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 4, marginBottom: 4 }}>
        <Text style={{ fontSize: 11, fontWeight: 600, letterSpacing: 0.5, textTransform: 'uppercase', color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>{label}</Text>
        {warn && <InfoCircleOutlined title={warn} style={{ fontSize: 11, color: '#fbbf24', cursor: 'help' }} />}
      </div>
      <div style={{ fontSize: 26, fontWeight: 700, fontFamily: "var(--font-mono)", color: color || TEXT_PRIMARY, lineHeight: 1.2 }}>{value}</div>
      <Text style={{ fontSize: 11, color: TEXT_MUTED, fontFamily: "var(--font-ui)", display: 'block', marginTop: 4 }}>{description}</Text>
      {children}
    </div>
  );
}

function Section({ title, description, children }: { title: string; description: string; children: React.ReactNode }) {
  const { BG_CARD, BORDER, TEXT_MUTED, TEXT_PRIMARY } = useColors();
  return (
    <div style={{ background: BG_CARD, border: `1px solid ${BORDER}`, borderRadius: 12, padding: 'clamp(16px, 3vw, 24px)', marginBottom: 16 }}>
      <div style={{ marginBottom: 16 }}>
        <Text strong style={{ fontSize: 16, fontFamily: "var(--font-ui)", color: TEXT_PRIMARY, display: 'block' }}>{title}</Text>
        <Text style={{ fontSize: 12, color: TEXT_MUTED, fontFamily: "var(--font-ui)" }}>{description}</Text>
      </div>
      {children}
    </div>
  );
}

function ChartLabel({ text }: { text: string }) {
  const { TEXT_MUTED } = useColors();
  return <Text style={{ fontSize: 11, fontWeight: 600, color: TEXT_MUTED, fontFamily: "var(--font-ui)", display: 'block', marginBottom: 8, marginTop: 16 }}>{text}</Text>;
}

function Legend({ items }: { items: { color: string; label: string }[] }) {
  const { TEXT_MUTED } = useColors();
  return (
    <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap', marginBottom: 6 }}>
      {items.map(i => (
        <span key={i.label} style={{ display: 'inline-flex', alignItems: 'center', gap: 4, fontSize: 11, fontFamily: "var(--font-ui)", color: TEXT_MUTED }}>
          <span style={{ width: 10, height: 10, borderRadius: 2, background: i.color, display: 'inline-block' }} /> {i.label}
        </span>
      ))}
    </div>
  );
}

/* ═══════════════════════════════════════════════════════════
   Time-series history
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
const CHART_COLORS = ['#2dd4bf', '#60a5fa', '#a78bfa', '#fbbf24', '#fb7185', '#34d399', '#f472b6', '#818cf8'];

interface StatsData {
  total_objects: number;
  total_original_size: number;
  total_stored_size: number;
  savings_percentage: number;
  truncated: boolean;
}

/* ═══════════════════════════════════════════════════════════
   Main component
   ═══════════════════════════════════════════════════════════ */

interface Props { onBack: () => void; embedded?: boolean; }

export default function MetricsPage({ onBack, embedded }: Props) {
  const colors = useColors();
  const { cardStyle } = useCardStyles();
  const [metricsMap, setMetricsMap] = useState<Map<string, ParsedMetric>>(new Map());
  const [stats, setStats] = useState<StatsData | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [history, setHistory] = useState<Snapshot[]>([]);
  const [activeView, setActiveView] = useState<'monitoring' | 'analytics'>(() => {
    const saved = localStorage.getItem('dg-metrics-view');
    return saved === 'analytics' ? 'analytics' : 'monitoring';
  });
  const [adminConfig, setAdminConfig] = useState<AdminConfig | null>(null);

  useEffect(() => {
    getAdminConfig().then(setAdminConfig).catch(() => {});
  }, []);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const prevRef = useRef<{ hits: number; misses: number; http: number; latencySum: number; latencyCount: number } | null>(null);

  const tooltipStyle = {
    contentStyle: { background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 8, fontSize: 12, color: colors.TEXT_PRIMARY },
    labelStyle: { color: colors.TEXT_PRIMARY },
    itemStyle: { color: colors.TEXT_SECONDARY },
  };

  // Stats are expensive (scans all objects) — fetch once then every 60s, not on every refresh
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
    try {
      const metricsRes = await fetch('/_/metrics', { credentials: 'include' });
      if (!metricsRes.ok) throw new Error(`HTTP ${metricsRes.status}`);
      const parsed = parsePrometheus(await metricsRes.text());
      setMetricsMap(parsed);

      // Build time-series snapshot
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
    } finally { setLoading(false); }
  }, []);

  useEffect(() => { fetchMetrics(); }, [fetchMetrics]);
  useEffect(() => {
    if (autoRefresh) intervalRef.current = setInterval(fetchMetrics, 5000);
    return () => { if (intervalRef.current) clearInterval(intervalRef.current); };
  }, [autoRefresh, fetchMetrics]);

  const m = metricsMap;

  if (loading) return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin tip="Loading metrics..." /></div>;

  // ── Derived values ──
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

  // Compression ratio distribution
  const compressionBuckets = histDifferential(histBuckets(m, 'deltaglider_delta_compression_ratio'))
    .map(b => ({ range: `${(parseFloat(b.range) * 100).toFixed(0)}%`, count: b.count }));

  // HTTP
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

  // Latency distribution
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

  // Build info
  const buildMetric = m.get('deltaglider_build_info');
  const buildVersion = buildMetric?.samples[0]?.labels.version || '?';
  const backendType = buildMetric?.samples[0]?.labels.backend_type || '?';

  // Auth
  const authAttempts = m.get('deltaglider_auth_attempts_total')?.samples.reduce((a, s) => a + s.value, 0) ?? 0;
  const authFailures = m.get('deltaglider_auth_failures_total')?.samples ?? [];
  const totalAuthFails = authFailures.reduce((a, s) => a + s.value, 0);

  const cacheHealthColor = cacheMissRate > 0.5 ? colors.ACCENT_RED : cacheMissRate > 0.2 ? '#fbbf24' : colors.ACCENT_GREEN;
  const STATUS_COLORS: Record<string, string> = { '2xx': '#2dd4bf', '3xx': '#60a5fa', '4xx': '#fbbf24', '5xx': '#fb7185' };

  return (
    <div className="animate-fade-in" style={{ maxWidth: 860, width: '100%', margin: '0 auto', padding: 'clamp(16px, 3vw, 24px) clamp(12px, 2vw, 16px)' }}>
      {/* Header */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 20, flexWrap: 'wrap', gap: 8 }}>
        <div>
          <Typography.Title level={4} style={{ margin: 0, fontFamily: "var(--font-ui)", fontWeight: 700 }}>Proxy Dashboard</Typography.Title>
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, fontFamily: "var(--font-mono)" }}>
            v{buildVersion} &middot; {backendType} backend &middot; up {uptimeStr}
          </Text>
        </div>
        <Space>
          <span title="Live refresh every 5s"><Switch size="small" checked={autoRefresh} onChange={setAutoRefresh} /></span>
          <Button size="small" icon={<ReloadOutlined />} onClick={fetchMetrics} style={{ borderRadius: 8 }}>Refresh</Button>
          {!embedded && <Button size="small" icon={<ArrowLeftOutlined />} onClick={onBack} style={{ borderRadius: 8 }}>Back</Button>}
        </Space>
      </div>

      {/* View toggle */}
      <div style={{ display: 'flex', gap: 0, marginBottom: 20, background: colors.BG_CARD, borderRadius: 8, border: `1px solid ${colors.BORDER}`, overflow: 'hidden' }}>
        {(['monitoring', 'analytics'] as const).map(v => (
          <button
            key={v}
            onClick={() => { setActiveView(v); localStorage.setItem('dg-metrics-view', v); }}
            style={{
              flex: 1, padding: '10px 16px', border: 'none', cursor: 'pointer',
              background: activeView === v ? `${colors.ACCENT_BLUE}18` : 'transparent',
              borderBottom: activeView === v ? `2px solid ${colors.ACCENT_BLUE}` : '2px solid transparent',
              color: activeView === v ? colors.ACCENT_BLUE : colors.TEXT_SECONDARY,
              fontSize: 13, fontWeight: 600, fontFamily: 'var(--font-ui)',
              transition: 'all 0.15s',
            }}
          >
            {v === 'monitoring' ? 'Monitoring' : 'Analytics'}
          </button>
        ))}
      </div>

      {activeView === 'analytics' ? (
        <AnalyticsSection config={adminConfig} />
      ) : (
      <>

      {error && <div style={{ ...cardStyle, borderColor: colors.ACCENT_RED, marginBottom: 16 }}><Text style={{ color: colors.ACCENT_RED }}>Failed to load metrics: {error}</Text></div>}

      {/* ════════════════ Top KPIs ════════════════ */}
      <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginBottom: 16 }}>
        <StatCard
          label="Objects Stored"
          value={stats ? `${fmtNum(stats.total_objects)}${stats.truncated ? '+' : ''}` : '...'}
          description={stats ? `${formatBytes(stats.total_original_size)}${stats.truncated ? '+ ' : ' '}original data${stats.truncated ? ' (sampled first 1,000)' : ''}` : 'Loading...'}
        />
        <StatCard
          label="Storage Savings"
          value={stats && stats.total_original_size > 0 ? `${stats.savings_percentage.toFixed(1)}%` : '—'}
          description={stats && stats.total_original_size > 0
            ? `${formatBytes(stats.total_original_size - stats.total_stored_size)} saved (${formatBytes(stats.total_stored_size)} on disk)${stats.truncated ? ' — sampled' : ''}`
            : 'No data yet'}
          color={stats && stats.savings_percentage > 10 ? colors.ACCENT_GREEN : undefined}
        />
        <StatCard label="Total Requests" value={fmtNum(totalHttp)} description={`Avg latency: ${fmtDuration(latencyStats.avg)}`}
          warn={errorRate > 0.05 ? `${fmtPct(errorRate)} error rate` : undefined} />
        <StatCard label="Peak Memory" value={formatBytes(peakRss)} description="Process RSS high-water mark" />
      </div>

      {/* ════════════════ Cache Health ════════════════ */}
      <Section title="Reference Cache" description="LRU cache for reference files used in delta reconstruction. A high miss rate means the cache is undersized for the number of active deltaspaces — each miss forces a full read from storage.">
        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginBottom: 12 }}>
          <StatCard label="Utilization" value={fmtPct(cacheUtil)} description={`${formatBytes(cacheUsed)} of ${formatBytes(cacheMax)}`}
            warn={cacheUtil > 0.9 ? 'Cache nearly full — consider increasing cache_size_mb' : undefined}>
            <Progress percent={Math.round(cacheUtil * 100)} size="small" strokeColor={cacheUtil > 0.9 ? colors.ACCENT_RED : colors.ACCENT_GREEN} showInfo={false} style={{ marginTop: 8 }} />
          </StatCard>
          <StatCard label="Hit Rate" value={cacheTotal > 0 ? fmtPct(1 - cacheMissRate) : '—'} description={`${fmtNum(cacheHits)} hits / ${fmtNum(cacheMisses)} misses`}
            color={cacheTotal > 0 ? cacheHealthColor : undefined}
            warn={cacheMissRate > 0.5 ? 'Over half of lookups miss cache — active deltaspaces exceed cache capacity' : undefined} />
          <StatCard label="Entries" value={fmtNum(cacheEntries)} description={cacheMax > 0 ? `${formatBytes(cacheMax / Math.max(cacheEntries, 1))} avg per entry` : 'Cache disabled'} />
        </div>

        {history.length > 1 && (<>
          <ChartLabel text="CACHE HITS VS MISSES (PER 5s INTERVAL)" />
          <Legend items={[{ color: '#2dd4bf', label: 'Hits' }, { color: '#fb7185', label: 'Misses' }]} />
          <ResponsiveContainer width="100%" height={120}>
            <AreaChart data={history} margin={{ top: 4, right: 0, bottom: 0, left: 0 }}>
              <XAxis dataKey="t" tick={false} axisLine={false} />
              <YAxis hide allowDecimals={false} />
              <RTooltip {...tooltipStyle} />
              <Area type="monotone" dataKey="cacheHits" stackId="1" stroke="#2dd4bf" fill="#2dd4bf66" strokeWidth={2} name="Hits" />
              <Area type="monotone" dataKey="cacheMisses" stackId="1" stroke="#fb7185" fill="#fb718566" strokeWidth={2} name="Misses" />
            </AreaChart>
          </ResponsiveContainer>
        </>)}
      </Section>

      {/* ════════════════ Delta Compression ════════════════ */}
      <Section title="Delta Compression" description="Files within a deltaspace are stored as binary diffs (xdelta3) against a reference baseline. Lower compression ratios = better space savings. The codec uses bounded concurrency to prevent CPU saturation.">
        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginBottom: 12 }}>
          <StatCard label="Avg Encode Time" value={encodeStats.count > 0 ? fmtDuration(encodeStats.avg) : '—'} description={`${fmtNum(encodeStats.count)} total encodes`} />
          <StatCard label="Avg Decode Time" value={decodeStats.count > 0 ? fmtDuration(decodeStats.avg) : '—'} description={`${fmtNum(decodeStats.count)} total decodes`} />
          <StatCard label="Avg Compression" value={compressionHist.count > 0 ? fmtPct(compressionHist.avg) : '—'}
            description={compressionHist.count > 0 ? `Across ${fmtNum(compressionHist.count)} delta decisions` : 'No deltas yet'}
            color={compressionHist.avg > 0 && compressionHist.avg < 0.5 ? colors.ACCENT_GREEN : undefined} />
          <StatCard label="Codec Slots" value={`${fmtNum(codecAvail)} free`} description="xdelta3 concurrency permits"
            warn={codecAvail === 0 ? 'All codec slots busy — requests may get 503' : undefined} />
        </div>

        {Object.keys(decisions).length > 0 && (<>
          <ChartLabel text="STORAGE DECISIONS" />
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', marginBottom: 12 }}>
            {Object.entries(decisions).sort(([,a],[,b]) => b - a).map(([decision, count]) => (
              <Tag key={decision} style={{ padding: '4px 12px', borderRadius: 8, fontSize: 13, fontFamily: "var(--font-mono)" }}>
                <span style={{ color: colors.TEXT_MUTED, marginRight: 8 }}>{decision}</span>
                <span style={{ fontWeight: 700 }}>{fmtNum(count)}</span>
              </Tag>
            ))}
          </div>
        </>)}

        {compressionBuckets.length > 0 && compressionHist.count > 0 && (<>
          <ChartLabel text="COMPRESSION RATIO DISTRIBUTION" />
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, display: 'block', marginBottom: 8 }}>
            How often deltas achieve each compression level. Lower is better — 20% means the delta is 80% smaller than the original.
          </Text>
          <ResponsiveContainer width="100%" height={100}>
            <BarChart data={compressionBuckets} margin={{ top: 0, right: 0, bottom: 0, left: 0 }}>
              <XAxis dataKey="range" tick={{ fontSize: 10, fill: colors.TEXT_MUTED }} axisLine={false} tickLine={false} />
              <YAxis hide />
              <RTooltip {...tooltipStyle} />
              <Bar dataKey="count" name="Deltas" radius={[4, 4, 0, 0]} fill="#a78bfa" />
            </BarChart>
          </ResponsiveContainer>
        </>)}
      </Section>

      {/* ════════════════ HTTP Traffic ════════════════ */}
      <Section title="HTTP Traffic" description="All S3-compatible API requests processed by the proxy. Includes GET (downloads), PUT (uploads), HEAD (metadata), LIST, and DELETE operations.">
        <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginBottom: 12 }}>
          <StatCard label="Avg Latency" value={latencyStats.count > 0 ? fmtDuration(latencyStats.avg) : '—'} description={`${fmtNum(latencyStats.count)} requests measured`} />
          <StatCard label="Avg Upload Size" value={reqSizeStats.count > 0 ? formatBytes(reqSizeStats.avg) : '—'} description={`${fmtNum(reqSizeStats.count)} uploads with Content-Length`} />
          <StatCard label="Avg Download Size" value={resSizeStats.count > 0 ? formatBytes(resSizeStats.avg) : '—'} description={`${fmtNum(resSizeStats.count)} responses with Content-Length`} />
          <StatCard label="Error Rate" value={totalHttp > 0 ? fmtPct(errorRate) : '—'}
            description={`${fmtNum((httpByStatus['4xx'] ?? 0) + (httpByStatus['5xx'] ?? 0))} errors of ${fmtNum(totalHttp)}`}
            color={errorRate > 0.05 ? colors.ACCENT_RED : errorRate > 0 ? '#fbbf24' : colors.ACCENT_GREEN}
            warn={errorRate > 0.1 ? 'High error rate — check client configuration and server logs' : undefined} />
        </div>

        {/* Status code breakdown */}
        {Object.keys(httpByStatus).length > 0 && (<>
          <ChartLabel text="BY STATUS CODE" />
          <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', marginBottom: 12 }}>
            {Object.entries(httpByStatus).sort(([a],[b]) => a.localeCompare(b)).map(([status, count]) => (
              <Tag key={status} color={STATUS_COLORS[status] || colors.TEXT_MUTED} style={{ padding: '4px 12px', borderRadius: 8, fontSize: 13, fontFamily: "var(--font-mono)" }}>
                {status}: {fmtNum(count)}
              </Tag>
            ))}
          </div>
        </>)}

        {/* Requests by operation */}
        {httpChartData.length > 0 && (
          <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap', alignItems: 'center' }}>
            <div style={{ flex: '1 1 300px', minWidth: 0 }}>
              <ChartLabel text="BY OPERATION" />
              <ResponsiveContainer width="100%" height={Math.max(140, httpChartData.length * 30)}>
                <BarChart data={httpChartData} layout="vertical" margin={{ top: 0, right: 12, bottom: 0, left: 0 }}>
                  <XAxis type="number" hide />
                  <YAxis type="category" dataKey="name" width={110} tick={{ fontSize: 11, fontFamily: "var(--font-mono)", fill: colors.TEXT_MUTED }} axisLine={false} tickLine={false} />
                  <RTooltip {...tooltipStyle} />
                  <Bar dataKey="value" name="Requests" radius={[0, 4, 4, 0]}>
                    {httpChartData.map((_, i) => <Cell key={i} fill={CHART_COLORS[i % CHART_COLORS.length]} />)}
                  </Bar>
                </BarChart>
              </ResponsiveContainer>
            </div>
            <div style={{ flex: '0 0 150px' }}>
              <ResponsiveContainer width={150} height={150}>
                <PieChart>
                  <Pie data={httpChartData} dataKey="value" nameKey="name" cx="50%" cy="50%" innerRadius={38} outerRadius={65} paddingAngle={2}>
                    {httpChartData.map((_, i) => <Cell key={i} fill={CHART_COLORS[i % CHART_COLORS.length]} />)}
                  </Pie>
                  <RTooltip {...tooltipStyle} />
                </PieChart>
              </ResponsiveContainer>
            </div>
          </div>
        )}

        {/* Latency distribution */}
        {latencyBuckets.length > 0 && latencyStats.count > 0 && (<>
          <ChartLabel text="LATENCY DISTRIBUTION" />
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, display: 'block', marginBottom: 8 }}>
            How long requests take to process. Spikes at higher buckets indicate slow operations (large delta reconstructions, storage latency).
          </Text>
          <ResponsiveContainer width="100%" height={100}>
            <BarChart data={latencyBuckets} margin={{ top: 0, right: 0, bottom: 0, left: 0 }}>
              <XAxis dataKey="range" tick={{ fontSize: 10, fill: colors.TEXT_MUTED }} axisLine={false} tickLine={false} />
              <YAxis hide />
              <RTooltip {...tooltipStyle} />
              <Bar dataKey="count" name="Requests" radius={[4, 4, 0, 0]} fill="#60a5fa" />
            </BarChart>
          </ResponsiveContainer>
        </>)}

        {/* Live request rate */}
        {history.length > 1 && (<>
          <ChartLabel text="REQUEST RATE & LATENCY (PER 5s INTERVAL)" />
          <Legend items={[{ color: '#60a5fa', label: 'Requests' }, { color: '#fbbf24', label: 'Avg latency (ms)' }]} />
          <ResponsiveContainer width="100%" height={100}>
            <AreaChart data={history} margin={{ top: 4, right: 0, bottom: 0, left: 0 }}>
              <XAxis dataKey="t" tick={false} axisLine={false} />
              <YAxis hide allowDecimals={false} />
              <RTooltip {...tooltipStyle} />
              <Area type="monotone" dataKey="httpTotal" stroke="#60a5fa" fill="#60a5fa55" strokeWidth={2} name="Requests" />
              <Area type="monotone" dataKey="avgLatency" stroke="#fbbf24" fill="#fbbf2433" strokeWidth={2} name="Avg latency (ms)" />
            </AreaChart>
          </ResponsiveContainer>
        </>)}
      </Section>

      {/* ════════════════ Authentication ════════════════ */}
      {authAttempts > 0 && (
        <Section title="Authentication" description="SigV4 request authentication. Each S3 request is verified against configured credentials. Failures may indicate misconfigured clients, expired signatures, or unauthorized access attempts.">
          <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap' }}>
            <StatCard label="Authenticated" value={fmtNum(authAttempts - totalAuthFails)} description="Successfully verified requests" color={colors.ACCENT_GREEN} />
            <StatCard label="Rejected" value={fmtNum(totalAuthFails)} description="Failed authentication attempts"
              color={totalAuthFails > 0 ? colors.ACCENT_RED : undefined}
              warn={totalAuthFails > 0 ? 'Review failure reasons below for potential security issues' : undefined} />
          </div>
          {authFailures.length > 0 && totalAuthFails > 0 && (<>
            <ChartLabel text="FAILURE REASONS" />
            <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
              {authFailures.filter(s => s.value > 0).map(s => (
                <Tag key={s.labels.reason} color="red" style={{ padding: '4px 12px', borderRadius: 8, fontSize: 13, fontFamily: "var(--font-mono)" }}>
                  {s.labels.reason}: {fmtNum(s.value)}
                </Tag>
              ))}
            </div>
          </>)}
        </Section>
      )}
      </>
      )}
    </div>
  );
}
