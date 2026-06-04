import { useEffect, useState } from 'react';
import { Button, Typography } from 'antd';
import { PlusOutlined, TeamOutlined, DeleteOutlined, CopyOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import { getUsers } from '../adminApi';
import type { IamUser } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useUsers, useDeleteUser, useCloneUser } from '../queries/users';
import { useAdminConfig } from '../queries/config';
import { qk } from '../queries/keys';
import { userPermissionSummary, filterItems } from '../masterDetailFilter';
import MasterDetailPanel from './MasterDetailPanel';
import UserForm from './UserForm';
import CredentialsBanner from './CredentialsBanner';
import IamSourceBanner from './IamSourceBanner';

const { Text } = Typography;

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

  // Bubble up 401 to the parent so the login screen can take over. Effect, not
  // render-body: react-query keeps `error` populated across renders, so calling
  // it in render would fire a setState (navigation) during render.
  useEffect(() => {
    if (rawError instanceof Error && rawError.message.includes('401')) {
      onSessionExpired?.();
    }
  }, [rawError, onSessionExpired]);

  const deleteMutation = useDeleteUser();
  const cloneMutation = useCloneUser();

  const selectedUser = users.find(u => u.id === selectedId) ?? null;
  const filtered = filterItems(users, search, u => [u.name, u.access_key_id]);

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
    const result = await qc.fetchQuery({ queryKey: qk.users.list(), queryFn: getUsers });
    const newUser = result.find(u => u.access_key_id === ak);
    if (newUser) setSelectedId(newUser.id);
    setCreating(false);
    setNewCreds({ ak, sk });
  };

  const handleClone = async (user: IamUser) => {
    onSavingChange?.(true);
    setNewCreds(null);
    try {
      // Clone via the mutation hook — it invalidates qk.users.list() on success.
      const cloned = await cloneMutation.mutateAsync({ id: user.id, copyGroupMemberships: true });
      setCreating(false);
      setSelectedId(cloned.id);
      setNewCreds({ ak: cloned.access_key_id, sk: cloned.secret_access_key ?? '' });
    } catch (err) {
      console.error('Duplicate user failed:', err);
    } finally {
      onSavingChange?.(false);
    }
  };

  const handleDeleted = () => {
    setSelectedId(null);
    setCreating(false);
    setNewCreds(null);
    qc.invalidateQueries({ queryKey: qk.users.list() });
  };

  const detail = (
    <>
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
          key="new"
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
    </>
  );

  return (
    <MasterDetailPanel<IamUser>
      // "Where does this data live?" banner — IAM state is DB-backed in GUI
      // mode, YAML-authoritative in Declarative mode. Shows on every IAM panel
      // so operators never wonder why Copy YAML on Access shows `access: {}`
      // after adding a user.
      banner={<IamSourceBanner iamMode={iamMode} resource="users" />}
      title="Users"
      searchPlaceholder="Search users..."
      items={filtered}
      getId={user => user.id}
      isSelected={user => user.id === selectedId && !creating}
      onSelect={handleSelect}
      rowPadding="12px 16px"
      rowClassName="user-list-item"
      onCreate={handleCreate}
      search={search}
      onSearchChange={setSearch}
      loading={loading}
      error={error}
      listEmptyState={(
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
      renderRowBody={user => {
        const isExternal = user.auth_source === 'external';
        const summary = userPermissionSummary(user);
        return (
          <>
            {/* Row 1: status dot + name + badges + actions */}
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
                size="small"
                icon={<CopyOutlined />}
                title="Duplicate user with fresh credentials"
                onClick={(e) => {
                  e.stopPropagation();
                  void handleClone(user);
                }}
                style={{ opacity: 0.5, padding: '2px 4px', minWidth: 0, flexShrink: 0 }}
                onMouseEnter={e => { e.currentTarget.style.opacity = '1'; }}
                onMouseLeave={e => { e.currentTarget.style.opacity = '0.5'; }}
              />
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
          </>
        );
      }}
      detail={detail}
    />
  );
}
