import { useState, useEffect, useCallback } from 'react';
import { Button, Typography, Spin, Alert, Input, Divider, Checkbox } from 'antd';
import { PlusOutlined, SearchOutlined, FolderOutlined, DeleteOutlined } from '@ant-design/icons';
import type { IamGroup, IamUser } from '../adminApi';
import { getGroups, createGroup, updateGroup, deleteGroup, addGroupMember, removeGroupMember, getUsers } from '../adminApi';
import { useCardStyles } from './shared-styles';
import { useColors } from '../ThemeContext';
import PermissionEditor, { permissionsToRows, rowsToPermissions } from './PermissionEditor';
import type { PermissionRow } from './PermissionEditor';

const { Text, Title } = Typography;

function permissionSummary(group: IamGroup): string {
  if (group.permissions.length === 0) return 'No permissions';
  const hasAll = group.permissions.some(p => p.actions.includes('*') && p.resources.includes('*'));
  if (hasAll) return 'Full access';
  return `${group.permissions.length} rule${group.permissions.length !== 1 ? 's' : ''}`;
}

interface GroupsPanelProps {
  onSessionExpired?: () => void;
  onSavingChange?: (saving: boolean) => void;
  initialGroupId?: number | null;
  onGroupSelected?: () => void;
}

export default function GroupsPanel({ onSessionExpired, onSavingChange, initialGroupId, onGroupSelected }: GroupsPanelProps) {
  const colors = useColors();
  const [groups, setGroups] = useState<IamGroup[]>([]);
  const [users, setUsers] = useState<IamUser[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState('');
  const [selectedId, setSelectedId] = useState<number | null>(initialGroupId ?? null);
  const [creating, setCreating] = useState(false);
  const [search, setSearch] = useState('');

  const loadData = useCallback(async () => {
    setLoading(true);
    setError('');
    try {
      const [g, u] = await Promise.all([getGroups(), getUsers()]);
      setGroups(g);
      setUsers(u);
    } catch (e) {
      const msg = e instanceof Error ? e.message : 'Failed to load data';
      if (msg.includes('401')) onSessionExpired?.();
      else setError(msg);
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired]);

  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => { loadData(); }, []);  // Load once on mount; mutations call loadData() explicitly

  // Navigate to a specific group when coming from UserForm
  useEffect(() => {
    if (initialGroupId != null && groups.length > 0) {
      setSelectedId(initialGroupId);
      setCreating(false);
      onGroupSelected?.();
    }
  }, [initialGroupId, groups.length, onGroupSelected]);

  const selectedGroup = groups.find(g => g.id === selectedId) ?? null;
  const filtered = search
    ? groups.filter(g => g.name.toLowerCase().includes(search.toLowerCase()))
    : groups;

  const handleSelect = (group: IamGroup) => {
    setCreating(false);
    setSelectedId(group.id);
  };

  const handleCreate = () => {
    setSelectedId(null);
    setCreating(true);
  };

  const handleSaved = () => { loadData(); };

  const handleDeleted = () => {
    setSelectedId(null);
    setCreating(false);
    loadData();
  };

  return (
    <div style={{ display: 'flex', height: '100%', overflow: 'hidden' }}>
      {/* Left: Group List */}
      <div style={{
        width: 300,
        minWidth: 260,
        borderRight: `1px solid ${colors.BORDER}`,
        display: 'flex',
        flexDirection: 'column',
        overflow: 'hidden',
      }}>
        <div style={{ padding: '16px 16px 12px', borderBottom: `1px solid ${colors.BORDER}` }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 10 }}>
            <Text strong style={{ fontSize: 14 }}>Groups</Text>
            <Button type="primary" size="small" icon={<PlusOutlined />} onClick={handleCreate}>
              New
            </Button>
          </div>
          <Input
            prefix={<SearchOutlined style={{ color: colors.TEXT_MUTED }} />}
            placeholder="Search groups..."
            value={search}
            onChange={e => setSearch(e.target.value)}
            allowClear
            size="small"
            style={{ borderRadius: 6 }}
          />
        </div>

        <div style={{ flex: 1, overflow: 'auto', padding: '4px 0' }}>
          {loading && groups.length === 0 && (
            <div style={{ textAlign: 'center', padding: 32 }}><Spin /></div>
          )}
          {error && (
            <Alert type="error" message={error} showIcon style={{ margin: 8, borderRadius: 8 }} />
          )}
          {!loading && groups.length === 0 && !error && (
            <div style={{ padding: 20, textAlign: 'center' }}>
              <Text type="secondary" style={{ fontSize: 13, display: 'block', marginBottom: 8 }}>No groups yet</Text>
              <Text type="secondary" style={{ fontSize: 11, display: 'block', marginBottom: 12 }}>
                Create groups to share permissions across multiple users.
              </Text>
              <Button type="primary" size="small" icon={<PlusOutlined />} onClick={handleCreate}>
                Create Group
              </Button>
            </div>
          )}
          {filtered.map(group => {
            const isSelected = group.id === selectedId && !creating;
            return (
              <div
                key={group.id}
                onClick={() => handleSelect(group)}
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
                  <FolderOutlined style={{ color: colors.TEXT_MUTED, flexShrink: 0 }} />
                  <Text strong style={{ fontSize: 13, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', flex: 1 }}>
                    {group.name}
                  </Text>
                  <Button
                    type="text"
                    danger
                    size="small"
                    icon={<DeleteOutlined />}
                    onClick={async (e) => {
                      e.stopPropagation();
                      if (!window.confirm(`Delete group "${group.name}"? This cannot be undone.`)) return;
                      try {
                        await deleteGroup(group.id);
                        handleDeleted();
                      } catch (err) {
                        console.error('Delete group failed:', err);
                      }
                    }}
                    style={{ opacity: 0.5, padding: '2px 4px', minWidth: 0 }}
                    onMouseEnter={e => { e.currentTarget.style.opacity = '1'; }}
                    onMouseLeave={e => { e.currentTarget.style.opacity = '0.5'; }}
                  />
                </div>
                <div style={{ marginLeft: 22, marginTop: 2 }}>
                  <Text type="secondary" style={{ fontSize: 11 }}>
                    {group.member_ids.length} member{group.member_ids.length !== 1 ? 's' : ''}
                    {' \u00b7 '}
                    {permissionSummary(group)}
                  </Text>
                </div>
              </div>
            );
          })}
        </div>
      </div>

      {/* Right: Detail Form */}
      <div style={{ flex: 1, overflow: 'auto', background: colors.BG_CARD }}>
        {creating ? (
          <GroupForm
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
                    Create groups to share permissions across multiple users. Users inherit all permissions from their groups.
                  </Text>
                </>
              ) : (
                <Text type="secondary" style={{ fontSize: 14 }}>Select a group to edit, or create a new one</Text>
              )}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

// === Group Edit Form ===

interface GroupFormProps {
  group: IamGroup | null;
  users: IamUser[];
  onSaved: () => void;
  onDeleted?: () => void;
  onCancel?: () => void;
  onSavingChange?: (saving: boolean) => void;
}

function GroupForm({ group, users, onSaved, onDeleted, onCancel, onSavingChange }: GroupFormProps) {
  const isEdit = group !== null;
  const { inputRadius } = useCardStyles();

  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [permissions, setPermissions] = useState<PermissionRow[]>([]);
  const [memberIds, setMemberIds] = useState<Set<number>>(new Set());
  const [saving, setSavingState] = useState(false);
  const [deleting, setDeletingState] = useState(false);
  const [error, setError] = useState('');

  const setSaving = (v: boolean) => { setSavingState(v); onSavingChange?.(v); };
  const setDeleting = (v: boolean) => { setDeletingState(v); onSavingChange?.(v); };

  useEffect(() => {
    if (group) {
      setName(group.name);
      setDescription(group.description);
      setPermissions(permissionsToRows(group.permissions));
      setMemberIds(new Set(group.member_ids));
    } else {
      setName('');
      setDescription('');
      setPermissions([{ effect: 'Allow', actions: [], resources: '' }]);
      setMemberIds(new Set());
    }
    setError('');
  }, [group]);

  const handleSave = async () => {
    if (!name.trim()) { setError('Name is required'); return; }
    setSaving(true);
    setError('');
    try {
      if (isEdit) {
        await updateGroup(group.id, {
          name: name.trim(),
          description: description.trim(),
          permissions: rowsToPermissions(permissions),
        });

        // Sync membership: add/remove as needed
        const currentMembers = new Set(group.member_ids);
        for (const uid of memberIds) {
          if (!currentMembers.has(uid)) {
            await addGroupMember(group.id, uid);
          }
        }
        for (const uid of currentMembers) {
          if (!memberIds.has(uid)) {
            await removeGroupMember(group.id, uid);
          }
        }
      } else {
        const created = await createGroup({
          name: name.trim(),
          description: description.trim(),
          permissions: rowsToPermissions(permissions),
        });
        // Add members to newly created group
        for (const uid of memberIds) {
          await addGroupMember(created.id, uid);
        }
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
      await deleteGroup(group.id);
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

  const label = (text: string, hint?: string) => (
    <div style={{ marginBottom: 4 }}>
      <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5, fontWeight: 600 }}>{text}</Text>
      {hint && <Text type="secondary" style={{ fontSize: 10, fontWeight: 400, marginLeft: 6 }}>{hint}</Text>}
    </div>
  );

  return (
    <div style={{ padding: '24px 28px', maxWidth: 600, overflow: 'auto', height: '100%' }}>
      <Title level={5} style={{ margin: '0 0 20px', fontFamily: 'var(--font-ui)' }}>
        {isEdit ? `Edit: ${group?.name}` : 'Create New Group'}
      </Title>

      {error && <Alert type="error" message={error} showIcon closable onClose={() => setError('')} style={{ marginBottom: 16, borderRadius: 8 }} />}

      <div style={{ marginBottom: 16 }}>
        {label('Name')}
        <Input value={name} onChange={e => setName(e.target.value)} placeholder="e.g. developers" style={{ ...inputRadius }} />
      </div>

      <div style={{ marginBottom: 16 }}>
        {label('Description')}
        <Input value={description} onChange={e => setDescription(e.target.value)} placeholder="e.g. Development team access" style={{ ...inputRadius }} />
      </div>

      <Divider style={{ margin: '16px 0 12px' }}>Permissions</Divider>

      <PermissionEditor permissions={permissions} onChange={setPermissions} />

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
                cursor: 'pointer',
              }}
              onClick={() => toggleMember(user.id)}
            >
              <Checkbox checked={memberIds.has(user.id)} />
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
        <Button type="primary" onClick={handleSave} loading={saving}>{isEdit ? 'Save' : 'Create Group'}</Button>
      </div>
    </div>
  );
}
