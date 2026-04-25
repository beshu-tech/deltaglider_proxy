import { useState } from 'react';
import { Button, Typography, Spin, Alert, Input } from 'antd';
import { PlusOutlined, SearchOutlined, TeamOutlined, DeleteOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import type { IamUser } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useUsers, useDeleteUser } from '../queries/users';
import { useAdminConfig } from '../queries/config';
import { qk } from '../queries/keys';
import UserForm from './UserForm';
import CredentialsBanner from './CredentialsBanner';
import IamSourceBanner from './IamSourceBanner';

const { Text } = Typography;

function permissionSummary(user: IamUser): string | null {
  const groupCount = user.group_ids?.length ?? 0;
  if (user.permissions.length === 0) {
    // SSO users with no direct rules get permissions from groups — don't show
    // a confusing label; the SSO badge and detail panel are enough context.
    if (user.auth_source === 'external') return null;
    // Wave 11 post-manual-review fix (UX-5): a user with group memberships
    // but no direct rules effectively HAS access (inherited); labelling
    // that "No access" was misleading new admins. Surface the inheritance
    // instead. The editor's "EFFECTIVE PERMISSIONS (X DIRECT + Y INHERITED)"
    // breakdown gives the full story on drill-down.
    if (groupCount > 0) {
      return `${groupCount} group${groupCount !== 1 ? 's' : ''} (inherited)`;
    }
    return 'No access';
  }
  const hasAll = user.permissions.some(p => p.actions.includes('*') && p.resources.includes('*'));
  if (hasAll) return 'Full admin';
  const rulePart = `${user.permissions.length} rule${user.permissions.length !== 1 ? 's' : ''}`;
  if (groupCount > 0) {
    return `${rulePart} · ${groupCount} group${groupCount !== 1 ? 's' : ''}`;
  }
  return rulePart;
}

interface UsersPanelProps {
  onSessionExpired?: () => void;
  onSavingChange?: (saving: boolean) => void;
  onNavigateToGroup?: (groupId: number) => void;
}

export default function UsersPanel({ onSessionExpired, onSavingChange, onNavigateToGroup }: UsersPanelProps) {
  const colors = useColors();
  const qc = useQueryClient();
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState('');
  const [newCreds, setNewCreds] = useState<{ ak: string; sk: string } | null>(null);

  // IAM mode banner: tells the operator where user state lives (DB in
  // `gui` mode vs YAML in `declarative`). Snapshot once via TanStack
  // Query — refetch is automatic if the mode flips elsewhere in the app.
  const { data: cfg } = useAdminConfig();
  const iamMode = cfg?.iam_mode;

  // Users list. Query handles loading/error/refetch automatically;
  // mutations on this resource invalidate this key (see queries/users.ts)
  // so we never have to manually call a refresh callback.
  const usersQuery = useUsers();
  const users = usersQuery.data ?? [];
  const loading = usersQuery.isLoading;
  const rawError = usersQuery.error;
  const error = rawError ? (rawError instanceof Error ? rawError.message : 'Failed to load users') : '';

  // Bubble up 401 to the parent so the login screen can take over.
  // Useful side-effect of being called once when the error transitions.
  if (rawError && rawError instanceof Error && rawError.message.includes('401')) {
    onSessionExpired?.();
  }

  const deleteMutation = useDeleteUser();

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
    qc.invalidateQueries({ queryKey: qk.users.list() });
  };

  const handleCreated = async (ak: string, sk: string) => {
    // Refetch synchronously to capture the new user's ID, then select it.
    const result = await qc.fetchQuery({ queryKey: qk.users.list(), queryFn: () => import('../adminApi').then(m => m.getUsers()) });
    const newUser = result.find(u => u.access_key_id === ak);
    if (newUser) setSelectedId(newUser.id);
    setCreating(false);
    setNewCreds({ ak, sk });
  };

  const handleDeleted = () => {
    setSelectedId(null);
    setCreating(false);
    setNewCreds(null);
    qc.invalidateQueries({ queryKey: qk.users.list() });
  };

  return (
    <div style={{ display: 'flex', flexDirection: 'column', height: '100%', overflow: 'hidden' }}>
      {/* "Where does this data live?" banner — IAM state is DB-backed
          in GUI mode, YAML-authoritative in Declarative mode. Shows
          on every IAM panel so operators never wonder why Copy YAML
          on Access shows `access: {}` after adding a user. */}
      <div style={{ padding: '12px 16px 0' }}>
        <IamSourceBanner iamMode={iamMode} resource="users" />
      </div>
      <div style={{ display: 'flex', flex: 1, overflow: 'hidden' }}>
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
            const isExternal = user.auth_source === 'external';
            const summary = permissionSummary(user);
            return (
              <div
                key={user.id}
                onClick={() => handleSelect(user)}
                className="user-list-item"
                style={{
                  padding: '12px 16px',
                  cursor: 'pointer',
                  background: isSelected ? colors.ACCENT_BLUE + '18' : 'transparent',
                  borderLeft: isSelected ? `3px solid ${colors.ACCENT_BLUE}` : '3px solid transparent',
                  transition: 'all 0.15s ease',
                  position: 'relative',
                }}
                onMouseEnter={e => { if (!isSelected) e.currentTarget.style.background = colors.BORDER + '40'; }}
                onMouseLeave={e => { if (!isSelected) e.currentTarget.style.background = 'transparent'; }}
              >
                {/* Row 1: status dot + name + badges + delete */}
                <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                  <div style={{
                    width: 8, height: 8, borderRadius: '50%',
                    background: user.enabled ? colors.ACCENT_GREEN : colors.TEXT_MUTED,
                    flexShrink: 0,
                  }} />
                  <Text strong style={{ fontSize: 14, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1, fontFamily: 'var(--font-ui)' }}>
                    {user.name}
                  </Text>
                  {isExternal && (
                    <span style={{
                      fontSize: 9, fontWeight: 700, letterSpacing: 0.5,
                      color: colors.ACCENT_PURPLE, background: colors.ACCENT_PURPLE + '18',
                      padding: '2px 6px', borderRadius: 4, fontFamily: 'var(--font-ui)',
                      textTransform: 'uppercase', flexShrink: 0,
                    }}>SSO</span>
                  )}
                  <Button
                    type="text"
                    danger
                    size="small"
                    icon={<DeleteOutlined />}
                    onClick={(e) => {
                      e.stopPropagation();
                      if (!window.confirm(`Delete "${user.name}"? This cannot be undone.`)) return;
                      deleteMutation.mutate(user.id, {
                        onSuccess: handleDeleted,
                        onError: (err) => console.error('Delete user failed:', err),
                      });
                    }}
                    style={{ opacity: 0.5, padding: '2px 4px', minWidth: 0, flexShrink: 0 }}
                    onMouseEnter={e => { e.currentTarget.style.opacity = '1'; }}
                    onMouseLeave={e => { e.currentTarget.style.opacity = '0.5'; }}
                  />
                </div>
                {/* Row 2: permission summary (hidden for SSO users with group-only access) */}
                {summary && (
                  <div style={{ marginLeft: 16, marginTop: 4 }}>
                    <Text style={{
                      fontSize: 11, color: summary === 'Full admin' ? colors.ACCENT_GREEN : summary === 'No access' ? colors.ACCENT_RED : colors.TEXT_MUTED,
                      fontFamily: 'var(--font-ui)', fontWeight: summary === 'Full admin' ? 600 : 400,
                    }}>
                      {summary}
                    </Text>
                  </div>
                )}
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
            <CredentialsBanner
              accessKey={newCreds.ak}
              secretKey={newCreds.sk}
              message="User created — save these credentials"
              onClose={() => setNewCreds(null)}
            />
          </div>
        )}

        {creating ? (
          <UserForm
            user={null}
            onSaved={handleSaved}
            onCreated={handleCreated}
            onCancel={() => setCreating(false)}
            onSavingChange={onSavingChange}
            onNavigateToGroup={onNavigateToGroup}
          />
        ) : selectedUser ? (
          <UserForm
            key={selectedUser.id}
            user={selectedUser}
            onSaved={handleSaved}
            onDeleted={handleDeleted}
            onSavingChange={onSavingChange}
            onNavigateToGroup={onNavigateToGroup}
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
    </div>
  );
}
