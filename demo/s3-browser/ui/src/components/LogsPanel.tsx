/**
 * LogsPanel — Diagnostics → Logs.
 *
 * A live, filterable view of the proxy's operational logs (the in-memory log
 * ring + SSE tail behind `src/logs.rs`). Closes the "SSH and grep stdout" loop
 * that made the CI-403 incident slow to diagnose:
 *
 *   * Backlog: the recent ring (newest first), server-side filtered by level /
 *     target / text.
 *   * Follow (live tail): an SSE stream (`/logs/stream`) prepends new entries as
 *     they happen, matching the same filters.
 *   * Virtualized list (@tanstack/react-virtual) so a 2000-row + live-appending
 *     view stays smooth. Click a row to expand its structured fields.
 *
 * Captured at INFO+ by default (DGP_LOG_RING_LEVEL). Read-only, per-instance,
 * bounded — a triage convenience, not a log store.
 */
import { useEffect, useMemo, useRef, useState, useCallback } from 'react';
import { Typography, Input, Button, Tag, Select, Space, Switch } from 'antd';
import { ReloadOutlined, SearchOutlined, FileTextOutlined } from '@ant-design/icons';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useColors } from '../ThemeContext';
import { fetchLogs, streamLogs, type LogEntry, type LogFilters } from '../adminApi';

const { Text } = Typography;
const RING_CAP = 2000; // client-side trim ceiling for the live tail

interface Props {
  onSessionExpired?: () => void;
}

function levelColour(level: string): string {
  switch (level.toUpperCase()) {
    case 'ERROR':
      return 'red';
    case 'WARN':
      return 'orange';
    case 'INFO':
      return 'blue';
    case 'DEBUG':
      return 'default';
    case 'TRACE':
      return 'purple';
    default:
      return 'default';
  }
}

function relativeTime(iso: string, now: Date): string {
  const t = new Date(iso).getTime();
  const d = Math.floor((now.getTime() - t) / 1000);
  if (d < 0) return 'just now';
  if (d < 60) return `${d}s`;
  if (d < 3600) return `${Math.floor(d / 60)}m`;
  if (d < 86400) return `${Math.floor(d / 3600)}h`;
  return `${Math.floor(d / 86400)}d`;
}

export default function LogsPanel({ onSessionExpired }: Props) {
  const c = useColors();
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const [level, setLevel] = useState<string | undefined>(undefined);
  const [target, setTarget] = useState('');
  const [q, setQ] = useState('');
  const [follow, setFollow] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(new Date());
  const unsubRef = useRef<(() => void) | null>(null);

  const filters: LogFilters = useMemo(
    () => ({ level, target: target || undefined, q: q || undefined }),
    [level, target, q],
  );

  const loadBacklog = useCallback(async () => {
    try {
      const resp = await fetchLogs(filters, RING_CAP);
      setEntries(resp.entries);
      setError(null);
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'fetch failed';
      if (/401|unauth/i.test(msg)) onSessionExpired?.();
      setError(msg);
    }
  }, [filters, onSessionExpired]);

  // Initial + filter-change backlog load (always; the tail layers on top).
  useEffect(() => {
    loadBacklog();
  }, [loadBacklog]);

  // Live tail: (re)subscribe whenever Follow or the filters change.
  useEffect(() => {
    unsubRef.current?.();
    unsubRef.current = null;
    if (!follow) return;
    unsubRef.current = streamLogs(
      filters,
      (entry) => {
        setEntries((prev) => {
          const next = [entry, ...prev];
          return next.length > RING_CAP ? next.slice(0, RING_CAP) : next;
        });
      },
      undefined,
      () => setError('log stream interrupted — retrying…'),
    );
    return () => {
      unsubRef.current?.();
      unsubRef.current = null;
    };
  }, [follow, filters]);

  // Tick "time ago" once a second.
  useEffect(() => {
    const id = window.setInterval(() => setNow(new Date()), 1000);
    return () => window.clearInterval(id);
  }, []);

  const parentRef = useRef<HTMLDivElement>(null);
  const rowVirtualizer = useVirtualizer({
    count: entries.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 30,
    overscan: 12,
  });

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', gap: 12 }}>
      <Space wrap style={{ flexShrink: 0 }}>
        <FileTextOutlined style={{ color: c.TEXT_MUTED }} />
        <Select
          value={level ?? 'all'}
          onChange={(v) => setLevel(v === 'all' ? undefined : v)}
          style={{ width: 110 }}
          options={[
            { value: 'all', label: 'All levels' },
            { value: 'error', label: 'Error' },
            { value: 'warn', label: 'Warn+' },
            { value: 'info', label: 'Info+' },
            { value: 'debug', label: 'Debug+' },
          ]}
        />
        <Input
          placeholder="target (module)…"
          value={target}
          onChange={(e) => setTarget(e.target.value)}
          style={{ width: 200 }}
          allowClear
        />
        <Input
          prefix={<SearchOutlined />}
          placeholder="search message + fields…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          style={{ width: 240 }}
          allowClear
        />
        <Button icon={<ReloadOutlined />} onClick={loadBacklog} disabled={follow}>
          Refresh
        </Button>
        <Space size={4}>
          <Switch checked={follow} onChange={setFollow} />
          <Text type="secondary">Follow</Text>
        </Space>
        <Text type="secondary">{entries.length} lines</Text>
      </Space>

      {error && <Text type="danger">{error}</Text>}

      <div
        ref={parentRef}
        style={{
          flex: 1,
          overflow: 'auto',
          border: `1px solid ${c.BORDER}`,
          borderRadius: 6,
          fontFamily: 'monospace',
          fontSize: 12,
        }}
      >
        <div style={{ height: rowVirtualizer.getTotalSize(), position: 'relative', width: '100%' }}>
          {rowVirtualizer.getVirtualItems().map((vi) => {
            const e = entries[vi.index];
            const hasFields = e.fields && Object.keys(e.fields).length > 0;
            return (
              <div
                key={vi.key}
                style={{
                  position: 'absolute',
                  top: 0,
                  left: 0,
                  width: '100%',
                  transform: `translateY(${vi.start}px)`,
                  padding: '4px 10px',
                  borderBottom: `1px solid ${c.BORDER}`,
                  display: 'flex',
                  gap: 8,
                  alignItems: 'baseline',
                  whiteSpace: 'nowrap',
                }}
              >
                <Tag color={levelColour(e.level)} style={{ margin: 0, minWidth: 48, textAlign: 'center' }}>
                  {e.level}
                </Tag>
                <span
                  title={new Date(e.ts).toLocaleString()}
                  style={{ color: c.TEXT_MUTED, width: 40, flexShrink: 0 }}
                >
                  {relativeTime(e.ts, now)}
                </span>
                <span style={{ color: c.TEXT_SECONDARY, flexShrink: 0 }}>
                  {e.target.replace(/^deltaglider_proxy::/, '')}
                </span>
                <span
                  style={{
                    color: c.TEXT_PRIMARY,
                    overflow: 'hidden',
                    textOverflow: 'ellipsis',
                  }}
                >
                  {e.message}
                  {hasFields && (
                    <span style={{ color: c.TEXT_MUTED }}>
                      {' '}
                      {Object.entries(e.fields)
                        .map(([k, v]) => `${k}=${String(v)}`)
                        .join(' ')}
                    </span>
                  )}
                </span>
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}
