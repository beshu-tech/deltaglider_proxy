import { useState, useEffect, useRef, useCallback } from 'react';
import { Typography, Space, Button, Spin, Tag, Tooltip, Switch } from 'antd';
import {
  ArrowLeftOutlined,
  ReloadOutlined,
  DashboardOutlined,
  ThunderboltOutlined,
  DatabaseOutlined,
  SafetyOutlined,
  ApiOutlined,
  CloudOutlined,
} from '@ant-design/icons';
import { getEndpoint } from '../s3client';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';

const { Text } = Typography;

/* ---------- Prometheus parser ---------- */

interface ParsedMetric {
  name: string;
  help: string;
  type: string;
  samples: { labels: Record<string, string>; value: number }[];
}

function parsePrometheus(text: string): ParsedMetric[] {
  const metrics: ParsedMetric[] = [];
  let current: ParsedMetric | null = null;

  for (const line of text.split('\n')) {
    if (line.startsWith('# HELP ')) {
      const rest = line.slice(7);
      const sp = rest.indexOf(' ');
      const name = rest.slice(0, sp);
      const help = rest.slice(sp + 1);
      current = { name, help, type: 'untyped', samples: [] };
    } else if (line.startsWith('# TYPE ')) {
      const rest = line.slice(7);
      const sp = rest.indexOf(' ');
      const type = rest.slice(sp + 1);
      if (current) current.type = type;
    } else if (line && !line.startsWith('#')) {
      // Sample line: metric_name{labels} value
      const braceIdx = line.indexOf('{');
      let name: string;
      let labels: Record<string, string> = {};
      let valueStr: string;
      if (braceIdx >= 0) {
        name = line.slice(0, braceIdx);
        const closeIdx = line.indexOf('}', braceIdx);
        const labelStr = line.slice(braceIdx + 1, closeIdx);
        // Parse labels: key="value",key="value"
        for (const m of labelStr.matchAll(/(\w+)="([^"]*)"/g)) {
          labels[m[1]] = m[2];
        }
        valueStr = line.slice(closeIdx + 2);
      } else {
        const sp = line.indexOf(' ');
        name = line.slice(0, sp);
        valueStr = line.slice(sp + 1);
      }
      const value = parseFloat(valueStr);
      if (!current || current.name !== name.replace(/_bucket$|_count$|_sum$|_total$/, '').replace(/_total$/, '')) {
        // Standalone sample or histogram sub-metric — find or create
        const baseName = name;
        let found = metrics.find((m) => m.name === baseName);
        if (!found) {
          found = { name: baseName, help: '', type: 'untyped', samples: [] };
          metrics.push(found);
        }
        found.samples.push({ labels, value });
      } else {
        current.samples.push({ labels, value });
      }
    } else if (line === '' && current) {
      // End of a metric block
      if (current.samples.length > 0) {
        metrics.push(current);
      }
      current = null;
    }
  }
  if (current && current.samples.length > 0) metrics.push(current);
  return metrics;
}

/* ---------- Grouping & formatting ---------- */

interface MetricGroup {
  title: string;
  icon: React.ReactNode;
  prefix: string;
  color: string;
}

const GROUPS: MetricGroup[] = [
  { title: 'Cache', icon: <DatabaseOutlined />, prefix: 'deltaglider_cache', color: '#2dd4bf' },
  { title: 'Delta Compression', icon: <ThunderboltOutlined />, prefix: 'deltaglider_delta', color: '#a78bfa' },
  { title: 'HTTP Requests', icon: <ApiOutlined />, prefix: 'deltaglider_http', color: '#60a5fa' },
  { title: 'Codec', icon: <DashboardOutlined />, prefix: 'deltaglider_codec', color: '#fbbf24' },
  { title: 'Auth', icon: <SafetyOutlined />, prefix: 'deltaglider_auth', color: '#fb7185' },
  { title: 'Process & Build', icon: <CloudOutlined />, prefix: 'process_|deltaglider_build', color: '#94a3b8' },
];

function matchGroup(name: string): MetricGroup | null {
  for (const g of GROUPS) {
    for (const p of g.prefix.split('|')) {
      if (name.startsWith(p)) return g;
    }
  }
  return null;
}

function fmtValue(value: number, name: string): string {
  if (name.includes('bytes') && !name.includes('ratio')) {
    if (value >= 1024 * 1024 * 1024) return `${(value / (1024 * 1024 * 1024)).toFixed(1)} GB`;
    if (value >= 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MB`;
    if (value >= 1024) return `${(value / 1024).toFixed(1)} KB`;
    return `${value.toFixed(0)} B`;
  }
  if (name.includes('ratio')) return `${(value * 100).toFixed(1)}%`;
  if (name.includes('seconds') && !name.includes('_total')) {
    if (value < 0.001) return `${(value * 1_000_000).toFixed(0)} us`;
    if (value < 1) return `${(value * 1000).toFixed(1)} ms`;
    return `${value.toFixed(2)} s`;
  }
  if (Number.isInteger(value) || value > 100) return value.toLocaleString();
  return value.toFixed(4);
}

function shortName(name: string): string {
  // Strip common prefixes for readability
  return name
    .replace(/^deltaglider_/, '')
    .replace(/^process_/, '')
    .replace(/_total$/, '');
}

function typeTag(type: string): React.ReactNode {
  const colorMap: Record<string, string> = {
    counter: 'blue',
    gauge: 'green',
    histogram: 'purple',
    summary: 'orange',
  };
  return <Tag color={colorMap[type] || 'default'} style={{ fontSize: 10, lineHeight: '16px', padding: '0 4px' }}>{type}</Tag>;
}

/* ---------- Component ---------- */

interface Props {
  onBack: () => void;
}

export default function MetricsPage({ onBack }: Props) {
  const colors = useColors();
  const { cardStyle, labelStyle } = useCardStyles();
  const [metrics, setMetrics] = useState<ParsedMetric[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [lastUpdate, setLastUpdate] = useState<Date | null>(null);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const fetchMetrics = useCallback(async () => {
    try {
      const endpoint = getEndpoint();
      const res = await fetch(`${endpoint}/metrics`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const text = await res.text();
      setMetrics(parsePrometheus(text));
      setLastUpdate(new Date());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to fetch');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchMetrics();
  }, [fetchMetrics]);

  useEffect(() => {
    if (autoRefresh) {
      intervalRef.current = setInterval(fetchMetrics, 5000);
    }
    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
    };
  }, [autoRefresh, fetchMetrics]);

  // Group metrics
  const grouped = new Map<string, ParsedMetric[]>();
  const ungrouped: ParsedMetric[] = [];
  for (const m of metrics) {
    const g = matchGroup(m.name);
    if (g) {
      const list = grouped.get(g.title) || [];
      list.push(m);
      grouped.set(g.title, list);
    } else {
      ungrouped.push(m);
    }
  }

  if (loading) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}>
        <Spin tip="Loading metrics..." />
      </div>
    );
  }

  return (
    <div className="animate-fade-in" style={{ maxWidth: 760, width: '100%', margin: '0 auto', padding: 'clamp(16px, 3vw, 24px) clamp(12px, 2vw, 16px)' }}>
      {/* Header */}
      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 16, flexWrap: 'wrap', gap: 8 }}>
        <Space>
          <Typography.Title level={4} style={{ margin: 0, fontFamily: "var(--font-ui)", fontWeight: 700 }}>Metrics</Typography.Title>
          {lastUpdate && (
            <Text style={{ color: colors.TEXT_MUTED, fontSize: 11, fontFamily: "var(--font-mono)" }}>
              {lastUpdate.toLocaleTimeString()}
            </Text>
          )}
        </Space>
        <Space>
          <Tooltip title="Auto-refresh every 5s">
            <Switch size="small" checked={autoRefresh} onChange={setAutoRefresh} />
          </Tooltip>
          <Button size="small" icon={<ReloadOutlined />} onClick={fetchMetrics} style={{ borderRadius: 8 }}>Refresh</Button>
          <Button size="small" icon={<ArrowLeftOutlined />} onClick={onBack} style={{ borderRadius: 8 }}>Back</Button>
        </Space>
      </div>

      {error && (
        <div style={{ ...cardStyle, borderColor: colors.ACCENT_RED, marginBottom: 16 }}>
          <Text style={{ color: colors.ACCENT_RED }}>Failed to load metrics: {error}</Text>
        </div>
      )}

      {/* Metric groups */}
      {GROUPS.map((group) => {
        const groupMetrics = grouped.get(group.title);
        if (!groupMetrics || groupMetrics.length === 0) return null;

        return (
          <div key={group.title} style={{ ...cardStyle, marginBottom: 16 }}>
            <SectionHeader icon={group.icon} title={group.title} />

            <div style={{ marginTop: 12 }}>
              <table style={{ width: '100%', borderCollapse: 'collapse' }}>
                <thead>
                  <tr>
                    <th style={{ ...labelStyle, textAlign: 'left', paddingBottom: 8, borderBottom: `1px solid ${colors.BORDER}` }}>Metric</th>
                    <th style={{ ...labelStyle, textAlign: 'left', paddingBottom: 8, borderBottom: `1px solid ${colors.BORDER}`, width: 60 }}>Type</th>
                    <th style={{ ...labelStyle, textAlign: 'right', paddingBottom: 8, borderBottom: `1px solid ${colors.BORDER}`, width: 120 }}>Value</th>
                  </tr>
                </thead>
                <tbody>
                  {groupMetrics.map((m) => {
                    // For simple metrics (1 sample, no labels), show inline
                    if (m.samples.length === 1 && Object.keys(m.samples[0].labels).length === 0) {
                      return (
                        <tr key={m.name}>
                          <td style={{ padding: '6px 0', borderBottom: `1px solid ${colors.BORDER}22` }}>
                            <Tooltip title={m.help}>
                              <Text style={{ fontFamily: "var(--font-mono)", fontSize: 12, cursor: 'help' }}>
                                {shortName(m.name)}
                              </Text>
                            </Tooltip>
                          </td>
                          <td style={{ padding: '6px 0', borderBottom: `1px solid ${colors.BORDER}22` }}>
                            {typeTag(m.type)}
                          </td>
                          <td style={{ padding: '6px 0', textAlign: 'right', fontFamily: "var(--font-mono)", fontSize: 13, fontWeight: 600, borderBottom: `1px solid ${colors.BORDER}22` }}>
                            {fmtValue(m.samples[0].value, m.name)}
                          </td>
                        </tr>
                      );
                    }
                    // Multi-sample: expand with labels
                    return m.samples.map((s, i) => {
                      const labelStr = Object.entries(s.labels).map(([k, v]) => `${k}="${v}"`).join(', ');
                      return (
                        <tr key={`${m.name}-${i}`}>
                          <td style={{ padding: '6px 0', paddingLeft: i === 0 ? 0 : 16, borderBottom: `1px solid ${colors.BORDER}22` }}>
                            <Tooltip title={m.help}>
                              <Text style={{ fontFamily: "var(--font-mono)", fontSize: 12, cursor: 'help' }}>
                                {i === 0 ? shortName(m.name) : ''}
                              </Text>
                            </Tooltip>
                            {labelStr && (
                              <Text style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: colors.TEXT_MUTED, marginLeft: i === 0 ? 6 : 0 }}>
                                {`{${labelStr}}`}
                              </Text>
                            )}
                          </td>
                          <td style={{ padding: '6px 0', borderBottom: `1px solid ${colors.BORDER}22` }}>
                            {i === 0 ? typeTag(m.type) : null}
                          </td>
                          <td style={{ padding: '6px 0', textAlign: 'right', fontFamily: "var(--font-mono)", fontSize: 13, fontWeight: 600, borderBottom: `1px solid ${colors.BORDER}22` }}>
                            {fmtValue(s.value, m.name)}
                          </td>
                        </tr>
                      );
                    });
                  })}
                </tbody>
              </table>
            </div>
          </div>
        );
      })}

      {/* Ungrouped metrics */}
      {ungrouped.length > 0 && (
        <div style={{ ...cardStyle, marginBottom: 16 }}>
          <SectionHeader icon={<DashboardOutlined />} title="Other" />
          <div style={{ marginTop: 12 }}>
            <table style={{ width: '100%', borderCollapse: 'collapse' }}>
              <thead>
                <tr>
                  <th style={{ ...labelStyle, textAlign: 'left', paddingBottom: 8, borderBottom: `1px solid ${colors.BORDER}` }}>Metric</th>
                  <th style={{ ...labelStyle, textAlign: 'left', paddingBottom: 8, borderBottom: `1px solid ${colors.BORDER}`, width: 60 }}>Type</th>
                  <th style={{ ...labelStyle, textAlign: 'right', paddingBottom: 8, borderBottom: `1px solid ${colors.BORDER}`, width: 120 }}>Value</th>
                </tr>
              </thead>
              <tbody>
                {ungrouped.map((m) =>
                  m.samples.map((s, i) => {
                    const labelStr = Object.entries(s.labels).map(([k, v]) => `${k}="${v}"`).join(', ');
                    return (
                      <tr key={`${m.name}-${i}`}>
                        <td style={{ padding: '6px 0', borderBottom: `1px solid ${colors.BORDER}22` }}>
                          <Tooltip title={m.help}>
                            <Text style={{ fontFamily: "var(--font-mono)", fontSize: 12, cursor: 'help' }}>
                              {i === 0 ? m.name : ''}
                            </Text>
                          </Tooltip>
                          {labelStr && (
                            <Text style={{ fontFamily: "var(--font-mono)", fontSize: 10, color: colors.TEXT_MUTED, marginLeft: 6 }}>
                              {`{${labelStr}}`}
                            </Text>
                          )}
                        </td>
                        <td style={{ padding: '6px 0', borderBottom: `1px solid ${colors.BORDER}22` }}>
                          {i === 0 ? typeTag(m.type) : null}
                        </td>
                        <td style={{ padding: '6px 0', textAlign: 'right', fontFamily: "var(--font-mono)", fontSize: 13, fontWeight: 600, borderBottom: `1px solid ${colors.BORDER}22` }}>
                          {fmtValue(s.value, m.name)}
                        </td>
                      </tr>
                    );
                  })
                )}
              </tbody>
            </table>
          </div>
        </div>
      )}
    </div>
  );
}
