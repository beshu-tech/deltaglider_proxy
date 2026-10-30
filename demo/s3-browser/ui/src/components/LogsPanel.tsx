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
import { useEffect, useMemo, useRef, useState } from 'react';
import { useDebounced } from '../hooks/useDebounced';
import { Typography, Input, Button, Tag, Select, Space, Switch } from 'antd';
import { ReloadOutlined, SearchOutlined, FileTextOutlined } from '@ant-design/icons';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useColors } from '../ThemeContext';
import { relativeTime } from '../utils';
import { streamLogs, type LogEntry, type LogFilters } from '../adminApi';
import { useLogBacklog } from '../queries/logs';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { normalizeUiError } from '../errorHandling';
import { useDocumentVisible } from '../useVisiblePolling';

const { Text } = Typography;
const RING_CAP = 2000; // client-side trim ceiling for the live tail
const FILTER_DEBOUNCE_MS = 300;

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

export default function LogsPanel({ onSessionExpired }: Props) {
  const c = useColors();
  const [level, setLevel] = useState<string | undefined>(undefined);
  const [target, setTarget] = useState('');
  const [q, setQ] = useState('');
  const [follow, setFollow] = useState(false);
  const [streamError, setStreamError] = useState<string | null>(null);
  const [now, setNow] = useState(new Date());
  const unsubRef = useRef<(() => void) | null>(null);

  // Typing in target/q must not fire a backlog fetch + SSE reconnect per
  // keystroke: the text filters feed `filters` only after a pause.
  const debouncedTarget = useDebounced(target, FILTER_DEBOUNCE_MS);
  const debouncedQ = useDebounced(q, FILTER_DEBOUNCE_MS);
  const filters: LogFilters = useMemo(
    () => ({ level, target: debouncedTarget || undefined, q: debouncedQ || undefined }),
    [level, debouncedTarget, debouncedQ],
  );

  // Backlog: refetched on a filter change; the previous filter's lines stay
  // until the new ones land.
  const backlog = useLogBacklog(filters, RING_CAP);
  const { refetch: refetchBacklog } = backlog;
  const loadBacklog = () => void refetchBacklog();
  useSessionExpiredOn(backlog.error, onSessionExpired);
  const error = backlog.error ? normalizeUiError(backlog.error, 'fetch failed') : null;

  // Live-tail lines, prepended to the backlog they arrived on top of. A new
  // backlog already holds them, so a tail stamped with an older backlog
  // counts as empty.
  const backlogAt = backlog.dataUpdatedAt;
  const backlogAtRef = useRef(backlogAt);
  backlogAtRef.current = backlogAt;
  const [tail, setTail] = useState<{ base: number; lines: LogEntry[] }>({ base: 0, lines: [] });
  const entries = useMemo(() => {
    const lines = backlog.data?.entries ?? [];
    if (tail.base !== backlogAt || tail.lines.length === 0) return lines;
    const next = [...tail.lines, ...lines];
    return next.length > RING_CAP ? next.slice(0, RING_CAP) : next;
  }, [backlog.data, backlogAt, tail]);

  // A hidden tab holds no live stream (it reconnects and delivered every
  // line for nobody). On return, the backlog fills the gap, then the tail
  // resumes.
  const visible = useDocumentVisible();
  const wasHidden = useRef(false);
  useEffect(() => {
    if (!visible) {
      wasHidden.current = true;
    } else if (wasHidden.current) {
      wasHidden.current = false;
      if (follow) void refetchBacklog();
    }
  }, [visible, follow, refetchBacklog]);

  // Live tail: (re)subscribe whenever Follow, the filters or the tab's
  // visibility change.
  useEffect(() => {
    unsubRef.current?.();
    unsubRef.current = null;
    setStreamError(null);
    if (!follow || !visible) return;
    unsubRef.current = streamLogs(
      filters,
      (entry) => {
        // An entry arrived: the stream is healthy again.
        setStreamError(null);
        setTail((prev) => {
          const base = backlogAtRef.current;
          const lines = prev.base === base ? [entry, ...prev.lines] : [entry];
          return { base, lines: lines.length > RING_CAP ? lines.slice(0, RING_CAP) : lines };
        });
      },
      undefined,
      () => setStreamError('log stream interrupted — retrying…'),
    );
    return () => {
      unsubRef.current?.();
      unsubRef.current = null;
    };
  }, [follow, filters, visible]);

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
          aria-label="Minimum level"
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
          aria-label="Target (module)"
          placeholder="target (module)…"
          value={target}
          onChange={(e) => setTarget(e.target.value)}
          style={{ width: 200 }}
          allowClear
        />
        <Input
          aria-label="Search message and fields"
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
          <Switch id="logs-follow" checked={follow} onChange={setFollow} />
          <label htmlFor="logs-follow"><Text type="secondary">Follow</Text></label>
        </Space>
        <Text type="secondary">{entries.length} lines</Text>
      </Space>

      {error && <Text type="danger">{error}</Text>}
      {streamError && <Text type="danger">{streamError}</Text>}

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
                  // Width in `ch` (monospace): the longest relativeTime output,
                  // "23h 59m ago", is 11 characters.
                  style={{ color: c.TEXT_MUTED, width: '11ch', flexShrink: 0 }}
                >
                  {relativeTime(e.ts, { now })}
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
