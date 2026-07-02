import { useState, useEffect, useMemo } from 'react';
import { Button, Typography, Alert, Input, Divider, Checkbox, message } from 'antd';
import { PlusOutlined, FolderOutlined, DeleteOutlined, CopyOutlined } from '@ant-design/icons';
import type { IamGroup, IamUser } from '../adminApi';
import { useAdminConfig } from '../queries/config';
import { useGroups, useCreateGroup, useUpdateGroup, useDeleteGroup, useCloneGroup, useAddGroupMember, useRemoveGroupMember } from '../queries/groups';
import { useUsers } from '../queries/users';
import { useCardStyles } from './shared-styles';
import FormLabel from './FormLabel';
import { useColors } from '../ThemeContext';
import PermissionEditor from './PermissionEditor';
import { permissionsToRows, rowsToPermissions, freshPermissionRowId, type PermissionRow } from './permissionRows';
import { groupPermissionSummary, filterItems } from '../masterDetailFilter';
import MasterDetailPanel from './MasterDetailPanel';
import IamSourceBanner from './IamSourceBanner';

const { Text, Title } = Typography;

interface GroupsPanelProps {
  onSessionExpired?: () => void;
  onSavingChange?: (saving: boolean) => void;
  initialGroupId?: number | null;
  onGroupSelected?: () => void;
}

export default function GroupsPanel({ onSessionExpired, onSavingChange, initialGroupId, onGroupSelected }: GroupsPanelProps) {
  const colors = useColors();
  const [selectedId, setSelectedId] = useState<number | null>(initialGroupId ?? null);
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState('');

  // IAM mode for the source-of-truth banner (cached react-query read).
  const { data: cfg } = useAdminConfig();
  const iamMode = cfg?.iam_mode;
  // Declarative IAM: YAML owns group state; admin-API mutations 403. Render
  // read-only (no New / clone / delete / save) — same as UsersPanel.
  const readOnly = iamMode === 'declarative';

  // Groups + users are read from the shared query cache; the form's mutations
  // (create/update/delete/clone + membership add/remove) invalidate these keys
  // on success, so there's no manual loadData()-after-every-mutation reload.
  const groupsQuery = useGroups();
  const usersQuery = useUsers();
  // Memoize so the `?? []` fallback doesn't hand a fresh array identity to the
  // selection effect's dep list on every render (which would re-run it needlessly).
  const groups = useMemo(() => groupsQuery.data ?? [], [groupsQuery.data]);
  const users = useMemo(() => usersQuery.data ?? [], [usersQuery.data]);
  const loading = groupsQuery.isLoading || usersQuery.isLoading;
  const rawError = groupsQuery.error ?? usersQuery.error;
  const error = rawError ? (rawError instanceof Error ? rawError.message : 'Failed to load data') : '';

  // Bubble a 401 up so the login screen can take over. Must run as an effect,
  // not in the render body: react-query keeps `error` populated across renders,
  // so a render-phase navigate would fire a setState during render.
  useEffect(() => {
    if (rawError instanceof Error && rawError.message.includes('401')) {
      onSessionExpired?.();
    }
  }, [rawError, onSessionExpired]);

  // Navigate to a specific group when coming from UserForm. Only act once the
  // requested group is actually present in the loaded list — checking
  // groups.length alone would fire before the target row exists (and could
  // select a stale id). find() uses strict === so prop-reference churn alone
  // can't re-trigger a no-op selection.
  useEffect(() => {
    if (initialGroupId == null) return;
    const group = groups.find(g => g.id === initialGroupId);
    if (!group) return;
    setSelectedId(group.id);
    setCreating(false);
    onGroupSelected?.();
  }, [initialGroupId, groups, onGroupSelected]);

  // Mutations used directly by the panel (the form gets its own). Each
  // invalidates qk.groups.list() (+ users) so the list refreshes automatically.
  const cloneMutation = useCloneGroup();
  const deleteMutation = useDeleteGroup();

  const selectedGroup = groups.find(g => g.id === selectedId) ?? null;
  const filtered = filterItems(groups, search, g => [g.name]);

  const handleSelect = (group: IamGroup) => {
    setCreating(false);
    setSelectedId(group.id);
  };

  const handleCreate = () => {
    setSelectedId(null);
    setCreating(true);
  };

  /**
   * Wave 11 post-manual-review fix (UX-1):
   * After a successful create, flip out of "creating" mode so the form
   * doesn't keep its stale fields visible. If the caller supplies the
   * new group's id, select it so the operator immediately lands on the
   * Edit view for the thing they just made. That pattern matches the
   * UsersPanel post-create flow.
   */
  // Mutations invalidate qk.groups.list() on success, so the list refetches
  // automatically — these handlers only manage local selection state.
  const handleSaved = (createdId?: number) => {
    if (creating) {
      setCreating(false);
      if (createdId !== undefined) setSelectedId(createdId);
    }
  };

  const handleDeleted = () => {
    setSelectedId(null);
    setCreating(false);
  };

  const handleClone = async (group: IamGroup) => {
    const copyMembers = group.member_ids.length > 0
      ? window.confirm(`Copy ${group.member_ids.length} member${group.member_ids.length !== 1 ? 's' : ''} into the duplicated group?`)
      : false;
    onSavingChange?.(true);
    try {
      const cloned = await cloneMutation.mutateAsync({ id: group.id, copyMembers });
      setCreating(false);
      setSelectedId(cloned.id);
    } catch (err) {
      message.error(err instanceof Error ? err.message : 'Duplicate group failed');
    } finally {
      onSavingChange?.(false);
    }
  };

  const detail = creating ? (
    <GroupForm
      key="new"
      group={null}
      users={users}
      onSaved={handleSaved}
      onCancel={() => setCreating(false)}
      onSavingChange={onSavingChange}
    />
  ) : selectedGroup ? (
    <GroupForm
      key={selectedGroup.id}
      group={selectedGroup}
      users={users}
      readOnly={readOnly}
      onSaved={handleSaved}
      onDeleted={handleDeleted}
      onSavingChange={onSavingChange}
    />
  ) : (
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'center', height: '100%', color: colors.TEXT_MUTED }}>
      <div style={{ textAlign: 'center', maxWidth: 360, padding: 24 }}>
        {groups.length === 0 ? (
          <>
            <FolderOutlined style={{ fontSize: 40, marginBottom: 12, color: colors.TEXT_MUTED }} />
            <div><Text type="secondary" style={{ fontSize: 15, fontWeight: 500 }}>Permission Groups</Text></div>
            <Text type="secondary" style={{ fontSize: 12, display: 'block', marginTop: 8 }}>
              {readOnly
                ? 'No groups in your YAML config. Add them under access.iam_groups and apply.'
                : 'Create groups to share permissions across multiple users. Users inherit all permissions from their groups.'}
            </Text>
          </>
        ) : (
          <Text type="secondary" style={{ fontSize: 14 }}>
            {readOnly ? 'Select a group to view' : 'Select a group to edit, or create a new one'}
          </Text>
        )}
      </div>
    </div>
  );

  return (
    <MasterDetailPanel<IamGroup>
      // IAM source-of-truth banner — same explainer as UsersPanel.
      banner={<IamSourceBanner iamMode={iamMode} resource="groups" />}
      title="Groups"
      searchPlaceholder="Search groups..."
      items={filtered}
      getId={group => group.id}
      isSelected={group => group.id === selectedId && !creating}
      onSelect={handleSelect}
      rowPadding="10px 16px"
      onCreate={handleCreate}
      readOnly={readOnly}
      search={search}
      onSearchChange={setSearch}
      loading={loading}
      error={error}
      listEmptyState={(
        <div style={{ padding: 20, textAlign: 'center' }}>
          <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 8 }}>No groups yet</Text>
          {readOnly ? (
            <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>
              Add groups under access.iam_groups in your YAML config and apply.
            </Text>
          ) : (
            <>
              <Text type="secondary" style={{ fontSize: 11, display: 'block', marginBottom: 12 }}>
                Create groups to share permissions across multiple users.
              </Text>
              <Button type="primary" size="small" icon={<PlusOutlined />} onClick={handleCreate}>
                Create Group
              </Button>
            </>
          )}
        </div>
      )}
      renderRowBody={group => (
        <>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <FolderOutlined style={{ color: colors.TEXT_MUTED, flexShrink: 0 }} />
            <Text strong style={{ fontSize: 13, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>
              {group.name}
            </Text>
            {!readOnly && (
              <>
                <Button
                  type="text"
                  size="small"
                  icon={<CopyOutlined />}
                  title="Duplicate group"
                  onClick={(e) => {
                    e.stopPropagation();
                    void handleClone(group);
                  }}
                  style={{ opacity: 0.5, padding: '2px 4px', minWidth: 0 }}
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
                    if (!window.confirm(`Delete group "${group.name}"? This cannot be undone.`)) return;
                    deleteMutation.mutate(group.id, {
                      onSuccess: handleDeleted,
                      onError: (err) => console.error('Delete group failed:', err),
                    });
                  }}
                  style={{ opacity: 0.5, padding: '2px 4px', minWidth: 0 }}
                  onMouseEnter={e => { e.currentTarget.style.opacity = '1'; }}
                  onMouseLeave={e => { e.currentTarget.style.opacity = '0.5'; }}
                />
              </>
            )}
          </div>
          <div style={{ marginLeft: 22, marginTop: 2 }}>
            <Text type="secondary" style={{ fontSize: 11 }}>
              {group.member_ids.length} member{group.member_ids.length !== 1 ? 's' : ''}
              {' · '}
              {groupPermissionSummary(group)}
            </Text>
          </div>
        </>
      )}
      detail={detail}
    />
  );
}

// === Group Edit Form ===

interface GroupFormProps {
  group: IamGroup | null;
  users: IamUser[];
  /** Declarative IAM: disable all inputs, hide the Save/Delete row. */
  readOnly?: boolean;
  /** Invoked on successful save. The `createdId` is supplied only on
   *  create (not edit) so the parent can select the new row. */
  onSaved: (createdId?: number) => void;
  onDeleted?: () => void;
  onCancel?: () => void;
  onSavingChange?: (saving: boolean) => void;
}

function GroupForm({ group, users, readOnly = false, onSaved, onDeleted, onCancel, onSavingChange }: GroupFormProps) {
  const isEdit = group !== null;
  const { inputRadius } = useCardStyles();

  // Mutations close the cache loop: each invalidates qk.groups.list() (+ users)
  // on success, so the panel list refreshes without a manual reload callback.
  const createGroupMutation = useCreateGroup();
  const updateGroupMutation = useUpdateGroup();
  const deleteGroupMutation = useDeleteGroup();
  const addMemberMutation = useAddGroupMember();
  const removeMemberMutation = useRemoveGroupMember();

  // Initialize from `group` once. The edit form is remounted with
  // `key={selectedGroup.id}` (see render site), and the create form mounts
  // fresh — so a keyed remount resets all state from these initializers. No
  // prop→state sync effect needed (it was a redundant mirror of the prop).
  const [name, setName] = useState(() => group?.name ?? '');
  const [description, setDescription] = useState(() => group?.description ?? '');
  const [permissions, setPermissions] = useState<PermissionRow[]>(() =>
    // Seed the initial empty row WITH a `_uiId` so PermissionEditor receives an
    // id-bearing row and its adoption effect no-ops (match the fresh-row shape
    // its Add button emits).
    group ? permissionsToRows(group.permissions) : [{ _uiId: freshPermissionRowId(), effect: 'Allow', actions: [], resources: [] }],
  );
  const [memberIds, setMemberIds] = useState<Set<number>>(
    () => new Set(group?.member_ids ?? []),
  );
  const [saving, setSavingState] = useState(false);
  const [deleting, setDeletingState] = useState(false);
  const [error, setError] = useState('');

  const setSaving = (v: boolean) => { setSavingState(v); onSavingChange?.(v); };
  const setDeleting = (v: boolean) => { setDeletingState(v); onSavingChange?.(v); };

  const handleSave = async () => {
    if (!name.trim()) { setError('Name is required'); return; }
    setSaving(true);
    setError('');
    try {
      if (isEdit) {
        await updateGroupMutation.mutateAsync({
          id: group.id,
          patch: {
            name: name.trim(),
            description: description.trim(),
            permissions: rowsToPermissions(permissions),
          },
        });

        // Sync membership: add/remove as needed
        const currentMembers = new Set(group.member_ids);
        for (const uid of memberIds) {
          if (!currentMembers.has(uid)) {
            await addMemberMutation.mutateAsync({ groupId: group.id, userId: uid });
          }
        }
        for (const uid of currentMembers) {
          if (!memberIds.has(uid)) {
            await removeMemberMutation.mutateAsync({ groupId: group.id, userId: uid });
          }
        }
      } else {
        const created = await createGroupMutation.mutateAsync({
          name: name.trim(),
          description: description.trim(),
          permissions: rowsToPermissions(permissions),
        });
        // Add members to newly created group
        for (const uid of memberIds) {
          await addMemberMutation.mutateAsync({ groupId: created.id, userId: uid });
        }
        onSaved(created.id);
        return;
      }
      onSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Operation failed');
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (!group || deleting) return;
    setDeleting(true);
    try {
      await deleteGroupMutation.mutateAsync(group.id);
      onDeleted?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Delete failed');
    } finally {
      setDeleting(false);
    }
  };

  const toggleMember = (userId: number) => {
    setMemberIds(prev => {
      const next = new Set(prev);
      if (next.has(userId)) next.delete(userId);
      else next.add(userId);
      return next;
    });
  };

  return (
    <div style={{ padding: '24px 28px', maxWidth: 600, overflow: 'auto', height: '100%' }}>
      <Title level={5} style={{ margin: '0 0 20px', fontFamily: 'var(--font-ui)' }}>
        {isEdit ? `${readOnly ? 'View' : 'Edit'}: ${group?.name}` : 'Create New Group'}
      </Title>

      {error && <Alert type="error" message={error} showIcon closable onClose={() => setError('')} style={{ marginBottom: 16, borderRadius: 8 }} />}

      <div style={{ marginBottom: 16 }}>
        <FormLabel text="Name" />
        <Input data-testid="group-name" aria-label="Group name" value={name} onChange={e => setName(e.target.value)} placeholder="e.g. developers" disabled={readOnly} style={{ ...inputRadius }} />
      </div>

      <div style={{ marginBottom: 16 }}>
        <FormLabel text="Description" />
        <Input value={description} onChange={e => setDescription(e.target.value)} placeholder="e.g. Development team access" disabled={readOnly} style={{ ...inputRadius }} />
      </div>

      <Divider style={{ margin: '16px 0 12px' }}>Permissions</Divider>

      {/* See UserForm: fieldset+pointer-events make the deep editor read-only
          without threading `disabled` through every inner control. */}
      <fieldset
        disabled={readOnly}
        style={{
          border: 'none',
          margin: 0,
          padding: 0,
          minInlineSize: 'auto',
          ...(readOnly ? { pointerEvents: 'none' as const, opacity: 0.85 } : {}),
        }}
      >
        <PermissionEditor permissions={permissions} onChange={setPermissions} />
      </fieldset>

      <Divider style={{ margin: '16px 0 12px' }}>Members</Divider>

      {users.length === 0 ? (
        <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 16 }}>
          No IAM users exist yet. Create users first, then add them to this group.
        </Text>
      ) : (
        <div style={{ marginBottom: 24 }}>
          {users.map(user => (
            <div
              key={user.id}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 10,
                padding: '6px 4px',
                borderRadius: 6,
                cursor: readOnly ? 'default' : 'pointer',
              }}
              onClick={() => { if (!readOnly) toggleMember(user.id); }}
            >
              <Checkbox checked={memberIds.has(user.id)} disabled={readOnly} />
              <div style={{ flex: 1 }}>
                <Text style={{ fontSize: 13 }}>{user.name}</Text>
                <Text type="secondary" style={{ fontSize: 11, marginLeft: 8, fontFamily: 'var(--font-mono)' }}>
                  {user.access_key_id}
                </Text>
              </div>
              {!user.enabled && (
                <Text type="secondary" style={{ fontSize: 10 }}>disabled</Text>
              )}
            </div>
          ))}
        </div>
      )}

      {!readOnly && (
        <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
          <div>
            {isEdit && (
              <Button danger loading={deleting} disabled={deleting} onClick={async () => {
                if (!window.confirm(`Delete group "${group?.name}"? This cannot be undone.`)) return;
                await handleDelete();
              }}>Delete Group</Button>
            )}
            {!isEdit && onCancel && <Button onClick={onCancel}>Cancel</Button>}
          </div>
          <Button data-testid="group-save" aria-label={isEdit ? 'Save group' : 'Create group'} type="primary" onClick={handleSave} loading={saving}>{isEdit ? 'Save' : 'Create Group'}</Button>
        </div>
      )}
    </div>
  );
}
