import { useMemo, useState } from 'react';
import { withKeyboardSort } from '../tableSort';
import { confirmDialog } from '../confirmDialog';
import { Alert, Button, Input, message, Select, Space, Switch, Table, Tag, Typography } from 'antd';
import {
  DeleteOutlined,
  InboxOutlined,
  ReloadOutlined,
  SearchOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import type { ColumnsType, TablePaginationConfig } from 'antd/es/table';
import type { SorterResult } from 'antd/es/table/interface';
import { useColors } from '../ThemeContext';
import {
  DELIVERY_STATE,
  deliveryStateOf,
  type EndpointDelivery,
  type EventOutboxRecord,
  type EventOutboxStatus,
} from '../adminApi';
import { contentColumn, CONTENT_WIDE } from './shared-styles';
import { EmptyState } from './StatePlaceholders';
import { useIsNarrow } from '../useIsNarrow';
import { normalizeUiError } from '../errorHandling';
import { isSessionExpired } from '../errorHandling';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import {
  useEventOutboxPage,
  usePurgeFailedEvents,
  useRequeueEvent,
  useRequeueEvents,
} from '../queries/eventOutbox';
import { relativeTime } from '../utils';
import { countTone, endpointDeliverySummary, eventStatusTone } from '../statusTone';

const { Text } = Typography;
const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [25, 50, 100, 250];
const EMPTY_COUNTS = { pending: 0, in_progress: 0, delivered: 0, failed: 0 };

interface Props {
  onSessionExpired?: () => void;
}

type StatusFilter = EventOutboxStatus | 'all';
type SortOrder = 'asc' | 'desc';

function fmtUnix(ts: number | null | undefined): string {
  if (!ts) return '—';
  return new Date(ts * 1000).toLocaleString(undefined, {
    year: 'numeric',
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}

/** Unix SECONDS → relative label. `future: true` because a row's
 *  `next_attempt_at` is scheduled ahead ("in 5m"). */
function fmtRelative(ts: number | null | undefined): string {
  return relativeTime(ts ? ts * 1000 : null, { future: true });
}

/** Second line of the Timing cell: what happens next to this event. */
function nextStep(row: EventOutboxRecord): string {
  if (row.delivered_at) return `delivered ${fmtRelative(row.delivered_at)}`;
  if (row.status === 'failed') return 'retries used up';
  if (row.next_attempt_at) {
    return row.next_attempt_at * 1000 <= Date.now()
      ? 'next try due now'
      : `next try ${fmtRelative(row.next_attempt_at)}`;
  }
  return `queued ${fmtRelative(row.created_at)}`;
}

/** A bulk-action label: the count only when there is something to act on
 *  (a disabled "Purge failed (0)" says the same thing twice). */
function bulkLabel(label: string, count: number): string {
  return count > 0 ? `${label} (${count})` : label;
}

export default function EventOutboxPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  // Phone: hide the secondary columns; the table keeps its own scroller.
  const narrow = useIsNarrow(600);
  const [filter, setFilter] = useState('');
  const [status, setStatus] = useState<StatusFilter>('all');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);
  const [sort, setSort] = useState('occurred_at');
  const [order, setOrder] = useState<SortOrder>('desc');
  const [autoRefresh, setAutoRefresh] = useState(false);
  const [requeueing, setRequeueing] = useState<number | 'bulk' | 'purge' | null>(null);

  // Polled every 3 s while auto-refresh is on; paused in a background tab.
  const outbox = useEventOutboxPage(
    { limit: pageSize, offset: (page - 1) * pageSize, status, sort, order },
    autoRefresh ? 3000 : false,
  );
  const requeueOneMutation = useRequeueEvent();
  const requeueManyMutation = useRequeueEvents();
  const purgeMutation = usePurgeFailedEvents();
  const rows = useMemo(() => outbox.data?.rows ?? [], [outbox.data]);
  const counts = outbox.data?.counts ?? EMPTY_COUNTS;
  const total = outbox.data?.total ?? 0;
  const deliveryState = outbox.data ? deliveryStateOf(outbox.data) : 'disabled';
  const loading = outbox.isFetching;
  const refresh = () => outbox.refetch();
  useSessionExpiredOn(outbox.error, onSessionExpired);
  const error =
    outbox.error && !isSessionExpired(outbox.error)
      ? normalizeUiError(outbox.error, 'Failed to load the event log')
      : null;

  const filtered = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return rows;
    return rows.filter((row) =>
      `${row.kind} ${row.bucket} ${row.key} ${row.source} ${row.status} ${row.last_error || ''}`
        .toLowerCase()
        .includes(q),
    );
  }, [filter, rows]);

  const visibleFailedIds = useMemo(
    () => filtered.filter((row) => row.status === 'failed').map((row) => row.id),
    [filtered],
  );

  const onTableChange = (
    pagination: TablePaginationConfig,
    _filters: Record<string, unknown>,
    sorter: SorterResult<EventOutboxRecord> | SorterResult<EventOutboxRecord>[],
  ) => {
    setPage(pagination.current || 1);
    setPageSize(pagination.pageSize || DEFAULT_PAGE_SIZE);

    const activeSorter = Array.isArray(sorter) ? sorter[0] : sorter;
    if (activeSorter?.field && activeSorter.order) {
      setSort(String(activeSorter.field));
      setOrder(activeSorter.order === 'ascend' ? 'asc' : 'desc');
    } else {
      setSort('occurred_at');
      setOrder('desc');
    }
  };

  const columns: ColumnsType<EventOutboxRecord> = [
    {
      title: 'ID',
      dataIndex: 'id',
      key: 'id',
      width: 72,
      sorter: true,
      sortOrder: sort === 'id' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (id: number) => <Text code>#{id}</Text>,
    },
    {
      title: 'Status',
      dataIndex: 'status',
      key: 'status',
      width: 120,
      sorter: true,
      sortOrder: sort === 'status' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (value: EventOutboxStatus, row) => {
        const summary = endpointDeliverySummary(row.deliveries ?? []);
        return (
          <div title={`Attempts: ${row.attempts}${row.claimed_by ? ` · claimed by ${row.claimed_by}` : ''}`}>
            <Tag color={eventStatusTone(value, row.attempts)}>{value}</Tag>
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
              {row.attempts} attempt{row.attempts === 1 ? '' : 's'}
              {summary && (
                <>
                  {' · '}
                  <Text type={summary.tone === 'success' ? 'secondary' : 'warning'} style={{ fontSize: 11 }}>
                    {summary.text}
                  </Text>
                </>
              )}
            </Text>
          </div>
        );
      },
    },
    {
      title: 'Kind',
      dataIndex: 'kind',
      key: 'kind',
      width: 170,
      responsive: ['md'],
      sorter: true,
      sortOrder: sort === 'kind' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (_: string, row) => (
        <div title={`${row.kind} · ${row.source}`} style={{ minWidth: 0 }}>
          <Text style={{ display: 'block', fontSize: 12, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{row.kind}</Text>
          <Text type="secondary" style={{ display: 'block', fontSize: 11, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{row.source}</Text>
        </div>
      ),
    },
    {
      title: 'Object',
      dataIndex: 'key',
      key: 'key',
      width: 250,
      sorter: true,
      sortOrder: sort === 'key' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (_: string, row) => (
        <div title={`${row.bucket}/${row.key}`} style={{ minWidth: 0 }}>
          <Text code style={{ display: 'block', maxWidth: 226, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
            {row.bucket}
          </Text>
          <Text style={{ display: 'block', maxWidth: 226, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
            {row.key || '—'}
          </Text>
        </div>
      ),
    },
    {
      title: 'Timing',
      dataIndex: 'occurred_at',
      key: 'occurred_at',
      width: 160,
      responsive: ['sm'],
      sorter: true,
      defaultSortOrder: 'descend',
      sortOrder: sort === 'occurred_at' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (value: number, row) => (
        <div
          title={[
            `Occurred ${fmtUnix(value)}`,
            `queued ${fmtUnix(row.created_at)}`,
            row.delivered_at ? `delivered ${fmtUnix(row.delivered_at)}` : row.next_attempt_at ? `next try ${fmtUnix(row.next_attempt_at)}` : '',
          ].filter(Boolean).join(' · ')}
        >
          <Text style={{ display: 'block', fontSize: 12 }}>{fmtRelative(value)}</Text>
          <Text type="secondary" style={{ display: 'block', fontSize: 11 }}>
            {nextStep(row)}
          </Text>
        </div>
      ),
    },
    {
      title: 'Error / Payload',
      key: 'last_error',
      responsive: ['md'],
      // No fixed width: this column takes the space that is left, so the
      // table fits the page at desktop widths.
      render: (_: unknown, row) => {
        const text = row.last_error || JSON.stringify(row.payload);
        return (
          <Text
            type={row.last_error ? 'danger' : 'secondary'}
            title={text}
            style={{ display: 'block', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
          >
            {text}
          </Text>
        );
      },
    },
    {
      title: 'Action',
      key: 'action',
      // Not `fixed: 'right'`: a sticky column covers the columns behind it
      // while the table scrolls sideways, and their headers read as clipped
      // fragments ("NI" for "Next try").
      width: 116,
      render: (_: unknown, row) => (
        <Button
          size="small"
          icon={<SyncOutlined />}
          disabled={row.status !== 'failed'}
          loading={requeueing === row.id}
          onClick={() => void requeueOne(row.id)}
        >
          Requeue
        </Button>
      ),
    },
  ];

  const requeueOne = async (id: number) => {
    try {
      setRequeueing(id);
      const res = await requeueOneMutation.mutateAsync(id);
      message.success(`Requeued ${res.requeued} event`);
    } catch (e) {
      if (isSessionExpired(e)) {
        onSessionExpired?.();
        return;
      }
      message.error(normalizeUiError(e, "Failed to requeue event"));
    } finally {
      setRequeueing(null);
    }
  };

  const requeueVisibleFailed = async () => {
    if (visibleFailedIds.length === 0) return;
    try {
      setRequeueing('bulk');
      const res = await requeueManyMutation.mutateAsync(visibleFailedIds);
      message.success(`Requeued ${res.requeued} failed event${res.requeued === 1 ? '' : 's'}`);
    } catch (e) {
      if (isSessionExpired(e)) {
        onSessionExpired?.();
        return;
      }
      message.error(normalizeUiError(e, "Failed to requeue failed events"));
    } finally {
      setRequeueing(null);
    }
  };

  const purgeFailed = async () => {
    if (!(await confirmDialog({
      title: `Permanently delete all ${counts.failed} failed event(s)?`,
      content: 'They have exhausted their retries. A requeue would only fail again against a target that is still broken.',
      okText: 'Delete',
      danger: true,
    }))) {
      return;
    }
    try {
      setRequeueing('purge');
      const res = await purgeMutation.mutateAsync();
      message.success(`Purged ${res.purged} failed event${res.purged === 1 ? '' : 's'}`);
    } catch (e) {
      if (isSessionExpired(e)) {
        onSessionExpired?.();
        return;
      }
      message.error(normalizeUiError(e, "Failed to purge failed events"));
    } finally {
      setRequeueing(null);
    }
  };

  return (
    <div style={{ ...contentColumn(CONTENT_WIDE), padding: 'clamp(12px, 2vw, 18px)', display: 'flex', flexDirection: 'column', gap: 10 }}>
      {error && <Alert type="error" showIcon title="Fetch failed" description={error} />}

      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <Input
          aria-label="Search loaded rows"
          placeholder="Search loaded rows..."
          prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          style={{ flex: narrow ? '1 1 100%' : '0 1 300px', minWidth: 0 }}
          allowClear
        />
        <Select
          aria-label="Status filter"
          value={status}
          onChange={(value) => {
            setStatus(value);
            setPage(1);
          }}
          style={{ width: 170, flex: narrow ? '1 1 140px' : undefined }}
          options={[
            { value: 'all', label: 'All statuses' },
            { value: 'pending', label: `Pending (${counts.pending})` },
            { value: 'in_progress', label: `In progress (${counts.in_progress})` },
            { value: 'failed', label: `Failed (${counts.failed})` },
            { value: 'delivered', label: `Delivered (${counts.delivered})` },
          ]}
        />
        <Button icon={<ReloadOutlined />} onClick={() => void refresh()} loading={loading && !autoRefresh} style={{ flex: narrow ? '1 1 140px' : undefined }}>
          Refresh
        </Button>
        <Button
          icon={<SyncOutlined />}
          onClick={() => void requeueVisibleFailed()}
          disabled={visibleFailedIds.length === 0}
          loading={requeueing === 'bulk'}
          title="Requeue the failed events on this page"
          style={{ flex: narrow ? '1 1 140px' : undefined }}
        >
          {bulkLabel('Requeue failed', visibleFailedIds.length)}
        </Button>
        {/* eslint-disable-next-line no-restricted-syntax -- toolbar bulk action (confirmed), not a row action */}
        <Button
          danger
          icon={<DeleteOutlined />}
          onClick={() => void purgeFailed()}
          disabled={counts.failed === 0}
          loading={requeueing === 'purge'}
          title="Permanently delete all failed events (retries exhausted)"
          style={{ flex: narrow ? '1 1 140px' : undefined }}
        >
          {bulkLabel('Purge failed', counts.failed)}
        </Button>
        <Space size={6}>
          <Switch id="outbox-auto-refresh" size="small" checked={autoRefresh} onChange={setAutoRefresh} />
          <label htmlFor="outbox-auto-refresh" style={{ fontSize: 12, color: colors.TEXT_MUTED }}>Auto-refresh (3s)</label>
        </Space>
        <Tag
          color={DELIVERY_STATE[deliveryState].color}
          title="Delivery runs in the background. Pending events stay queued; failed events can be requeued."
          style={{ marginInlineEnd: 0 }}
        >
          delivery {DELIVERY_STATE[deliveryState].label.toLowerCase()}
        </Tag>
        <Text type="secondary" style={{ fontSize: 12 }}>
          {filtered.length} shown on this page · {total} total
        </Text>
      </div>

      {/* A grid, not a wrapping row: four counters on a phone wrapped 3 + 1. */}
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 8 }}>
        <CountPill label="Pending" value={counts.pending} colour="default" active={status === 'pending'} onClick={() => { setStatus('pending'); setPage(1); }} />
        <CountPill label="In progress" value={counts.in_progress} colour={countTone(counts.in_progress, 'processing')} active={status === 'in_progress'} onClick={() => { setStatus('in_progress'); setPage(1); }} />
        <CountPill label="Failed" value={counts.failed} colour={countTone(counts.failed, 'error')} active={status === 'failed'} onClick={() => { setStatus('failed'); setPage(1); }} />
        <CountPill label="Delivered" value={counts.delivered} colour={countTone(counts.delivered, 'success')} active={status === 'delivered'} onClick={() => { setStatus('delivered'); setPage(1); }} />
        {status !== 'all' && (
          <Button size="small" onClick={() => { setStatus('all'); setPage(1); }}>
            Clear status filter
          </Button>
        )}
      </div>

      {/* ponytail: dense server-paginated/sorted diagnostic table — scrolls
          horizontally on mobile (clipped wrapper); card-stack would drop sort+pagination. */}
      <div style={{ border: `1px solid ${colors.BORDER}`, borderRadius: 8, overflow: 'hidden', background: colors.BG_CARD }}>
        <Table<EventOutboxRecord>
          columns={withKeyboardSort(columns)}
          dataSource={filtered}
          rowKey="id"
          loading={loading && !autoRefresh}
          size="small"
          tableLayout="fixed"
          showSorterTooltip={false}
          scroll={{ x: narrow ? 'max-content' : 1040 }}
          onChange={onTableChange}
          expandable={{
            // Per-endpoint delivery state: which webhook URL or Slack channel
            // accepted the event, and why the others failed.
            // An accessible name for the expand column's header cell.
            columnTitle: <span className="sr-only">Details</span>,
            rowExpandable: (row) => (row.deliveries?.length ?? 0) > 0,
            expandedRowRender: (row) => <EndpointDeliveries deliveries={row.deliveries} />,
          }}
          locale={{
            // While loading the Table's own spinner overlay provides the motion;
            // a static "Loading..." underneath it just reads as a dead label.
            emptyText: loading ? (
              ' '
            ) : status === 'all' && !filter ? (
              <EmptyState
                compact
                icon={<InboxOutlined />}
                title="No object events have been recorded yet"
                hint="The proxy records events only while event delivery or replication is on."
              />
            ) : (
              <EmptyState compact title="No events match this view" hint="Change the search or the status filter." />
            ),
          }}
          pagination={{
            current: page,
            pageSize,
            total,
            showSizeChanger: true,
            pageSizeOptions: PAGE_SIZE_OPTIONS.map(String),
            size: 'small',
            showTotal: (value, range) => `${range[0]}-${range[1]} of ${value}`,
          }}
        />
      </div>
    </div>
  );
}

function CountPill({
  label,
  value,
  colour,
  active,
  onClick,
}: {
  label: string;
  value: number;
  colour: string;
  active?: boolean;
  onClick?: () => void;
}) {
  // Theme tokens: the CSS variables used before (--border, --card-bg,
  // --accent-blue) are not defined, so the pills had no border or fill.
  const { ACCENT_BLUE, BORDER, BG_CARD } = useColors();
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        gap: 6,
        border: `1px solid ${active ? ACCENT_BLUE : BORDER}`,
        borderRadius: 999,
        padding: '5px 9px',
        background: active ? `${ACCENT_BLUE}1f` : BG_CARD,
        cursor: 'pointer',
        fontFamily: 'var(--font-ui)',
      }}
    >
      <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.4 }}>{label}</Text>
      <Tag color={colour} style={{ marginInlineEnd: 0 }}>{value}</Tag>
    </button>
  );
}

/** Read-only list of one event's per-endpoint delivery state. */
function EndpointDeliveries({ deliveries }: { deliveries: EndpointDelivery[] }) {
  return (
    <div style={{ display: 'grid', gap: 4, padding: '4px 8px' }}>
      {deliveries.map((d) => (
        <div key={d.endpoint_id} style={{ display: 'flex', gap: 8, alignItems: 'baseline', minWidth: 0 }}>
          <Tag color={d.status === 'delivered' ? 'success' : 'error'}>{d.status}</Tag>
          <Text style={{ fontSize: 12, flexShrink: 0 }} title={`endpoint id ${d.endpoint_id}`}>
            {d.label ?? 'endpoint no longer configured'}
          </Text>
          <Text type="secondary" style={{ fontSize: 11, flexShrink: 0 }}>
            {d.attempts} attempt{d.attempts === 1 ? '' : 's'} · {fmtRelative(d.updated_at)}
          </Text>
          {d.last_error && (
            <Text
              type="danger"
              title={d.last_error}
              style={{ fontSize: 11, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
            >
              {d.last_error}
            </Text>
          )}
        </div>
      ))}
    </div>
  );
}
