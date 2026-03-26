import { useState, useEffect, useCallback } from 'react';
import { Button, Typography, Spin, Alert, Input } from 'antd';
import { PlusOutlined, SearchOutlined, TeamOutlined } from '@ant-design/icons';
import type { IamUser } from '../adminApi';
import { getUsers } from '../adminApi';
import { useColors } from '../ThemeContext';
import UserForm from './UserForm';

const { Text } = Typography;

function permissionSummary(user: IamUser): string {
  if (user.permissions.length === 0) return 'No access';
  const hasAll = user.permissions.some(p => p.actions.includes('*') && p.resources.includes('*'));
  if (hasAll) return 'Full admin';
  return `${user.permissions.length} rule${user.permissions.length !== 1 ? 's' : ''}`;
}

interface UsersPanelProps {
  onSessionExpired?: () => void;
}

export default function UsersPanel({ onSessionExpired }: UsersPanelProps) {
  const colors = useColors();
  const [users, setUsers] = useState<IamUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState('');
  const [newCreds, setNewCreds] = useState<{ ak: string; sk: string } | null>(null);

  const loadUsers = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const data = await getUsers();
      setUsers(data);
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Failed to load users';
      if (msg.includes('401')) onSessionExpired?.();
      else setError(msg);
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired]);

  useEffect(() => { loadUsers(); }, [loadUsers]);

  const selectedUser = users.find(u => u.id === selectedId) ?? null;
  const filtered = search
    ? users.filter(u => u.name.toLowerCase().includes(search.toLowerCase()) || u.access_key_id.toLowerCase().includes(search.toLowerCase()))
    : users;

  const handleSelect = (user: IamUser) => {
    setCreating(false);
    setSelectedId(user.id);
    setNewCreds(null);
  };

  const handleCreate = () => {
    setSelectedId(null);
    setCreating(true);
    setNewCreds(null);
  };

  const handleSaved = () => {
    loadUsers();
  };

  const handleCreated = async (ak: string, sk: string) => {
    // Reload users, select the new one, show credentials banner
    const data = await getUsers().catch(() => []);
    setUsers(data);
    const newUser = data.find(u => u.access_key_id === ak);
    if (newUser) setSelectedId(newUser.id);
    setCreating(false);
    setNewCreds({ ak, sk });
  };

  const handleDeleted = () => {
    setSelectedId(null);
    setCreating(false);
    setNewCreds(null);
    loadUsers();
  };

  return (
    <div style={{ display: 'flex', height: '100%', overflow: 'hidden' }}>
      {/* Left: User List */}
      <div style={{
        width: 300,
        minWidth: 260,
        borderRight: `1px solid ${colors.BORDER}`,
        display: 'flex',
        flexDirection: 'column',
        overflow: 'hidden',
      }}>
        {/* Header */}
        <div style={{ padding: '16px 16px 12px', borderBottom: `1px solid ${colors.BORDER}` }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 10 }}>
            <Text strong style={{ fontSize: 14 }}>Users</Text>
            <Button type="primary" size="small" icon={<PlusOutlined />} onClick={handleCreate}>
              New
            </Button>
          </div>
          <Input
            prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
            placeholder="Search users..."
            value={search}
            onChange={e => setSearch(e.target.value)}
            allowClear
            size="small"
            style={{ borderRadius: 6 }}
          />
        </div>

        {/* User List */}
        <div style={{ flex: 1, overflow: 'auto', padding: '4px 0' }}>
          {loading && users.length === 0 && (
            <div style={{ textAlign: 'center', padding: 32 }}><Spin /></div>
          )}
          {error && (
            <Alert type="error" message={error} showIcon style={{ margin: 8, borderRadius: 8 }} />
          )}
          {!loading && users.length === 0 && !error && (
            <div style={{ padding: 20, textAlign: 'center' }}>
              <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 8 }}>No IAM users yet</Text>
              <Text type="secondary" style={{ fontSize: 11, display: 'block', marginBottom: 12 }}>
                Your current credentials will be migrated automatically as an admin user.
              </Text>
              <Button type="primary" size="small" icon={<PlusOutlined />} onClick={handleCreate}>
                Set Up IAM
              </Button>
            </div>
          )}
          {filtered.map(user => {
            const isSelected = user.id === selectedId && !creating;
            return (
              <div
                key={user.id}
                onClick={() => handleSelect(user)}
                style={{
                  padding: '10px 16px',
                  cursor: 'pointer',
                  background: isSelected ? colors.ACCENT_BLUE + '18' : 'transparent',
                  borderLeft: isSelected ? `3px solid ${colors.ACCENT_BLUE}` : '3px solid transparent',
                  transition: 'all 0.15s ease',
                }}
                onMouseEnter={e => { if (!isSelected) e.currentTarget.style.background = colors.BORDER + '40'; }}
                onMouseLeave={e => { if (!isSelected) e.currentTarget.style.background = 'transparent'; }}
              >
                <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                  <div style={{
                    width: 8, height: 8, borderRadius: '50%',
                    background: user.enabled ? '#52c41a' : colors.TEXT_MUTED,
                    flexShrink: 0,
                  }} />
                  <Text strong style={{ fontSize: 13, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                    {user.name}
                  </Text>
                </div>
                <div style={{ marginLeft: 16, marginTop: 2 }}>
                  <Text type="secondary" style={{ fontSize: 11, fontFamily: 'var(--font-mono)' }}>
                    {user.access_key_id.length > 24
                      ? user.access_key_id.substring(0, 20) + '...'
                      : user.access_key_id}
                  </Text>
                  <Text type="secondary" style={{ fontSize: 11, marginLeft: 8 }}>
                    · {permissionSummary(user)}
                  </Text>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      {/* Right: Detail Form */}
      <div style={{ flex: 1, overflow: 'auto', background: colors.BG_CARD }}>
        {/* Credentials banner after create */}
        {newCreds && (
          <div style={{ padding: '16px 28px 0' }}>
            <Alert
              type="success"
              showIcon
              closable
              onClose={() => setNewCreds(null)}
              message="User created — save these credentials"
              description={
                <div style={{ marginTop: 8 }}>
                  <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase' }}>Access Key</Text>
                  <div><Text code copyable style={{ fontFamily: 'var(--font-mono)' }}>{newCreds.ak}</Text></div>
                  <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', marginTop: 8, display: 'block' }}>Secret Key</Text>
                  <div><Text code copyable style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{newCreds.sk}</Text></div>
                  <Text type="warning" style={{ fontSize: 11, marginTop: 8, display: 'block' }}>The secret will not be shown again.</Text>
                </div>
              }
              style={{ borderRadius: 8 }}
            />
          </div>
        )}

        {creating ? (
          <UserForm
            user={null}
            onSaved={handleSaved}
            onCreated={handleCreated}
            onCancel={() => setCreating(false)}
          />
        ) : selectedUser ? (
          <UserForm
            key={selectedUser.id}
            user={selectedUser}
            onSaved={handleSaved}
            onDeleted={handleDeleted}
          />
        ) : (
          <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: '100%', color: colors.TEXT_MUTED }}>
            <div style={{ textAlign: 'center', maxWidth: 360, padding: 24 }}>
              {users.length === 0 ? (
                <>
                  <TeamOutlined style={{ fontSize: 40, marginBottom: 12, color: colors.TEXT_MUTED }} />
                  <div><Text type="secondary" style={{ fontSize: 15, fontWeight: 500 }}>Multi-User Access Control</Text></div>
                  <Text type="secondary" style={{ fontSize: 12, display: 'block', marginTop: 8 }}>
                    Create your first IAM user to enable per-user credentials and permissions.
                    Your current login credentials will be preserved as an admin account automatically.
                  </Text>
                </>
              ) : (
                <Text type="secondary" style={{ fontSize: 14 }}>Select a user to edit, or create a new one</Text>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
