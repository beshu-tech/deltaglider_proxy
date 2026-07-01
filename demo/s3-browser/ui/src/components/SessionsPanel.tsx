/**
 * SessionsPanel — live admin session list + force-logout (revoke).
 *
 * Closes the security hole where a stolen admin cookie could only be killed by
 * restarting the proxy: an admin can now see every live session and revoke one
 * (DELETE /sessions/:id) or all sessions of an IAM key (revoke-user). Sessions
 * are in-memory, so a proxy restart still clears everything.
 */
import { useCallback, useEffect, useState } from 'react';
import { Typography, Button, Tag, Table, Space, message, Input } from 'antd';
import { ReloadOutlined, LogoutOutlined } from '@ant-design/icons';
import { listSessions, revokeSession, revokeUserSessions, type SessionSummary } from '../adminApi';

const { Text } = Typography;

function ageLabel(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

export default function SessionsPanel({ onSessionExpired }: { onSessionExpired?: () => void }) {
  const [rows, setRows] = useState<SessionSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [revokeKey, setRevokeKey] = useState('');

  const refresh = useCallback(async () => {
    setLoading(true);
    try {
      setRows(await listSessions());
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) { onSessionExpired?.(); return; }
      message.error(e instanceof Error ? e.message : 'Failed to load sessions');
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired]);

  useEffect(() => { void refresh(); }, [refresh]);

  const revokeOne = async (id: string) => {
    if (!window.confirm(`Force-logout session ${id}? That session is signed out immediately.`)) return;
    try {
      setBusy(id);
      await revokeSession(id);
      message.success('Session revoked');
      await refresh();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Failed to revoke session');
    } finally {
      setBusy(null);
    }
  };

  const revokeUser = async () => {
    const key = revokeKey.trim();
    if (!key) return;
    if (!window.confirm(`Force-logout ALL sessions of identity "${key}" — on every instance. If this is YOUR OWN identity you will be logged out too. Use this after rotating a compromised key.`)) return;
    try {
      setBusy('user');
      const res = await revokeUserSessions(key);
      const local = `Revoked ${res.revoked_local} session${res.revoked_local === 1 ? '' : 's'} locally`;
      if (!res.persisted) {
        message.warning(`${local} — but the revocation could NOT be persisted; sessions on OTHER instances stay valid. Retry, or restart the peers.`);
      } else {
        message.success(`${local}; peers converge within ~${Math.ceil((res.propagation_bound_secs ?? 300) / 60)} min`);
      }
      setRevokeKey('');
      await refresh();
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Failed to revoke user sessions');
    } finally {
      setBusy(null);
    }
  };

  const columns = [
    { title: 'Session', dataIndex: 'id', key: 'id', render: (v: string) => <Text code>{v}</Text> },
    {
      title: 'Auth',
      dataIndex: 'auth',
      key: 'auth',
      render: (v: string, r: SessionSummary) => (
        <Space size={4}>
          <Tag color={v === 'bootstrap' ? 'gold' : v === 'external' ? 'purple' : 'blue'}>{v}</Tag>
          {r.admin_gui ? <Tag color="red">admin</Tag> : <Tag>browser</Tag>}
        </Space>
      ),
    },
    { title: 'Identity', dataIndex: 'identity', key: 'identity', render: (v: string | null) => v ?? <Text type="secondary">—</Text> },
    { title: 'IP', dataIndex: 'ip', key: 'ip', render: (v: string | null) => v ?? <Text type="secondary">—</Text> },
    { title: 'Age', dataIndex: 'age_secs', key: 'age', render: (v: number) => ageLabel(v) },
    {
      title: '',
      key: 'action',
      render: (_: unknown, r: SessionSummary) => (
        <Button danger size="small" icon={<LogoutOutlined />} loading={busy === r.id} onClick={() => void revokeOne(r.id)}>
          Revoke
        </Button>
      ),
    },
  ];

  return (
    <div style={{ maxWidth: 1100, margin: '0 auto', padding: 'clamp(12px, 2vw, 18px)', display: 'flex', flexDirection: 'column', gap: 14 }}>
      <Space>
        <Button icon={<ReloadOutlined />} onClick={() => void refresh()} loading={loading}>Refresh</Button>
        <Text type="secondary">{rows.length} live session{rows.length === 1 ? '' : 's'}</Text>
      </Space>
      <Table rowKey="id" size="small" columns={columns} dataSource={rows} loading={loading} pagination={false} />
      <Space.Compact style={{ maxWidth: 480 }}>
        <Input
          placeholder="identity to force-logout: access_key_id, or provider:user-id for external users"
          value={revokeKey}
          onChange={(e) => setRevokeKey(e.target.value)}
          onPressEnter={() => void revokeUser()}
        />
        <Button danger loading={busy === 'user'} disabled={!revokeKey.trim()} onClick={() => void revokeUser()}>
          Revoke key
        </Button>
      </Space.Compact>
    </div>
  );
}
