/**
 * SessionsPanel — live admin session list + force-logout (revoke).
 *
 * Closes the security hole where a stolen admin cookie could only be killed by
 * restarting the proxy: an admin can now see every live session and revoke one
 * (DELETE /sessions/:id) or all sessions of an IAM key (revoke-user). Sessions
 * are in-memory, so a proxy restart still clears everything.
 */
import { useEffect, useState } from 'react';
import { confirmDialog } from '../confirmDialog';
import { Typography, Button, Tag, Table, Space, message, Input } from 'antd';
import { ReloadOutlined, LogoutOutlined } from '@ant-design/icons';
import type { SessionSummary } from '../adminApi';
import { useRevokeSession, useRevokeUserSessions, useSessions } from '../queries/sessions';
import { contentColumn, CONTENT_WIDE } from './shared-styles';
import { normalizeUiError } from '../errorHandling';
import { isSessionExpired } from '../errorHandling';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { formatDuration } from '../utils';
import RowActionsMenu from './RowActionsMenu';

const { Text } = Typography;

export default function SessionsPanel({ onSessionExpired }: { onSessionExpired?: () => void }) {
  const sessions = useSessions();
  const revokeSessionMutation = useRevokeSession();
  const revokeUserMutation = useRevokeUserSessions();
  const rows = sessions.data ?? [];
  const loading = sessions.isFetching;
  const [busy, setBusy] = useState<string | null>(null);
  const [revokeKey, setRevokeKey] = useState('');

  useSessionExpiredOn(sessions.error, onSessionExpired);
  // A load failure is a toast, as before; an expiry goes to sign-in instead.
  useEffect(() => {
    if (sessions.error && !isSessionExpired(sessions.error)) {
      message.error(normalizeUiError(sessions.error, 'Failed to load sessions'));
    }
  }, [sessions.error]);

  const revokeOne = async (id: string) => {
    try {
      setBusy(id);
      await revokeSessionMutation.mutateAsync(id);
      message.success('Session revoked');
    } catch (e) {
      message.error(normalizeUiError(e, 'Failed to revoke session'));
    } finally {
      setBusy(null);
    }
  };

  const revokeUser = async () => {
    const key = revokeKey.trim();
    if (!key) return;
    // Say what the click does: the sessions it ends here, or that none here
    // uses the identity (the sign-out still reaches the other instances,
    // where this list cannot look).
    const here = rows.filter((r) => r.identity === key).length;
    const confirmed = here > 0
      ? await confirmDialog({
          title: `Sign out ${here === 1 ? 'the session' : `all ${here} sessions`} of "${key}"?`,
          content: 'This ends the sessions on every instance. If this is your own identity, you are signed out too. Use it after you rotate a compromised key.',
          okText: 'Sign out all sessions',
          danger: true,
        })
      : await confirmDialog({
          title: `No live session on this instance uses "${key}"`,
          content: 'Check the access key ID. If you run more than one instance, a session of this identity can still be live on another one: sign-out also ends those.',
          okText: 'Sign out on other instances',
          cancelText: 'Cancel',
        });
    if (!confirmed) return;
    try {
      setBusy('user');
      const res = await revokeUserMutation.mutateAsync(key);
      const local = `Revoked ${res.revoked_local} session${res.revoked_local === 1 ? '' : 's'} locally`;
      if (!res.persisted) {
        message.warning(`${local} — but the revocation could NOT be persisted; sessions on OTHER instances stay valid. Retry, or restart the peers.`);
      } else {
        message.success(`${local}; peers converge within ~${Math.ceil((res.propagation_bound_secs ?? 300) / 60)} min`);
      }
      setRevokeKey('');
    } catch (e) {
      message.error(normalizeUiError(e, 'Failed to revoke user sessions'));
    } finally {
      setBusy(null);
    }
  };

  const columns = [
    {
      title: 'Session',
      dataIndex: 'id',
      key: 'id',
      // nowrap: a 12-character id broke into three lines on a phone.
      render: (v: string, r: SessionSummary) => (
        <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6, whiteSpace: 'nowrap' }}>
          <Text code style={{ whiteSpace: 'nowrap' }}>{v}</Text>
          {r.current && <Tag style={{ margin: 0 }}>you</Tag>}
        </span>
      ),
    },
    {
      title: 'Auth',
      dataIndex: 'auth',
      key: 'auth',
      // Secondary columns step aside on a narrow screen (AntD breakpoints).
      responsive: ['sm' as const],
      render: (v: string, r: SessionSummary) => (
        <Space size={4}>
          {/* Neutral chips: red and amber are kept for real problems. */}
          <Tag>{v}</Tag>
          {r.admin_gui ? <Tag color="blue">admin</Tag> : <Tag>browser</Tag>}
        </Space>
      ),
    },
    // A column that is "—" on every row (bootstrap-password sessions have no
    // identity) says nothing: it shows only when one row has an identity.
    ...(rows.some((r) => r.identity)
      ? [{ title: 'Identity', dataIndex: 'identity', key: 'identity', render: (v: string | null) => v ?? <Text type="secondary">—</Text> }]
      : []),
    { title: 'IP', dataIndex: 'ip', key: 'ip', responsive: ['md' as const], render: (v: string | null) => v ?? <Text type="secondary">—</Text> },
    // Age stays on a phone: it tells an old session from a fresh one.
    { title: 'Age', dataIndex: 'age_secs', key: 'age', render: (v: number) => formatDuration(v) },
    {
      title: 'Actions',
      key: 'action',
      align: 'right' as const,
      width: 80,
      // Every row has the same "⋯" menu. The server refuses to revoke the
      // caller's own session, so that row's item is off and says why (the
      // "you" tag in the Session cell marks the row).
      render: (_: unknown, r: SessionSummary) => (
        <RowActionsMenu
          label={`More actions for session ${r.id}`}
          loading={busy === r.id}
          actions={[
            r.current
              ? {
                  key: 'revoke',
                  label: 'Your session: use Sign out in the account menu',
                  icon: <LogoutOutlined />,
                  disabled: true,
                  onSelect: () => {},
                }
              : {
                  key: 'revoke',
                  label: 'Sign out this session…',
                  icon: <LogoutOutlined />,
                  danger: true,
                  confirm: {
                    title: 'Sign out this session?',
                    content: `Session ${r.id}${r.identity ? ` (${r.identity})` : ''} is signed out immediately.`,
                    okText: 'Sign out',
                  },
                  onSelect: () => revokeOne(r.id),
                },
          ]}
        />
      ),
    },
  ];

  return (
    <div style={{ ...contentColumn(CONTENT_WIDE), padding: 'clamp(12px, 2vw, 18px)', display: 'flex', flexDirection: 'column', gap: 14 }}>
      <Space>
        <Button icon={<ReloadOutlined />} onClick={() => void sessions.refetch()} loading={loading}>Refresh</Button>
        <Text type="secondary">{rows.length} live session{rows.length === 1 ? '' : 's'}</Text>
      </Space>
      <Table rowKey="id" size="small" columns={columns} dataSource={rows} loading={loading} pagination={false} scroll={{ x: 'max-content' }} />
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, maxWidth: 480, width: '100%' }}>
        <label htmlFor="sessions-revoke-identity" style={{ fontSize: 13.5, fontWeight: 600 }}>
          Sign out every session of an identity
        </label>
        <Space.Compact style={{ width: '100%' }}>
          <Input
            id="sessions-revoke-identity"
            aria-describedby="sessions-revoke-identity-help"
            placeholder="Access key or provider:user"
            value={revokeKey}
            onChange={(e) => setRevokeKey(e.target.value)}
            onPressEnter={() => void revokeUser()}
          />
          <Button danger loading={busy === 'user'} disabled={!revokeKey.trim()} onClick={() => void revokeUser()}>
            Revoke key
          </Button>
        </Space.Compact>
        <Text id="sessions-revoke-identity-help" type="secondary" style={{ fontSize: 12.5 }}>
          An IAM user&apos;s access key ID, or provider:user-id for a user who signs in with an external provider.
        </Text>
      </div>
    </div>
  );
}
