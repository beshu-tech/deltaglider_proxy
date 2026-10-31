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
 *     view stays smooth. Click a row (or press Enter on it) to wrap the whole
 *     line (the caret on each row says so), or turn on "Wrap lines" for all;
 *     line and list its structured fields, one per line.
 *
 * Captured at INFO+ by default (DGP_LOG_RING_LEVEL). Read-only, per-instance,
 * bounded — a triage convenience, not a log store.
 */
import { useEffect, useMemo, useRef, useState } from 'react';
import { useDebounced } from '../hooks/useDebounced';
import { Typography, Input, Button, Tag, Select, Space, Switch } from 'antd';
import { DownOutlined, ReloadOutlined, RightOutlined, SearchOutlined } from '@ant-design/icons';
import { useVirtualizer } from '@tanstack/react-virtual';
import { useColors } from '../ThemeContext';
import { noun, relativeTime } from '../utils';
import { EmptyState, ErrorState } from './StatePlaceholders';
import { streamLogs, type LogEntry, type LogFilters } from '../adminApi';
import { useLogBacklog } from '../queries/logs';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { isSessionExpired } from '../errorHandling';
import { useDocumentVisible } from '../useVisiblePolling';
import { CARD_RADIUS, contentColumn, CONTENT_WIDE, SPACE } from './shared-styles';
import { activateOnKey } from '../keyboard';

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

/** Stable identity of a log line across prepends (an index moves when the
 *  live tail adds lines on top). */
function lineKey(e: LogEntry): string {
  return `${e.ts}\u0000${e.target}\u0000${e.message}`;
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
  // Lines the user opened: the whole message wraps and the fields show.
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  // "Wrap lines": every line shows in full (UX harvest D61: a cut line
  // had no visible way to read its end).
  const [wrapAll, setWrapAll] = useState(false);
  const toggleLine = (key: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });

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
  // No lines at all: ONE error state in the log box (UX harvest D17).
  const failed = Boolean(backlog.error) && !isSessionExpired(backlog.error) && backlog.data === undefined;

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
      () => setStreamError('The live log stream stopped. The proxy reconnects…'),
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
    <div
      style={{
        ...contentColumn(CONTENT_WIDE),
        boxSizing: 'border-box',
        display: 'flex',
        flexDirection: 'column',
        height: '100%',
        gap: SPACE.sm,
      }}
    >
      <Space wrap style={{ flexShrink: 0 }}>
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
        <Space size={4}>
          <Switch id="logs-wrap" checked={wrapAll} onChange={setWrapAll} />
          <label htmlFor="logs-wrap"><Text type="secondary">Wrap lines</Text></label>
        </Space>
        <Text type="secondary">{entries.length} {noun(entries.length, 'line')}</Text>
      </Space>

      {streamError && <Text type="warning">{streamError}</Text>}

      <div
        ref={parentRef}
        style={{
          flex: 1,
          overflow: 'auto',
          border: `1px solid ${c.BORDER}`,
          borderRadius: CARD_RADIUS,
          fontFamily: 'var(--font-mono)',
          fontSize: 12,
        }}
      >
        {/* A filter with no match left only an empty box (UX harvest C57). */}
        {failed && <ErrorState error={backlog.error} what="the logs" onRetry={loadBacklog} />}
        {entries.length === 0 && backlog.isSuccess && (
          <EmptyState
            icon={<SearchOutlined />}
            {...emptyLogCopy(debouncedQ, debouncedTarget, level)}
          />
        )}
        <div style={{ height: rowVirtualizer.getTotalSize(), position: 'relative', width: '100%' }}>
          {rowVirtualizer.getVirtualItems().map((vi) => {
            const e = entries[vi.index];
            const hasFields = e.fields && Object.keys(e.fields).length > 0;
            const key = lineKey(e);
            const open = wrapAll || expanded.has(key);
            const fieldText = hasFields
              ? Object.entries(e.fields).map(([k, v]) => `${k}=${String(v)}`)
              : [];
            return (
              <div
                key={vi.key}
                ref={rowVirtualizer.measureElement}
                data-index={vi.index}
                role="button"
                tabIndex={0}
                aria-expanded={open}
                title={wrapAll ? undefined : open ? 'Click to collapse' : 'Click to show the full line'}
                onClick={() => !wrapAll && toggleLine(key)}
                onKeyDown={activateOnKey(() => !wrapAll && toggleLine(key))}
                style={{
                  position: 'absolute',
                  top: 0,
                  left: 0,
                  width: '100%',
                  boxSizing: 'border-box',
                  transform: `translateY(${vi.start}px)`,
                  padding: `${SPACE.xxs}px ${SPACE.sm}px`,
                  borderBottom: `1px solid ${c.BORDER}`,
                  display: 'flex',
                  gap: SPACE.xs,
                  alignItems: 'baseline',
                  whiteSpace: open ? 'normal' : 'nowrap',
                  cursor: wrapAll ? 'default' : 'pointer',
                  background: open && !wrapAll ? c.BG_ELEVATED : undefined,
                }}
              >
                {/* The visible affordance: this row opens. */}
                {!wrapAll && (open
                  ? <DownOutlined aria-hidden style={{ fontSize: 10, color: c.TEXT_MUTED, flexShrink: 0 }} />
                  : <RightOutlined aria-hidden style={{ fontSize: 10, color: c.TEXT_MUTED, flexShrink: 0 }} />)}
                <Tag color={levelColour(e.level)} style={{ margin: 0, minWidth: 48, textAlign: 'center' }}>
                  {e.level}
                </Tag>
                <span
                  title={new Date(e.ts).toLocaleString()}
                  // Width in `ch` (monospace): the longest relativeTime output,
                  // "23h 59m ago", is 11 characters.
                  style={{ color: c.TEXT_MUTED, width: '11ch', flexShrink: 0, whiteSpace: 'nowrap' }}
                >
                  {relativeTime(e.ts, { now })}
                </span>
                <span style={{ color: c.TEXT_SECONDARY, flexShrink: 0, whiteSpace: 'nowrap' }}>
                  {e.target.replace(/^deltaglider_proxy::/, '')}
                </span>
                {open ? (
                  <span style={{ color: c.TEXT_PRIMARY, minWidth: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>
                    {e.message}
                    {fieldText.map((f) => (
                      <span key={f} style={{ display: 'block', color: c.TEXT_MUTED }}>
                        {f}
                      </span>
                    ))}
                  </span>
                ) : (
                  <span
                    style={{
                      color: c.TEXT_PRIMARY,
                      minWidth: 0,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                    }}
                  >
                    {e.message}
                    {hasFields && <span style={{ color: c.TEXT_MUTED }}> {fieldText.join(' ')}</span>}
                  </span>
                )}
              </div>
            );
          })}
        </div>
      </div>
    </div>
  );
}

/** The empty-list copy: what was asked for, and what to change. */
function emptyLogCopy(q: string, target: string, level: string | undefined): { title: string; hint: string } {
  if (!q && !target && !level) {
    return { title: 'No log lines yet', hint: 'Lines appear here as the proxy writes them.' };
  }
  const parts = ['No log line'];
  if (q) parts.push(`matches "${q}"`);
  if (target) parts.push(`from "${target}"`);
  if (level) parts.push('at this level');
  return {
    title: parts.join(' '),
    hint: 'Change the filters to see more lines. The proxy keeps only the most recent lines in memory.',
  };
}
