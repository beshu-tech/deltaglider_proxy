/**
 * AuditLogPanel — Wave 11 of the admin UI revamp.
 *
 * Surfaces the server's in-memory audit ring (see `src/audit.rs`)
 * as a sortable, filterable table under
 * `/_/admin/diagnostics/audit`.
 *
 * Why this exists: the backend has emitted structured `AUDIT |`
 * log lines for every mutation for ages, but the admin GUI had no
 * way to read them — operators debugging a production incident
 * had to shell in and `tail -f`. This panel closes that loop:
 *
 *   * Table of the last 500 entries (configurable via
 *     `DGP_AUDIT_RING_SIZE`), newest first.
 *   * Quick client-side filter on action / user / ip / bucket.
 *   * Auto-refresh toggle (3s interval). Off by default — the
 *     operator can flip it on while reproducing something.
 *   * Refresh button for manual one-shot refetch.
 *
 * This is a read-only surface. No mutations, no pagination (the
 * ring is bounded — fetching the whole thing is always cheap).
 * For long-term audit, stdout remains authoritative: this panel
 * is a triage convenience, not a compliance substitute.
 */
import { useMemo, useState } from 'react';
import { Typography, Input, Button, Tag, Space, Switch } from 'antd';
import {
  ReloadOutlined,
  SearchOutlined,
  FileTextOutlined,
  InfoCircleOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { useAuditLog } from '../queries/audit';
import { EmptyState, LoadingState, QueryState } from './StatePlaceholders';
import { relativeTime } from '../utils';
import { contentColumn, CONTENT_WIDE, SPACE } from './shared-styles';
import { isSessionExpired } from '../errorHandling';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { useIsNarrow } from '../useIsNarrow';
import TruncatedName from './TruncatedName';

const { Text } = Typography;

/** Single-line, ellipsis-on-overflow cell for the IP column (user, bucket
 *  and target are names: <TruncatedName> cuts them in the middle). */
const CELL_TRUNCATE_STYLE = {
  overflow: 'hidden',
  textOverflow: 'ellipsis',
  whiteSpace: 'nowrap',
} as const;

/** The column tracks of the audit table, defined once on the outer grid.
 *  Time, action, user, IP and bucket size to their content (up to a cap,
 *  then ellipsis), and the target column takes the rest. Short values do
 *  not spread over the full width. The action column fits its widest tag,
 *  so a long action such as `apply_config_section:admission` never runs
 *  into the user column. */
const AUDIT_COLUMNS =
  '76px max-content fit-content(200px) fit-content(150px) fit-content(160px) minmax(160px, 1fr)';

/** A header or data row: a subgrid that spans every column of the table. */
const AUDIT_ROW = {
  display: 'grid',
  gridTemplateColumns: 'subgrid',
  gridColumn: '1 / -1',
  columnGap: 16,
} as const;

/** The hint of the empty state: what gets recorded. */
const AUDIT_EMPTY_HINT =
  'The audit log records sign-ins, changes to users, groups and settings, job actions, key and password changes, and refused requests.';

/** Below this width the table becomes one card per entry (UX harvest D40). */
const AUDIT_CARD_BREAKPOINT = 600;

interface Props {
  onSessionExpired?: () => void;
}

/**
 * Colour a common action with a semantic tag colour. Reads a bit
 * better than a wall of white text. Anything unknown falls back
 * to neutral — we don't try to be exhaustive here.
 */
function actionColour(action: string): string {
  // Red is kept for real problems (refused sign-ins and requests). A delete
  // is a normal admin action, so it stays neutral.
  if (action.startsWith('login_fail') || action.endsWith('_denied')) return 'red';
  if (action.startsWith('login') || action === 'whoami') return 'green';
  if (action.startsWith('delete') || action.startsWith('remove')) return 'default';
  if (action.startsWith('create') || action.startsWith('add')) return 'blue';
  if (action.startsWith('update') || action.startsWith('put')) return 'geekblue';
  if (action.startsWith('public_read')) return 'cyan';
  return 'default';
}

type Entry = NonNullable<ReturnType<typeof useAuditLog>['data']>['entries'][number];

export default function AuditLogPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const narrow = useIsNarrow(AUDIT_CARD_BREAKPOINT);
  const [filter, setFilter] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(false);
  // Auto-refresh. 3s — quick enough for incident-debugging ("does my click show
  // up?") without being abusive. react-query skips a tick while a fetch is in
  // flight and pauses in a background tab.
  const audit = useAuditLog(autoRefresh ? 3000 : false);
  const entries = useMemo(() => audit.data?.entries ?? [], [audit.data]);
  // `now` follows the last fetch so the relative-time column stays honest
  // without re-rendering the world every second.
  const now = useMemo(() => new Date(audit.dataUpdatedAt || Date.now()), [audit.dataUpdatedAt]);
  useSessionExpiredOn(audit.error, onSessionExpired);
  // An expired session goes to sign-in (above); it is not an error state here.
  const view = { ...audit, error: audit.error && !isSessionExpired(audit.error) ? audit.error : null };

  // Client-side filter — free-text substring match across action,
  // user, target, ip, bucket, and path. Deliberately lenient:
  // operators paste IPs, access keys, bucket names verbatim and
  // expect hits regardless of which column the string lives in.
  const filtered = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return entries;
    return entries.filter((e) => {
      const blob =
        `${e.action} ${e.user} ${e.target} ${e.ip} ${e.bucket} ${e.path}`.toLowerCase();
      return blob.includes(q);
    });
  }, [entries, filter]);

  const card = {
    border: `1px solid ${colors.BORDER}`,
    borderRadius: 12,
    width: '100%',
    minWidth: 0,
    background: colors.BG_CARD,
  } as const;

  const actionTag = (e: Entry) => (
    <Tag color={actionColour(e.action)} style={{ margin: 0, fontFamily: 'var(--font-mono)', fontSize: 11 }}>
      {e.action || '—'}
    </Tag>
  );
  const time = (e: Entry) => (
    <span title={new Date(e.timestamp).toLocaleString()} style={{ color: colors.TEXT_SECONDARY, fontSize: 11 }}>
      {relativeTime(e.timestamp, { now })}
    </span>
  );

  /** Phone: one card per entry — time + action on line 1, user · ip · bucket · target on line 2. */
  const cards = (rows: Entry[]) => (
    <div style={card} role="list" aria-label="Audit entries">
      {rows.map((e, i) => (
        <div
          key={`${e.timestamp}-${i}`}
          role="listitem"
          style={{
            padding: `${SPACE.sm}px ${SPACE.md}px`,
            borderBottom: i < rows.length - 1 ? `1px solid ${colors.BORDER}` : 'none',
            display: 'flex',
            flexDirection: 'column',
            gap: SPACE.xxs,
            minWidth: 0,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs, flexWrap: 'wrap' }}>
            {actionTag(e)}
            {time(e)}
          </div>
          <div style={{ fontSize: 12, fontFamily: 'var(--font-mono)', color: colors.TEXT_SECONDARY, overflowWrap: 'anywhere' }}>
            {[e.user, e.ip, e.bucket, e.path || e.target].filter(Boolean).join(' · ') || '—'}
          </div>
        </div>
      ))}
    </div>
  );

  /** Desktop: the table — ONE grid owns the column tracks; the header and
   *  every row are subgrids of it, so all rows share the same widths. */
  const table = (rows: Entry[]) => (
    <div style={{ ...card, overflowX: 'auto', overflowY: 'hidden' }}>
      <div
        style={{
          display: 'grid',
          gridTemplateColumns: AUDIT_COLUMNS,
          // Fill the card; below the sum of the column minimums the card
          // scrolls sideways instead of squeezing the columns.
          width: '100%',
          minWidth: 720,
        }}
      >
        <div
          style={{
            ...AUDIT_ROW,
            padding: `${SPACE.sm}px ${SPACE.md}px`,
            borderBottom: `1px solid ${colors.BORDER}`,
            fontSize: 12,
            fontWeight: 600,
            color: colors.TEXT_MUTED,
            fontFamily: 'var(--font-ui)',
            background: colors.BG_ELEVATED,
          }}
        >
          <div>Time</div>
          <div>Action</div>
          <div>User</div>
          <div>IP</div>
          <div>Bucket</div>
          <div>Target / path</div>
        </div>
        {rows.map((e, i) => (
          <div
            key={`${e.timestamp}-${i}`}
            style={{
              ...AUDIT_ROW,
              padding: `${SPACE.xs}px ${SPACE.md}px`,
              borderBottom: i < rows.length - 1 ? `1px solid ${colors.BORDER}` : 'none',
              fontSize: 12,
              fontFamily: 'var(--font-mono)',
              alignItems: 'center',
            }}
          >
            <div>{time(e)}</div>
            <div>{actionTag(e)}</div>
            <div style={{ minWidth: 0, color: e.user ? colors.TEXT_PRIMARY : colors.TEXT_MUTED }}>
              <TruncatedName text={e.user || '—'} title={e.user || false} />
            </div>
            <div style={{ ...CELL_TRUNCATE_STYLE, color: colors.TEXT_SECONDARY }} title={e.ua ? `${e.ip} · ${e.ua}` : e.ip}>
              {e.ip || '—'}
            </div>
            <div style={{ minWidth: 0, color: colors.TEXT_SECONDARY }}>
              <TruncatedName text={e.bucket || '—'} title={e.bucket || false} />
            </div>
            <div style={{ minWidth: 0, color: colors.TEXT_SECONDARY }}>
              <TruncatedName
                text={e.path || e.target || '—'}
                title={e.path ? `${e.target} · ${e.path}` : e.target || false}
              />
            </div>
          </div>
        ))}
      </div>
    </div>
  );

  return (
    <div
      style={{
        // width:100% + min-width:0 pin this to the pane width so the wide
        // table's max-content scrolls within its card, not the page.
        ...contentColumn(CONTENT_WIDE),
        width: '100%',
        display: 'flex',
        flexDirection: 'column',
        minWidth: 0,
        boxSizing: 'border-box',
        gap: SPACE.md,
      }}
    >
      {/* Toolbar */}
      <Space size="middle" style={{ flexWrap: 'wrap' }}>
        <Input
          aria-label="Filter audit entries"
          size="middle"
          placeholder="Filter (action / user / ip / bucket / path)..."
          prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          style={{ width: narrow ? '100%' : 360 }}
          allowClear
        />
        <Button
          size="middle"
          icon={<ReloadOutlined />}
          onClick={() => void audit.refetch()}
          loading={audit.isFetching && !autoRefresh}
        >
          Refresh
        </Button>
        <Space size={SPACE.xs}>
          <Switch
            size="small"
            checked={autoRefresh}
            onChange={setAutoRefresh}
            aria-label="Auto-refresh every 3 seconds"
          />
          <Text style={{ fontSize: 12, color: colors.TEXT_MUTED }}>
            Auto-refresh (3s)
          </Text>
        </Space>
        <Text type="secondary" style={{ fontSize: 12 }}>
          {/* The one count on the page. */}
          {filter.trim()
            ? `${filtered.length} of ${entries.length} shown`
            : `${entries.length} ${entries.length === 1 ? 'entry' : 'entries'}`}
        </Text>
      </Space>
      {/* UX harvest D62: one quiet caption, the env var only on hover. */}
      <Text
        type="secondary"
        style={{ fontSize: 12, marginTop: -SPACE.xs }}
        title="Set the number of entries with DGP_AUDIT_RING_SIZE. The proxy log keeps the long-term record."
      >
        The most recent entries, kept in memory. A restart clears them.{' '}
        <InfoCircleOutlined aria-hidden />
      </Text>

      <QueryState
        query={view}
        what="the audit log"
        loading={<div style={{ ...card, padding: `0 ${SPACE.md}px` }}><LoadingState variant="list" rows={6} label="Loading audit entries" /></div>}
        isEmpty={(d) => d.entries.length === 0}
        empty={
          <div style={card}>
            <EmptyState icon={<FileTextOutlined />} title="No audit entries yet" hint={AUDIT_EMPTY_HINT} />
          </div>
        }
      >
        {() =>
          filtered.length === 0 ? (
            <div style={card}>
              <EmptyState compact title="No entries match this filter" hint="Change or clear the filter." />
            </div>
          ) : narrow ? (
            cards(filtered)
          ) : (
            table(filtered)
          )
        }
      </QueryState>
    </div>
  );
}
