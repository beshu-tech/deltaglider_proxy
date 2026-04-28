/**
 * AnalyticsSection — cost + storage savings view.
 *
 * Shares the DashboardGrid / Panel primitives with MetricsPage so
 * both tabs read as one tool. Four rows:
 *   - KPI strip: Total Storage · Space Saved · Savings % · Est. Monthly Savings.
 *   - Storage by bucket horizontal bar + Top-5 table.
 *   - Session savings time-series + Compression opportunities.
 *   - (Optional future: per-bucket savings-% distribution histogram.)
 *
 * Data source: listBuckets() + `GET /stats?bucket=<name>` fetched per
 * bucket in chunks of 5. Savings history is an in-memory ring, same
 * as before.
 */
import { useState, useEffect, useCallback } from 'react';
import { Spin } from 'antd';
import { BarChart, Bar, XAxis, YAxis, ResponsiveContainer, Tooltip as RechartsTooltip, AreaChart, Area, Cell } from 'recharts';
import { SettingOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { listBuckets } from '../s3client';
import { adminFetch } from '../adminApi';
import type { AdminConfig } from '../adminApi';
import { formatBytes } from '../utils';
import DashboardGrid from './dashboard/DashboardGrid';
import Panel from './dashboard/Panel';
import StatValue from './dashboard/StatValue';
import { CHART_PALETTE, chartTooltipStyle, axisTickStyle, fmtNum } from './dashboard/chartDefaults';

interface BucketStats {
  bucket: string;
  totalOriginal: number;
  totalStored: number;
  savings: number;
  savingsPercent: number;
  objectCount: number;
}

interface Props {
  config: AdminConfig | null;
}

const COST_PRESETS = [
  { label: 'AWS S3', rate: 0.023 },
  { label: 'AWS S3 IA', rate: 0.0125 },
  { label: 'Hetzner', rate: 0.00524 },
  { label: 'Backblaze', rate: 0.006 },
  { label: 'Cloudflare R2', rate: 0 },
];

export default function AnalyticsSection({ config }: Props) {
  const colors = useColors();
  const tt = chartTooltipStyle(colors);
  const [loading, setLoading] = useState(true);
  const [bucketStats, setBucketStats] = useState<BucketStats[]>([]);
  const [totalOriginal, setTotalOriginal] = useState(0);
  const [totalStored, setTotalStored] = useState(0);
  const [costRate, setCostRate] = useState(() => {
    const saved = localStorage.getItem('dg-cost-per-gb');
    return saved ? parseFloat(saved) : 0.00524;
  });
  const [showCostConfig, setShowCostConfig] = useState(false);
  const [savingsHistory, setSavingsHistory] = useState<Array<{ time: string; saved: number }>>([]);

  const saveCostRate = (rate: number) => {
    setCostRate(rate);
    localStorage.setItem('dg-cost-per-gb', String(rate));
  };

  const fetchStats = useCallback(async () => {
    setLoading(true);
    try {
      const buckets = await listBuckets();
      const stats: BucketStats[] = [];
      const chunks: string[][] = [];
      for (let i = 0; i < buckets.length; i += 5) {
        chunks.push(buckets.slice(i, i + 5).map(b => b.name));
      }
      for (const chunk of chunks) {
        const results = await Promise.all(
          chunk.map(async (name) => {
            try {
              const res = await adminFetch(`/stats?bucket=${encodeURIComponent(name)}`);
              if (!res.ok) return null;
              const data = await res.json();
              return {
                bucket: name,
                totalOriginal: data.total_original_size || 0,
                totalStored: data.total_stored_size || 0,
                savings: (data.total_original_size || 0) - (data.total_stored_size || 0),
                savingsPercent: data.savings_percentage || 0,
                objectCount: data.total_objects || 0,
              } as BucketStats;
            } catch { return null; }
          })
        );
        stats.push(...results.filter((r): r is BucketStats => r !== null));
      }
      stats.sort((a, b) => b.totalOriginal - a.totalOriginal);
      setBucketStats(stats);
      const origTotal = stats.reduce((s, b) => s + b.totalOriginal, 0);
      const storedTotal = stats.reduce((s, b) => s + b.totalStored, 0);
      setTotalOriginal(origTotal);
      setTotalStored(storedTotal);

      const saved = origTotal - storedTotal;
      setSavingsHistory(prev => {
        const now = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
        return [...prev, { time: now, saved }].slice(-20);
      });
    } catch (e) {
      console.error('Analytics fetch failed:', e);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchStats(); }, [fetchStats]);
  useEffect(() => {
    const id = setInterval(fetchStats, 60000);
    return () => clearInterval(id);
  }, [fetchStats]);

  const totalSavings = totalOriginal - totalStored;
  const savingsPercent = totalOriginal > 0 ? (totalSavings / totalOriginal * 100) : 0;
  const monthlySavings = (totalSavings / (1024 * 1024 * 1024)) * costRate;
  const totalObjects = bucketStats.reduce((s, b) => s + b.objectCount, 0);

  const opportunities = bucketStats.filter(b => {
    const policy =
      config?.bucket_policies?.[b.bucket] ?? config?.bucket_policies?.[b.bucket.toLowerCase()];
    const bucketCompressionOn = policy?.compression ?? true;
    return !bucketCompressionOn && b.totalOriginal > 1024 * 1024;
  });

  if (loading && bucketStats.length === 0) {
    return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin /></div>;
  }

  // Bar-chart shape. Truncate long bucket names in the axis label,
  // preserve the real name for the tooltip.
  const chartData = bucketStats.map(b => ({
    name: b.bucket.length > 18 ? b.bucket.slice(0, 15) + '…' : b.bucket,
    stored: b.totalStored,
    saved: Math.max(0, b.totalOriginal - b.totalStored),
    fullName: b.bucket,
  }));

  // Top-5 table (or fewer if <5 buckets). Renders next to the stacked bar.
  const topBuckets = bucketStats.slice(0, 5);

  return (
    <DashboardGrid>
      {/* ── Row 1: KPI strip ──────────────────────────────────── */}
      <Panel title="Total storage" subtitle={`${fmtNum(totalObjects)} objects`} colSpan={3}>
        <StatValue value={formatBytes(totalOriginal)} hint="Original size across buckets" />
      </Panel>
      <Panel title="Space saved" subtitle="Via delta compression" colSpan={3} accent="green">
        <StatValue value={formatBytes(totalSavings)} tone="good" hint={`${formatBytes(totalStored)} actually on disk`} />
      </Panel>
      <Panel title="Savings" subtitle="Compression ratio" colSpan={3} accent="blue">
        <StatValue value={savingsPercent.toFixed(1)} unit="%" tone="neutral" hint="Lower = saved more" />
      </Panel>
      <Panel
        title="Est. monthly savings"
        subtitle={`at $${costRate}/GB/mo`}
        colSpan={3}
        accent="purple"
        actions={
          <button
            onClick={() => setShowCostConfig(!showCostConfig)}
            aria-label="Pick cost rate"
            title="Pick cost rate"
            style={{
              background: 'transparent', border: 'none', cursor: 'pointer',
              color: colors.TEXT_MUTED, padding: 2, display: 'flex', alignItems: 'center',
            }}
          >
            <SettingOutlined />
          </button>
        }
      >
        <div style={{ position: 'relative', flex: 1, display: 'flex', flexDirection: 'column' }}>
          <StatValue value={`$${monthlySavings.toFixed(2)}`} unit="/mo" tone="neutral" hint="At current cost rate" />
          {showCostConfig && (
            <div
              role="listbox"
              aria-label="Cost per GB/month"
              style={{
                position: 'absolute', top: '100%', right: 0, zIndex: 10, marginTop: 4,
                padding: 10, background: colors.BG_ELEVATED, border: `1px solid ${colors.BORDER}`,
                borderRadius: 8, boxShadow: '0 8px 24px rgba(0,0,0,0.25)', minWidth: 200,
              }}
              onKeyDown={e => { if (e.key === 'Escape') setShowCostConfig(false); }}
            >
              <div style={{ fontSize: 10, fontWeight: 700, color: colors.TEXT_MUTED, textTransform: 'uppercase', letterSpacing: '0.06em', marginBottom: 6 }}>
                Cost per GB/month
              </div>
              {COST_PRESETS.map(p => (
                <div
                  key={p.label}
                  role="option"
                  tabIndex={0}
                  aria-selected={costRate === p.rate}
                  onClick={() => { saveCostRate(p.rate); setShowCostConfig(false); }}
                  onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); saveCostRate(p.rate); setShowCostConfig(false); } }}
                  style={{
                    padding: '6px 8px', cursor: 'pointer', borderRadius: 4, fontSize: 12,
                    color: costRate === p.rate ? colors.ACCENT_BLUE : colors.TEXT_PRIMARY,
                    background: costRate === p.rate ? `${colors.ACCENT_BLUE}18` : 'transparent',
                  }}
                >
                  {p.label} — ${p.rate}/GB
                </div>
              ))}
            </div>
          )}
        </div>
      </Panel>

      {/* ── Row 2: Storage by bucket + Top-5 table ───────────── */}
      <Panel
        title="Storage by bucket"
        subtitle="Stored on disk vs bytes saved"
        colSpan={8}
        rowSpan={3}
        empty={chartData.length === 0 ? { title: 'No bucket data yet', hint: 'Create a bucket and upload a few objects to populate analytics.' } : undefined}
      >
        {chartData.length > 0 && (
          <div style={{ flex: 1, minHeight: 0 }}>
            <ResponsiveContainer width="100%" height="100%">
              <BarChart data={chartData} layout="vertical" margin={{ top: 8, right: 20, bottom: 0, left: 8 }}>
                <XAxis type="number" tickFormatter={v => formatBytes(v)} tick={axisTickStyle(colors)} axisLine={false} tickLine={false} />
                <YAxis type="category" dataKey="name" width={128} tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} />
                <RechartsTooltip
                  {...tt}
                  formatter={(value, name) => [formatBytes(Number(value)), name === 'stored' ? 'Stored' : 'Saved']}
                  labelFormatter={(_, payload) => {
                    const p = payload?.[0]?.payload as { fullName?: string } | undefined;
                    return p?.fullName ?? '';
                  }}
                />
                <Bar dataKey="stored" stackId="a" fill={CHART_PALETTE[1]} />
                <Bar dataKey="saved" stackId="a" fill={`${CHART_PALETTE[5]}aa`} radius={[0, 4, 4, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        )}
      </Panel>
      <Panel
        title="Top buckets"
        subtitle="By original size"
        colSpan={4}
        rowSpan={3}
        empty={topBuckets.length === 0 ? { title: 'No buckets' } : undefined}
      >
        {topBuckets.length > 0 && (
          <div style={{ flex: 1, overflow: 'auto', fontSize: 12 }}>
            {topBuckets.map((b, i) => (
              <div
                key={b.bucket}
                style={{
                  display: 'grid',
                  gridTemplateColumns: '1fr auto',
                  gap: 4,
                  padding: '8px 0',
                  borderTop: i === 0 ? 'none' : `1px solid ${colors.BORDER}`,
                }}
              >
                <div style={{ minWidth: 0 }}>
                  <div style={{
                    fontFamily: 'var(--font-mono)',
                    fontSize: 12,
                    color: colors.TEXT_PRIMARY,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                    whiteSpace: 'nowrap',
                  }}>
                    {b.bucket}
                  </div>
                  <div style={{ fontSize: 10.5, color: colors.TEXT_MUTED, marginTop: 2 }}>
                    {fmtNum(b.objectCount)} objects · {formatBytes(b.totalOriginal)} original
                  </div>
                </div>
                <div style={{ textAlign: 'right' }}>
                  <div style={{
                    fontSize: 13,
                    fontWeight: 700,
                    color: b.savingsPercent > 10 ? colors.ACCENT_GREEN : colors.TEXT_PRIMARY,
                    fontFamily: 'var(--font-ui)',
                    fontVariantNumeric: 'tabular-nums',
                  }}>
                    {b.savingsPercent.toFixed(1)}%
                  </div>
                  <div style={{ fontSize: 10.5, color: colors.TEXT_MUTED }}>
                    {formatBytes(b.savings)} saved
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </Panel>

      {/* ── Row 3: Session time-series + Compression opportunities ── */}
      <Panel
        title="Session savings"
        subtitle="Cumulative bytes saved while this dashboard is open"
        colSpan={8}
        rowSpan={2}
        empty={savingsHistory.length < 2 ? { title: 'Warming up', hint: 'The first point lands on the next 60-second refresh.' } : undefined}
      >
        {savingsHistory.length >= 2 && (
          <div style={{ flex: 1, minHeight: 0 }}>
            <ResponsiveContainer width="100%" height="100%">
              <AreaChart data={savingsHistory} margin={{ top: 8, right: 8, bottom: 0, left: -20 }}>
                <XAxis dataKey="time" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} minTickGap={40} />
                <YAxis tickFormatter={v => formatBytes(v)} tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={56} />
                <RechartsTooltip {...tt} formatter={(v) => [formatBytes(Number(v)), 'Saved']} />
                <Area type="monotone" dataKey="saved" stroke={CHART_PALETTE[5]} fill={`${CHART_PALETTE[5]}33`} strokeWidth={2} />
              </AreaChart>
            </ResponsiveContainer>
          </div>
        )}
      </Panel>
      <Panel
        title="Compression opportunities"
        subtitle="Buckets with compression off + meaningful data"
        colSpan={4}
        rowSpan={2}
        accent={opportunities.length > 0 ? 'amber' : undefined}
        empty={opportunities.length === 0 ? { title: 'Nothing to flag', hint: 'All buckets with data have compression enabled — good.' } : undefined}
      >
        {opportunities.length > 0 && (
          <div style={{ flex: 1, overflow: 'auto', fontSize: 12 }}>
            {opportunities.map((b, i) => (
              <div
                key={b.bucket}
                style={{
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'space-between',
                  gap: 8,
                  padding: '8px 0',
                  borderTop: i === 0 ? 'none' : `1px solid ${colors.BORDER}`,
                }}
              >
                <div style={{
                  fontFamily: 'var(--font-mono)',
                  fontSize: 12,
                  color: colors.TEXT_PRIMARY,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                  minWidth: 0,
                  flex: 1,
                }}>
                  {b.bucket}
                </div>
                <div style={{ fontSize: 11, color: colors.TEXT_MUTED, fontVariantNumeric: 'tabular-nums' }}>
                  {formatBytes(b.totalOriginal)}
                </div>
              </div>
            ))}
          </div>
        )}
      </Panel>

      {/* ── Row 4 (optional): per-bucket savings-% distribution ── */}
      {bucketStats.length >= 3 && (
        <Panel
          title="Per-bucket savings %"
          subtitle="How evenly compression is working across buckets"
          colSpan={12}
          rowSpan={2}
        >
          <div style={{ flex: 1, minHeight: 0 }}>
            <ResponsiveContainer width="100%" height="100%">
              <BarChart
                data={bucketStats.map(b => ({
                  name: b.bucket.length > 20 ? b.bucket.slice(0, 17) + '…' : b.bucket,
                  savings: b.savingsPercent,
                  fullName: b.bucket,
                }))}
                margin={{ top: 8, right: 8, bottom: 0, left: -16 }}
              >
                <XAxis dataKey="name" tick={axisTickStyle(colors, true)} axisLine={false} tickLine={false} interval={0} angle={-15} textAnchor="end" height={60} />
                <YAxis tickFormatter={v => `${v.toFixed(0)}%`} tick={axisTickStyle(colors)} axisLine={false} tickLine={false} width={48} domain={[0, 100]} />
                <RechartsTooltip
                  {...tt}
                  formatter={(v) => [`${Number(v).toFixed(1)}%`, 'Savings']}
                  labelFormatter={(_, payload) => {
                    const p = payload?.[0]?.payload as { fullName?: string } | undefined;
                    return p?.fullName ?? '';
                  }}
                />
                <Bar dataKey="savings" radius={[4, 4, 0, 0]}>
                  {bucketStats.map((b, i) => (
                    <Cell
                      key={i}
                      fill={b.savingsPercent >= 50 ? CHART_PALETTE[5] : b.savingsPercent >= 20 ? CHART_PALETTE[0] : CHART_PALETTE[3]}
                    />
                  ))}
                </Bar>
              </BarChart>
            </ResponsiveContainer>
          </div>
        </Panel>
      )}
    </DashboardGrid>
  );
}
