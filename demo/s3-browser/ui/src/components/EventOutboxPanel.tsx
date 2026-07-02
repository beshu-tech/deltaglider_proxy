import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useVisiblePolling } from '../useVisiblePolling';
import { Alert, Button, Input, message, Select, Space, Switch, Table, Tag, Typography } from 'antd';
import {
  DatabaseOutlined,
  DeleteOutlined,
  ReloadOutlined,
  SearchOutlined,
  SyncOutlined,
} from '@ant-design/icons';
import type { ColumnsType, TablePaginationConfig } from 'antd/es/table';
import type { SorterResult } from 'antd/es/table/interface';
import { useColors } from '../ThemeContext';
import {
  fetchEventOutbox,
  requeueEventOutbox,
  requeueEventOutboxMany,
  purgeFailedEventOutbox,
  type EventOutboxRecord,
  type EventOutboxStatus,
} from '../adminApi';

const { Text } = Typography;
const DEFAULT_PAGE_SIZE = 50;
const PAGE_SIZE_OPTIONS = [25, 50, 100, 250];

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

function fmtRelative(ts: number | null | undefined): string {
  if (!ts) return '—';
  const seconds = Math.round((ts * 1000 - Date.now()) / 1000);
  const abs = Math.abs(seconds);
  const unit =
    abs < 60 ? ['s', abs] :
      abs < 3600 ? ['m', Math.round(abs / 60)] :
        abs < 86400 ? ['h', Math.round(abs / 3600)] :
          ['d', Math.round(abs / 86400)];
  return seconds >= 0 ? `in ${unit[1]}${unit[0]}` : `${unit[1]}${unit[0]} ago`;
}

function statusColour(status: string): string {
  if (status === 'delivered') return 'success';
  if (status === 'failed') return 'error';
  if (status === 'in_progress') return 'processing';
  return 'warning';
}

export default function EventOutboxPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const [rows, setRows] = useState<EventOutboxRecord[]>([]);
  const [counts, setCounts] = useState({ pending: 0, in_progress: 0, delivered: 0, failed: 0 });
  const [total, setTotal] = useState(0);
  const [deliveryEnabled, setDeliveryEnabled] = useState(false);
  const [deliveryActive, setDeliveryActive] = useState(false);
  const [filter, setFilter] = useState('');
  const [status, setStatus] = useState<StatusFilter>('all');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(DEFAULT_PAGE_SIZE);
  const [sort, setSort] = useState('occurred_at');
  const [order, setOrder] = useState<SortOrder>('desc');
  const [autoRefresh, setAutoRefresh] = useState(false);
  const [loading, setLoading] = useState(true);
  const [requeueing, setRequeueing] = useState<number | 'bulk' | 'purge' | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Generation guard: the interval tick and dep-change refetches can overlap,
  // so stamp each fetch and only publish the newest response (an older,
  // slower response must not clobber a newer one).
  const fetchGen = useRef(0);
  const refresh = useCallback(async () => {
    const gen = ++fetchGen.current;
    try {
      setLoading(true);
      const res = await fetchEventOutbox(pageSize, status, (page - 1) * pageSize, sort, order);
      if (gen !== fetchGen.current) return; // superseded by a newer refresh
      setRows(res.rows);
      setCounts(res.counts);
      setTotal(res.total);
      setDeliveryEnabled(res.delivery_enabled);
      setDeliveryActive(res.delivery_active);
      setError(null);
    } catch (e) {
      if (gen !== fetchGen.current) return;
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(e instanceof Error ? e.message : 'Failed to load event outbox');
    } finally {
      if (gen === fetchGen.current) setLoading(false);
    }
  }, [onSessionExpired, order, page, pageSize, sort, status]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Visibility-gated: a backgrounded tab stops polling and catches up on return.
  useVisiblePolling(() => void refresh(), 3000, autoRefresh);

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
      width: 86,
      sorter: true,
      sortOrder: sort === 'id' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (id: number) => <Text code>#{id}</Text>,
    },
    {
      title: 'Status',
      dataIndex: 'status',
      key: 'status',
      width: 138,
      sorter: true,
      sortOrder: sort === 'status' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (value: EventOutboxStatus, row) => (
        <div title={`Attempts: ${row.attempts}${row.claimed_by ? ` · claimed by ${row.claimed_by}` : ''}`}>
          <Tag color={statusColour(value)}>{value}</Tag>
          <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
            {row.attempts} attempt{row.attempts === 1 ? '' : 's'}
          </Text>
        </div>
      ),
    },
    {
      title: 'Kind',
      dataIndex: 'kind',
      key: 'kind',
      width: 168,
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
      width: 360,
      sorter: true,
      sortOrder: sort === 'key' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (_: string, row) => (
        <div title={`${row.bucket}/${row.key}`} style={{ minWidth: 0 }}>
          <Text code style={{ display: 'block', maxWidth: 330, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
            {row.bucket}
          </Text>
          <Text style={{ display: 'block', maxWidth: 330, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
            {row.key || '—'}
          </Text>
        </div>
      ),
    },
    {
      title: 'Timing',
      dataIndex: 'occurred_at',
      key: 'occurred_at',
      width: 210,
      sorter: true,
      defaultSortOrder: 'descend',
      sortOrder: sort === 'occurred_at' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (value: number, row) => (
        <div title={`Occurred ${fmtUnix(value)} · queued ${fmtUnix(row.created_at)}`}>
          <Text style={{ display: 'block', fontSize: 12 }}>{fmtRelative(value)}</Text>
          <Text type="secondary" style={{ display: 'block', fontSize: 11 }}>
            queued {fmtRelative(row.created_at)}
          </Text>
        </div>
      ),
    },
    {
      title: 'Next Try',
      dataIndex: 'next_attempt_at',
      key: 'next_attempt_at',
      width: 130,
      sorter: true,
      sortOrder: sort === 'next_attempt_at' ? (order === 'asc' ? 'ascend' : 'descend') : null,
      render: (value: number | null, row) => (
        <span title={row.delivered_at ? `Delivered ${fmtUnix(row.delivered_at)}` : fmtUnix(value)}>
          {row.delivered_at ? 'delivered' : fmtRelative(value)}
        </span>
      ),
    },
    {
      title: 'Error / Payload',
      key: 'last_error',
      width: 380,
      render: (_: unknown, row) => {
        const text = row.last_error || JSON.stringify(row.payload);
        return (
          <Text
            type={row.last_error ? 'danger' : 'secondary'}
            title={text}
            style={{ display: 'block', maxWidth: 350, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}
          >
            {text}
          </Text>
        );
      },
    },
    {
      title: 'Action',
      key: 'action',
      width: 122,
      fixed: 'right',
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
      const res = await requeueEventOutbox(id);
      message.success(`Requeued ${res.requeued} event`);
      await refresh();
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      message.error(e instanceof Error ? e.message : 'Failed to requeue event');
    } finally {
      setRequeueing(null);
    }
  };

  const requeueVisibleFailed = async () => {
    if (visibleFailedIds.length === 0) return;
    try {
      setRequeueing('bulk');
      const res = await requeueEventOutboxMany(visibleFailedIds);
      message.success(`Requeued ${res.requeued} failed event${res.requeued === 1 ? '' : 's'}`);
      await refresh();
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      message.error(e instanceof Error ? e.message : 'Failed to requeue failed events');
    } finally {
      setRequeueing(null);
    }
  };

  const purgeFailed = async () => {
    if (!window.confirm(`Permanently delete all ${counts.failed} failed event(s)? They have exhausted retries; requeue would only re-fail them against a still-broken target.`)) {
      return;
    }
    try {
      setRequeueing('purge');
      const res = await purgeFailedEventOutbox();
      message.success(`Purged ${res.purged} failed event${res.purged === 1 ? '' : 's'}`);
      await refresh();
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      message.error(e instanceof Error ? e.message : 'Failed to purge failed events');
    } finally {
      setRequeueing(null);
    }
  };

  return (
    <div style={{ maxWidth: 1180, margin: '0 auto', padding: 'clamp(12px, 2vw, 18px)', display: 'flex', flexDirection: 'column', gap: 10 }}>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          gap: 12,
          padding: '10px 12px',
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 10,
          background: colors.BG_CARD,
        }}
      >
        <Space size={10}>
          <DatabaseOutlined style={{ color: colors.ACCENT_BLUE }} />
          <Text strong>Object change events</Text>
          <Tag
            color={deliveryActive ? 'success' : deliveryEnabled ? 'warning' : 'default'}
            title="Delivery runs in the background. Pending rows stay queued; failed rows can be requeued."
          >
            delivery {deliveryActive ? 'active' : deliveryEnabled ? 'waiting' : 'off'}
          </Tag>
        </Space>
        <Text type="secondary" style={{ fontSize: 12 }} title="Events are stored safely until they're delivered. Delivered events are eventually cleaned up; pending and failed events are kept so they can still be retried.">
          Durable queue for S3 object changes
        </Text>
      </div>

      {error && <Alert type="error" showIcon message="Fetch failed" description={error} />}

      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <Input
          placeholder="Search loaded rows..."
          prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          style={{ width: 300 }}
          allowClear
        />
        <Select
          value={status}
          onChange={(value) => {
            setStatus(value);
            setPage(1);
          }}
          style={{ width: 170 }}
          options={[
            { value: 'all', label: 'All statuses' },
            { value: 'pending', label: `Pending (${counts.pending})` },
            { value: 'in_progress', label: `In progress (${counts.in_progress})` },
            { value: 'failed', label: `Failed (${counts.failed})` },
            { value: 'delivered', label: `Delivered (${counts.delivered})` },
          ]}
        />
        <Button icon={<ReloadOutlined />} onClick={() => void refresh()} loading={loading && !autoRefresh}>
          Refresh
        </Button>
        <Button
          icon={<SyncOutlined />}
          onClick={() => void requeueVisibleFailed()}
          disabled={visibleFailedIds.length === 0}
          loading={requeueing === 'bulk'}
        >
          Requeue failed shown ({visibleFailedIds.length})
        </Button>
        <Button
          danger
          icon={<DeleteOutlined />}
          onClick={() => void purgeFailed()}
          disabled={counts.failed === 0}
          loading={requeueing === 'purge'}
          title="Permanently delete all failed events (retries exhausted)"
        >
          Purge failed ({counts.failed})
        </Button>
        <Space size={6}>
          <Switch size="small" checked={autoRefresh} onChange={setAutoRefresh} />
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>Auto-refresh (3s)</Text>
        </Space>
        <Text type="secondary" style={{ fontSize: 12 }}>
          {filtered.length} shown on this page · {total} total
        </Text>
      </div>

      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <CountPill label="Pending" value={counts.pending} colour="warning" active={status === 'pending'} onClick={() => { setStatus('pending'); setPage(1); }} />
        <CountPill label="In progress" value={counts.in_progress} colour="processing" active={status === 'in_progress'} onClick={() => { setStatus('in_progress'); setPage(1); }} />
        <CountPill label="Failed" value={counts.failed} colour="error" active={status === 'failed'} onClick={() => { setStatus('failed'); setPage(1); }} />
        <CountPill label="Delivered" value={counts.delivered} colour="success" active={status === 'delivered'} onClick={() => { setStatus('delivered'); setPage(1); }} />
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
          columns={columns}
          dataSource={filtered}
          rowKey="id"
          loading={loading}
          size="small"
          tableLayout="fixed"
          showSorterTooltip={false}
          scroll={{ x: 'max-content' }}
          onChange={onTableChange}
          locale={{
            emptyText: loading
              ? 'Loading...'
              : status === 'all' && !filter
                ? 'No object events have been recorded yet.'
                : 'No outbox rows match this view.',
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
  return (
    <button
      type="button"
      onClick={onClick}
      style={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 6,
        border: `1px solid ${active ? 'var(--accent-blue)' : 'var(--border)'}`,
        borderRadius: 999,
        padding: '5px 9px',
        background: active ? 'rgba(72, 160, 255, 0.12)' : 'var(--card-bg)',
        cursor: 'pointer',
      }}
    >
      <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.4 }}>{label}</Text>
      <Tag color={colour} style={{ marginInlineEnd: 0 }}>{value}</Tag>
    </button>
  );
}
