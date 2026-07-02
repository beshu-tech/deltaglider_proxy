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
import { useEffect, useMemo, useState } from 'react';
import { Typography, Input, Button, Tag, Alert, Space, Switch } from 'antd';
import { useVisiblePolling } from '../useVisiblePolling';
import {
  ReloadOutlined,
  SearchOutlined,
  FileTextOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { fetchAudit, type AuditEntry } from '../adminApi';
import { relativeTime } from '../utils';
import { contentColumn, CONTENT_WIDE } from './shared-styles';

const { Text } = Typography;

/** Single-line, ellipsis-on-overflow cell — shared by the user/ip/bucket/target columns. */
const CELL_TRUNCATE_STYLE = {
  overflow: 'hidden',
  textOverflow: 'ellipsis',
  whiteSpace: 'nowrap',
} as const;

interface Props {
  onSessionExpired?: () => void;
}

/**
 * Colour a common action with a semantic tag colour. Reads a bit
 * better than a wall of white text. Anything unknown falls back
 * to neutral — we don't try to be exhaustive here.
 */
function actionColour(action: string): string {
  if (action.startsWith('login_fail')) return 'red';
  if (action.startsWith('login') || action === 'whoami') return 'green';
  if (action.startsWith('delete') || action.startsWith('remove')) return 'volcano';
  if (action.startsWith('create') || action.startsWith('add')) return 'blue';
  if (action.startsWith('update') || action.startsWith('put')) return 'geekblue';
  if (action.startsWith('public_read')) return 'cyan';
  return 'default';
}

export default function AuditLogPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const [entries, setEntries] = useState<AuditEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(false);
  // `now` is refreshed on the same cadence as the fetch so the
  // relative-time column stays honest without re-rendering the
  // world every second.
  const [now, setNow] = useState(() => new Date());

  const refresh = async () => {
    try {
      setLoading(true);
      const res = await fetchAudit(500);
      setEntries(res.entries);
      setNow(new Date());
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(
        `Failed to load audit entries: ${e instanceof Error ? e.message : 'unknown'}`
      );
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Auto-refresh. 3s — quick enough for incident-debugging ("does my click show
  // up?") without being abusive. Visibility-gated: a backgrounded tab stops
  // polling and catches up on return, instead of hammering the audit endpoint.
  useVisiblePolling(() => void refresh(), 3000, autoRefresh);

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
        gap: 16,
      }}
    >
      {/* One quiet line — TabHeader already titles the page "Audit log".
          Stdout / your log pipeline stays authoritative for long-term audit. */}
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          padding: '8px 12px',
          borderLeft: `3px solid ${colors.ACCENT_BLUE}`,
          background: colors.BG_ELEVATED,
          borderRadius: 6,
          fontSize: 12.5,
          color: colors.TEXT_SECONDARY,
          lineHeight: 1.5,
        }}
      >
        <FileTextOutlined style={{ color: colors.ACCENT_BLUE, flexShrink: 0 }} aria-hidden />
        <span>
          Newest {entries.length || 500} entries kept in memory for quick inspection.{' '}
          <code
            title="Environment variable controlling the ring size (default 500)"
            style={{ fontFamily: 'var(--font-mono)', fontSize: 11, opacity: 0.6, color: colors.TEXT_MUTED }}
          >
            DGP_AUDIT_RING_SIZE
          </code>
        </span>
      </div>

      {error && <Alert type="error" showIcon message="Fetch failed" description={error} />}

      {/* Toolbar */}
      <Space size="middle" style={{ flexWrap: 'wrap' }}>
        <Input
          size="middle"
          placeholder="Filter (action / user / ip / bucket / path)..."
          prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          style={{ width: 360 }}
          allowClear
        />
        <Button
          size="middle"
          icon={<ReloadOutlined />}
          onClick={() => void refresh()}
          loading={loading && !autoRefresh}
        >
          Refresh
        </Button>
        <Space size={8}>
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
          {filtered.length} of {entries.length} shown
        </Text>
      </Space>

      {/* Table — hand-rolled so we can tightly control the monospace
          IP / action columns. AntD's Table would work too but is
          heavier than this view needs. */}
      <div
        style={{
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 8,
          // width-capped so the inner grid's max-content scrolls WITHIN the card
          // instead of widening the whole page on narrow.
          width: '100%',
          minWidth: 0,
          overflowX: 'auto',
          overflowY: 'hidden',
          background: colors.BG_CARD,
        }}
      >
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: '170px 150px 140px 120px 100px minmax(120px, 1fr)',
            width: 'max-content',
            gap: 0,
            padding: '10px 14px',
            borderBottom: `1px solid ${colors.BORDER}`,
            fontSize: 11,
            fontWeight: 700,
            letterSpacing: 0.5,
            textTransform: 'uppercase',
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
          <div>Target / Path</div>
        </div>
        {filtered.length === 0 ? (
          <div
            style={{
              padding: 40,
              textAlign: 'center',
              color: colors.TEXT_MUTED,
              fontSize: 13,
            }}
          >
            {loading ? 'Loading…' : 'No entries match this filter.'}
          </div>
        ) : (
          filtered.map((e, i) => (
            <div
              key={`${e.timestamp}-${i}`}
              style={{
                display: 'grid',
                gridTemplateColumns: '170px 150px 140px 120px 100px minmax(120px, 1fr)',
                width: 'max-content',
                minWidth: '100%',
                gap: 0,
                padding: '8px 14px',
                borderBottom:
                  i < filtered.length - 1
                    ? `1px solid ${colors.BORDER}`
                    : 'none',
                fontSize: 12,
                fontFamily: 'var(--font-mono)',
                alignItems: 'center',
              }}
            >
              <div
                title={new Date(e.timestamp).toLocaleString()}
                style={{ color: colors.TEXT_SECONDARY, fontSize: 11 }}
              >
                {relativeTime(e.timestamp, now)}
              </div>
              <div>
                <Tag
                  color={actionColour(e.action)}
                  style={{
                    margin: 0,
                    fontFamily: 'var(--font-mono)',
                    fontSize: 11,
                  }}
                >
                  {e.action || '—'}
                </Tag>
              </div>
              <div
                style={{
                  ...CELL_TRUNCATE_STYLE,
                  color: e.user ? colors.TEXT_PRIMARY : colors.TEXT_MUTED,
                }}
                title={e.user}
              >
                {e.user || '—'}
              </div>
              <div
                style={{ ...CELL_TRUNCATE_STYLE, color: colors.TEXT_SECONDARY }}
                title={e.ua ? `${e.ip} · ${e.ua}` : e.ip}
              >
                {e.ip || '—'}
              </div>
              <div
                style={{ ...CELL_TRUNCATE_STYLE, color: colors.TEXT_SECONDARY }}
                title={e.bucket}
              >
                {e.bucket || '—'}
              </div>
              <div
                style={{ ...CELL_TRUNCATE_STYLE, color: colors.TEXT_SECONDARY }}
                title={e.path ? `${e.target} · ${e.path}` : e.target}
              >
                {e.path || e.target || '—'}
              </div>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
