import { useState, useEffect, useCallback } from 'react';
import { Typography, Spin, Button } from 'antd';
import { BarChart, Bar, XAxis, YAxis, ResponsiveContainer, Tooltip as RechartsTooltip, AreaChart, Area } from 'recharts';
import { SettingOutlined } from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { listBuckets } from '../s3client';
import { adminFetch } from '../adminApi';
import type { AdminConfig } from '../adminApi';
import { formatBytes } from '../utils';

const { Text } = Typography;

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
  const [loading, setLoading] = useState(true);
  const [bucketStats, setBucketStats] = useState<BucketStats[]>([]);
  const [totalOriginal, setTotalOriginal] = useState(0);
  const [totalStored, setTotalStored] = useState(0);
  const [costRate, setCostRate] = useState(() => {
    const saved = localStorage.getItem('dg-cost-per-gb');
    return saved ? parseFloat(saved) : 0.00524; // Hetzner default
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
      // Fetch stats for each bucket (max 5 concurrent)
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

      // Add to savings history
      const saved = origTotal - storedTotal;
      setSavingsHistory(prev => {
        const now = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
        const next = [...prev, { time: now, saved }];
        return next.slice(-20); // keep last 20 data points
      });
    } catch (e) {
      console.error('Analytics fetch failed:', e);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { fetchStats(); }, [fetchStats]);

  // Auto-refresh every 60s
  useEffect(() => {
    const id = setInterval(fetchStats, 60000);
    return () => clearInterval(id);
  }, [fetchStats]);

  const totalSavings = totalOriginal - totalStored;
  const savingsPercent = totalOriginal > 0 ? (totalSavings / totalOriginal * 100) : 0;
  const monthlySavings = (totalSavings / (1024 * 1024 * 1024)) * costRate;

  const cardStyle: React.CSSProperties = {
    background: colors.BG_CARD,
    border: `1px solid ${colors.BORDER}`,
    borderRadius: 12,
    padding: 16,
    flex: '1 1 180px',
    minWidth: 160,
  };

  // Find compression opportunities — buckets where compression is effectively OFF
  const globalCompressionOn = (config?.max_delta_ratio ?? 0.75) > 0;
  const opportunities = bucketStats.filter(b => {
    const policy = config?.bucket_policies?.[b.bucket];
    const bucketCompressionOn = policy?.compression ?? globalCompressionOn;
    return !bucketCompressionOn && b.totalOriginal > 1024 * 1024; // >1MB with compression off
  });

  if (loading && bucketStats.length === 0) {
    return <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}><Spin /></div>;
  }

  const chartData = bucketStats.map(b => ({
    name: b.bucket.length > 15 ? b.bucket.slice(0, 12) + '...' : b.bucket,
    stored: b.totalStored,
    saved: Math.max(0, b.totalOriginal - b.totalStored),
    fullName: b.bucket,
  }));

  return (
    <div>
      {/* Summary Cards */}
      <div style={{ display: 'flex', gap: 12, marginBottom: 16, flexWrap: 'wrap' }}>
        <div style={cardStyle}>
          <Text style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0.5, color: colors.TEXT_MUTED, display: 'block', fontFamily: 'var(--font-ui)', marginBottom: 4 }}>Total Storage</Text>
          <Text style={{ fontSize: 26, fontWeight: 700, lineHeight: 1.2, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-mono)' }}>{formatBytes(totalOriginal)}</Text>
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, display: 'block', fontFamily: 'var(--font-ui)', marginTop: 4 }}>original size across {bucketStats.reduce((s, b) => s + b.objectCount, 0).toLocaleString()} objects</Text>
        </div>
        <div style={cardStyle}>
          <Text style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0.5, color: colors.ACCENT_GREEN, display: 'block', fontFamily: 'var(--font-ui)', marginBottom: 4 }}>Space Saved</Text>
          <Text style={{ fontSize: 26, fontWeight: 700, lineHeight: 1.2, color: colors.ACCENT_GREEN, fontFamily: 'var(--font-mono)' }}>{formatBytes(totalSavings)}</Text>
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, display: 'block', fontFamily: 'var(--font-ui)', marginTop: 4 }}>via delta compression</Text>
        </div>
        <div style={cardStyle}>
          <Text style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0.5, color: colors.ACCENT_BLUE, display: 'block', fontFamily: 'var(--font-ui)', marginBottom: 4 }}>Savings</Text>
          <Text style={{ fontSize: 26, fontWeight: 700, lineHeight: 1.2, color: colors.ACCENT_BLUE, fontFamily: 'var(--font-mono)' }}>{savingsPercent.toFixed(1)}%</Text>
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, display: 'block', fontFamily: 'var(--font-ui)', marginTop: 4 }}>compression ratio</Text>
        </div>
        <div style={{ ...cardStyle, position: 'relative' }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <Text style={{ fontSize: 11, fontWeight: 600, textTransform: 'uppercase', letterSpacing: 0.5, color: colors.ACCENT_PURPLE, display: 'block', fontFamily: 'var(--font-ui)', marginBottom: 4 }}>Est. Monthly Savings</Text>
            <Button type="text" size="small" icon={<SettingOutlined />} onClick={() => setShowCostConfig(!showCostConfig)} style={{ color: colors.TEXT_MUTED }} />
          </div>
          <Text style={{ fontSize: 26, fontWeight: 700, lineHeight: 1.2, color: colors.ACCENT_PURPLE, fontFamily: 'var(--font-mono)' }}>
            ${monthlySavings.toFixed(2)}/mo
          </Text>
          <Text style={{ fontSize: 11, color: colors.TEXT_MUTED, display: 'block', fontFamily: 'var(--font-ui)', marginTop: 4 }}>at ${costRate}/GB/mo</Text>
          {showCostConfig && (
            <div
              role="listbox"
              aria-label="Cost per GB/month"
              style={{ position: 'absolute', top: '100%', right: 0, zIndex: 10, marginTop: 4, padding: 12, background: colors.BG_ELEVATED, border: `1px solid ${colors.BORDER}`, borderRadius: 8, boxShadow: '0 8px 24px rgba(0,0,0,0.3)', minWidth: 200 }}
              onKeyDown={e => { if (e.key === 'Escape') setShowCostConfig(false); }}
            >
              <Text style={{ fontSize: 11, fontWeight: 600, color: colors.TEXT_MUTED, display: 'block', marginBottom: 8 }}>Cost per GB/month</Text>
              {COST_PRESETS.map(p => (
                <div
                  key={p.label}
                  role="option"
                  tabIndex={0}
                  aria-selected={costRate === p.rate}
                  onClick={() => { saveCostRate(p.rate); setShowCostConfig(false); }}
                  onKeyDown={e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); saveCostRate(p.rate); setShowCostConfig(false); } }}
                  style={{ padding: '6px 8px', cursor: 'pointer', borderRadius: 4, fontSize: 12, color: costRate === p.rate ? colors.ACCENT_BLUE : colors.TEXT_PRIMARY, background: costRate === p.rate ? `${colors.ACCENT_BLUE}18` : 'transparent' }}
                >
                  {p.label} — ${p.rate}/GB
                </div>
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Per-Bucket Breakdown */}
      {chartData.length > 0 && (
        <div style={{ background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 12, padding: 'clamp(16px, 3vw, 24px)', marginBottom: 16 }}>
          <Text style={{ fontSize: 16, fontWeight: 600, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)', display: 'block', marginBottom: 16 }}>Storage by Bucket</Text>
          <ResponsiveContainer width="100%" height={Math.max(200, chartData.length * 40)}>
            <BarChart data={chartData} layout="vertical" margin={{ left: 10, right: 20 }}>
              <XAxis type="number" tickFormatter={v => formatBytes(v)} style={{ fontSize: 10 }} />
              <YAxis type="category" dataKey="name" width={120} style={{ fontSize: 11, fontFamily: 'var(--font-mono)' }} />
              <RechartsTooltip
                formatter={(value, name) => [formatBytes(Number(value)), name === 'stored' ? 'Stored' : 'Saved']}
                contentStyle={{ background: colors.BG_ELEVATED, border: `1px solid ${colors.BORDER}`, borderRadius: 6, fontSize: 12 }}
              />
              <Bar dataKey="stored" stackId="a" fill={colors.ACCENT_BLUE} radius={[0, 0, 0, 0]} />
              <Bar dataKey="saved" stackId="a" fill={colors.ACCENT_GREEN + '60'} radius={[0, 4, 4, 0]} />
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Savings Over Time */}
      {savingsHistory.length > 1 && (
        <div style={{ background: colors.BG_CARD, border: `1px solid ${colors.BORDER}`, borderRadius: 12, padding: 'clamp(16px, 3vw, 24px)', marginBottom: 16 }}>
          <Text style={{ fontSize: 16, fontWeight: 600, color: colors.TEXT_PRIMARY, fontFamily: 'var(--font-ui)', display: 'block', marginBottom: 16 }}>Session Savings</Text>
          <ResponsiveContainer width="100%" height={180}>
            <AreaChart data={savingsHistory}>
              <XAxis dataKey="time" style={{ fontSize: 10 }} />
              <YAxis tickFormatter={v => formatBytes(v)} style={{ fontSize: 10 }} />
              <Area type="monotone" dataKey="saved" stroke={colors.ACCENT_GREEN} fill={colors.ACCENT_GREEN + '20'} />
            </AreaChart>
          </ResponsiveContainer>
        </div>
      )}

      {/* Compression Opportunities */}
      {opportunities.length > 0 && (
        <div style={{ background: colors.BG_CARD, border: `1px solid ${colors.ACCENT_AMBER}30`, borderRadius: 12, padding: 'clamp(16px, 3vw, 24px)' }}>
          <Text style={{ fontSize: 16, fontWeight: 600, color: colors.ACCENT_AMBER, fontFamily: 'var(--font-ui)', display: 'block', marginBottom: 8 }}>
            Compression Opportunities
          </Text>
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED, display: 'block', marginBottom: 12 }}>
            These buckets have compression disabled but contain significant data that could benefit from delta encoding.
          </Text>
          {opportunities.map(b => (
            <div key={b.bucket} style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '8px 0', borderTop: `1px solid ${colors.BORDER}` }}>
              <Text style={{ flex: 1, fontSize: 13, fontFamily: 'var(--font-mono)', color: colors.TEXT_PRIMARY }}>{b.bucket}</Text>
              <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>{formatBytes(b.totalOriginal)}</Text>
              <Text style={{ fontSize: 11, color: colors.ACCENT_AMBER }}>compression off</Text>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
