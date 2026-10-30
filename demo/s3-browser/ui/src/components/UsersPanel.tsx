import { useEffect, useMemo, useState, useCallback } from 'react';
import { Button, Typography, message } from 'antd';
import { UserOutlined, CopyOutlined } from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import { getUsers } from '../adminApi';
import type { IamUser } from '../adminApi';
import { useColors } from '../ThemeContext';
import { useUsers, useCloneUser } from '../queries/users';
import { useAdminConfig, useIamMode } from '../queries/config';
import { qk } from '../queries/keys';
import { userPermissionSummary, filterItems } from '../masterDetailFilter';
import MasterDetailPanel from './MasterDetailPanel';
import UserForm from './UserForm';
import CredentialsModal from './CredentialsModal';
import IamSourceBanner from './IamSourceBanner';
import { EmptyState } from './StatePlaceholders';
import { normalizeUiError } from '../errorHandling';
import { useNavigation } from '../NavigationContext';
import { buildViewUrl, parseAdminQuery } from '../urlState';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';
import { IAM_DIRTY_KEYS, confirmDiscardEdits } from '../useDirtyFlag';

const { Text } = Typography;

interface UsersPanelProps {
  onSessionExpired?: () => void;
  onSavingChange?: (saving: boolean) => void;
  onNavigateToGroup?: (groupId: number) => void;
  /** Raw query string (with leading `?`) for the ?user=<id> deep-link. */
  search?: string;
}

/** The credentials dialog of the first IAM user (the switch is explained before the create). */
const FIRST_USER_CREATED_NOTE = 'You now sign in with an access key.';

export default function UsersPanel({ onSessionExpired, onSavingChange, onNavigateToGroup, search }: UsersPanelProps) {
  const colors = useColors();
  const qc = useQueryClient();
  const { navigate } = useNavigation();
  // URL deep-link: ?user=<id> pre-selects a user on direct load and is kept
  // in sync on every selection change (replace, not push, to avoid history
  // spam). The local filter textbox is a separate piece of state.
  const queryParams = useMemo(() => parseAdminQuery(search ?? ''), [search]);
  const urlUserId = queryParams.user && !Number.isNaN(Number(queryParams.user)) ? Number(queryParams.user) : null;
  const [selectedId, setSelectedId] = useState<number | null>(urlUserId);
  const [creating, setCreating] = useState(false);
  const [filterText, setFilterText] = useState('');
  // `firstSwitch`: this create was the first IAM user, so the dialog also
  // says in one line that sign-in changed.
  const [newCreds, setNewCreds] = useState<{ ak: string; sk: string; firstSwitch?: boolean } | null>(null);

  // IAM mode banner: tells the operator where user state lives (DB in
  // `gui` mode vs YAML in `declarative`). Snapshot once via TanStack
  // Query — refetch is automatic if the mode flips elsewhere in the app.
  const { iamMode, readOnly, loadError: iamLoadError } = useIamMode(onSessionExpired);
  // Declarative IAM: YAML is the source of truth and every admin-API IAM
  // mutation returns 403. Render the whole panel read-only so operators browse
  // the reconciled state instead of filling out forms that 403 on save.
  // An unknown mode (config still loading, or failed) is read-only too.

  // Users list. Query handles loading/error/refetch automatically;
  // mutations on this resource invalidate this key (see queries/users.ts)
  // so we never have to manually call a refresh callback.
  const usersQuery = useUsers();
  const users = usersQuery.data ?? [];
  const loading = usersQuery.isLoading;
  const rawError = usersQuery.error;
  const error = rawError ? (normalizeUiError(rawError, "Failed to load users")) : '';
  // The first create switches the proxy to IAM mode (UserForm confirms it).
  // Only when the list is known to be empty: a failed load says nothing.
  const isFirstUser = !loading && !rawError && usersQuery.data !== undefined && users.length === 0;
  const bootstrapAccessKey = useAdminConfig({ enabled: isFirstUser }).data?.access_key_id ?? undefined;

  // Bubble up 401 to the parent so the login screen can take over. Effect, not
  // render-body: react-query keeps `error` populated across renders, so calling
  // it in render would fire a setState (navigation) during render.
  useSessionExpiredOn(rawError, onSessionExpired);

  // URL → state: when the query string changes (direct load, Back/Forward),
  // re-select that user. Only acts when a ?user= param is present so local
  // state (e.g. the unsaved "creating" form) isn't clobbered by a param-less
  // URL. It must NOT clear `newCreds`: create and clone write ?user=<new id>,
  // and the secret they return is shown only once.
  useEffect(() => {
    if (urlUserId == null) return;
    setSelectedId(urlUserId);
    setCreating(false);
  }, [urlUserId]);

  const cloneMutation = useCloneUser();

  const selectedUser = users.find(u => u.id === selectedId) ?? null;
  const filtered = filterItems(users, filterText, u => [u.name, u.access_key_id]);

  // Write ?user=<id> (or strip it) into the URL bar on every selection change.
  // `replace: true` swaps the current entry instead of pushing, so row clicks
  // don't spam browser history — Back still leaves the admin section entirely.
  const writeSelectionUrl = useCallback(
    (id: number | null) => {
      const query = id != null ? { user: String(id) } : undefined;
      navigate(buildViewUrl('admin', 'access/users', query), { replace: true });
    },
    [navigate],
  );

  const handleSelect = async (user: IamUser) => {
    if (user.id === selectedId && !creating) return;
    if (!(await confirmDiscardEdits(IAM_DIRTY_KEYS.users))) return;
    setCreating(false);
    setSelectedId(user.id);
    setNewCreds(null);
    writeSelectionUrl(user.id);
  };

  const handleCreate = async () => {
    if (!(await confirmDiscardEdits(IAM_DIRTY_KEYS.users))) return;
    setSelectedId(null);
    setCreating(true);
    setNewCreds(null);
    writeSelectionUrl(null);
  };

  const handleSaved = () => {
    qc.invalidateQueries({ queryKey: qk.users.list() });
  };

  const handleCreated = async (ak: string, sk: string) => {
    const firstSwitch = isFirstUser;
    // Refetch synchronously to capture the new user's ID, then select it.
    const result = await qc.fetchQuery({ queryKey: qk.users.list(), queryFn: getUsers });
    const newUser = result.find(u => u.access_key_id === ak);
    if (newUser) {
      setSelectedId(newUser.id);
      writeSelectionUrl(newUser.id);
    }
    setCreating(false);
    setNewCreds({ ak, sk, firstSwitch });
  };

  const handleClone = async (user: IamUser) => {
    // The clone becomes the selection, which unmounts the open form.
    if (!(await confirmDiscardEdits(IAM_DIRTY_KEYS.users))) return;
    onSavingChange?.(true);
    setNewCreds(null);
    try {
      // Clone via the mutation hook — it invalidates qk.users.list() on success.
      const cloned = await cloneMutation.mutateAsync({ id: user.id, copyGroupMemberships: true });
      setCreating(false);
      setSelectedId(cloned.id);
      writeSelectionUrl(cloned.id);
      setNewCreds({ ak: cloned.access_key_id, sk: cloned.secret_access_key ?? '' });
    } catch (err) {
      message.error(normalizeUiError(err, "Duplicate user failed"));
    } finally {
      onSavingChange?.(false);
    }
  };

  const handleDeleted = () => {
    setSelectedId(null);
    setCreating(false);
    setNewCreds(null);
    writeSelectionUrl(null);
    qc.invalidateQueries({ queryKey: qk.users.list() });
  };

  const detail = (
    <>
      {newCreds && (
        <CredentialsModal
          accessKey={newCreds.ak}
          secretKey={newCreds.sk}
          title="User created: save these credentials"
          // The create confirm explained the switch a moment ago: one line here.
          note={newCreds.firstSwitch ? FIRST_USER_CREATED_NOTE : undefined}
          onClose={() => setNewCreds(null)}
        />
      )}

      {creating ? (
        <UserForm
          key="new"
          user={null}
          onSaved={handleSaved}
          onCreated={handleCreated}
          onCancel={() => { void confirmDiscardEdits(IAM_DIRTY_KEYS.users).then((ok) => { if (ok) setCreating(false); }); }}
          onSavingChange={onSavingChange}
          onNavigateToGroup={onNavigateToGroup}
          firstUser={isFirstUser ? { bootstrapAccessKey } : undefined}
        />
      ) : selectedUser ? (
        <UserForm
          key={selectedUser.id}
          user={selectedUser}
          readOnly={readOnly}
          onSaved={handleSaved}
          onDeleted={handleDeleted}
          onSavingChange={onSavingChange}
          onNavigateToGroup={onNavigateToGroup}
        />
      ) : (
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: '100%' }}>
          {users.length === 0 ? (
            <EmptyState
              icon={<UserOutlined />}
              title={readOnly ? 'No users declared' : 'Create the first user'}
              hint={readOnly
                ? 'Your YAML config has no users. Add them under access.iam_users and apply.'
                : 'Click New to create a user with its own credentials and permissions. The first user switches sign-in from the admin password to IAM users.'}
            />
          ) : (
            <Text type="secondary" style={{ fontSize: 14 }}>
              {readOnly ? 'Select a user to view' : 'Select a user to edit, or create a new one'}
            </Text>
          )}
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
      banner={<IamSourceBanner iamMode={iamMode} loadError={iamLoadError} resource="users" />}
      title="Users"
      searchPlaceholder="Search users..."
      items={filtered}
      getId={user => user.id}
      isSelected={user => user.id === selectedId && !creating}
      onSelect={handleSelect}
      rowPadding="12px 16px"
      rowClassName="user-list-item"
      onCreate={handleCreate}
      readOnly={readOnly}
      search={filterText}
      onSearchChange={setFilterText}
      loading={loading}
      error={error}
      // ONE empty state: with no users the detail pane explains, so the list
      // stays blank. It speaks only when the search hides every user.
      listEmptyState={users.length > 0 ? (
        <div style={{ padding: 20, textAlign: 'center' }}>
          <Text type="secondary" style={{ fontSize: 13 }}>No users match the search</Text>
        </div>
      ) : undefined}
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
              <Text strong title={user.name} style={{ fontSize: 14, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1, fontFamily: 'var(--font-ui)' }}>
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
      renderRowActions={readOnly ? undefined : user => (
        <Button
          type="text"
          size="small"
          icon={<CopyOutlined />}
          title="Duplicate user with fresh credentials"
          aria-label={`Duplicate ${user.name} with fresh credentials`}
          onClick={() => void handleClone(user)}
          style={{ opacity: 0.5, padding: '2px 4px', minWidth: 0, flexShrink: 0 }}
          onMouseEnter={e => { e.currentTarget.style.opacity = '1'; }}
          onMouseLeave={e => { e.currentTarget.style.opacity = '0.5'; }}
        />
      )}
      detail={detail}
    />
  );
}
