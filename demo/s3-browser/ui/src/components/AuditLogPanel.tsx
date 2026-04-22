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
import {
  ReloadOutlined,
  SearchOutlined,
  FileTextOutlined,
} from '@ant-design/icons';
import { useColors } from '../ThemeContext';
import { fetchAudit, type AuditEntry } from '../adminApi';

const { Text } = Typography;

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

/**
 * Compact time-ago in the same row as the absolute timestamp.
 * Example: "2m ago". No localisation today (§10.2 i18n scaffold
 * exists but this panel is English-only for now).
 */
function relativeTime(iso: string, now: Date): string {
  const t = new Date(iso).getTime();
  const deltaSec = Math.floor((now.getTime() - t) / 1000);
  if (deltaSec < 0) return 'just now';
  if (deltaSec < 60) return `${deltaSec}s ago`;
  if (deltaSec < 3600) return `${Math.floor(deltaSec / 60)}m ago`;
  if (deltaSec < 86400) return `${Math.floor(deltaSec / 3600)}h ago`;
  return `${Math.floor(deltaSec / 86400)}d ago`;
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

  // Auto-refresh. Interval is intentionally 3s — quick enough for
  // incident-debugging ("does my click show up?") without being
  // abusive.
  useEffect(() => {
    if (!autoRefresh) return;
    const id = setInterval(() => void refresh(), 3000);
    return () => clearInterval(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [autoRefresh]);

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
        // Responsive-pad wrapper (same as AdmissionPanel et al.) so
        // the audit viewer doesn't render flush against the sidebar.
        // Width 1100: the 6-column table (time / action / user / ip /
        // bucket / target) needs more breathing room than the 960
        // form panels, otherwise Target/Path clips on common paths.
        maxWidth: 1100,
        margin: '0 auto',
        padding: 'clamp(16px, 3vw, 24px)',
        display: 'flex',
        flexDirection: 'column',
        gap: 16,
      }}
    >
      <Alert
        type="info"
        showIcon
        icon={<FileTextOutlined />}
        message="In-memory audit ring"
        description={
          <>
            Shows the most-recent {entries.length === 0 ? '' : entries.length + ' '}
            audit entries from this process. The ring is bounded (default 500
            entries; set <code>DGP_AUDIT_RING_SIZE</code> to change). Stdout /
            your log pipeline remains authoritative for long-term audit —
            this panel is a debugging convenience for incidents.
          </>
        }
      />

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
          overflow: 'hidden',
          background: colors.BG_CARD,
        }}
      >
        <div
          style={{
            display: 'grid',
            gridTemplateColumns: '170px 150px 140px 120px 100px 1fr',
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
                gridTemplateColumns: '170px 150px 140px 120px 100px 1fr',
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
                  color: e.user ? colors.TEXT_PRIMARY : colors.TEXT_MUTED,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                }}
                title={e.user}
              >
                {e.user || '—'}
              </div>
              <div
                style={{
                  color: colors.TEXT_SECONDARY,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                }}
                title={e.ua ? `${e.ip} · ${e.ua}` : e.ip}
              >
                {e.ip || '—'}
              </div>
              <div
                style={{
                  color: colors.TEXT_SECONDARY,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                }}
                title={e.bucket}
              >
                {e.bucket || '—'}
              </div>
              <div
                style={{
                  color: colors.TEXT_SECONDARY,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                }}
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
