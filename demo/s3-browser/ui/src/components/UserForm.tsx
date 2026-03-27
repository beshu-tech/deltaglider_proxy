import { useState, useEffect } from 'react';
import { Input, Switch, Button, Alert, Space, Divider, Typography, Tag } from 'antd';
import { ThunderboltOutlined, CheckCircleFilled, MinusCircleFilled, CrownFilled } from '@ant-design/icons';
import type { IamUser, IamGroup, CreateUserRequest, UpdateUserRequest, CannedPolicy } from '../adminApi';
import { createUser, updateUser, deleteUser, rotateUserKeys, getCannedPolicies, getGroups } from '../adminApi';
import { setCredentials } from '../s3client';
import { useCardStyles } from './shared-styles';
import { useColors } from '../ThemeContext';
import PermissionEditor, { permissionsToRows, rowsToPermissions } from './PermissionEditor';
import type { PermissionRow } from './PermissionEditor';

const { Text, Title } = Typography;

// Fallback presets used if the API is unavailable
const FALLBACK_PRESETS: Record<string, PermissionRow[]> = {
  'Full Admin': [{ effect: 'Allow', actions: ['*'], resources: '*' }],
  'Read/Write': [{ effect: 'Allow', actions: ['read', 'write', 'list'], resources: '*' }],
  'Read Only': [{ effect: 'Allow', actions: ['read', 'list'], resources: '*' }],
};

function generateId(): string {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
  return 'AK' + Array.from({ length: 18 }, () => chars[Math.floor(Math.random() * chars.length)]).join('');
}

function generateSecret(): string {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  return Array.from({ length: 40 }, () => chars[Math.floor(Math.random() * chars.length)]).join('');
}

interface UserFormProps {
  user: IamUser | null; // null = create mode
  onSaved: () => void;
  onDeleted?: () => void;
  onCancel?: () => void;
  /** Called after a successful create with the new user's credentials */
  onCreated?: (ak: string, sk: string) => void;
  /** Notify parent when save/delete is in progress (prevents Escape close) */
  onSavingChange?: (saving: boolean) => void;
  /** Navigate to a group by ID (switches to Groups tab) */
  onNavigateToGroup?: (groupId: number) => void;
}

export default function UserForm({ user, onSaved, onDeleted, onCancel, onCreated, onSavingChange, onNavigateToGroup }: UserFormProps) {
  const isEdit = user !== null;
  const { inputRadius } = useCardStyles();
  const colors = useColors();

  const [name, setName] = useState('');
  const [accessKeyId, setAccessKeyId] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [enabled, setEnabled] = useState(true);
  const [permissions, setPermissions] = useState<PermissionRow[]>([]);
  const [saving, setSavingState] = useState(false);
  const [deleting, setDeletingState] = useState(false);
  const [error, setError] = useState('');
  const [savedCredentials, setSavedCredentials] = useState<{ ak: string; sk: string } | null>(null);
  const [cannedPolicies, setCannedPolicies] = useState<CannedPolicy[]>([]);
  const [userGroups, setUserGroups] = useState<IamGroup[]>([]);

  const setSaving = (v: boolean) => { setSavingState(v); onSavingChange?.(v); };
  const setDeleting = (v: boolean) => { setDeletingState(v); onSavingChange?.(v); };

  useEffect(() => {
    getCannedPolicies().then(policies => {
      if (policies.length > 0) setCannedPolicies(policies);
    });
    getGroups().then(groups => setUserGroups(groups)).catch(() => {});
  }, []);

  useEffect(() => {
    if (user) {
      setName(user.name);
      setAccessKeyId(user.access_key_id);
      setSecretKey('');
      setEnabled(user.enabled);
      setPermissions(permissionsToRows(user.permissions));
    } else {
      setName('');
      setAccessKeyId('');
      setSecretKey('');
      setEnabled(true);
      setPermissions([{ effect: 'Allow', actions: ['*'], resources: '*' }]);
    }
    setError('');
    setSavedCredentials(null);
  }, [user]);

  const handleSave = async () => {
    if (!name.trim()) { setError('Name is required'); return; }
    setSaving(true);
    setError('');
    setSavedCredentials(null);
    try {
      if (isEdit) {
        const req: UpdateUserRequest = {
          name: name.trim(),
          enabled,
          permissions: rowsToPermissions(permissions),
        };
        await updateUser(user.id, req);

        const akChanged = accessKeyId.trim() && accessKeyId.trim() !== user.access_key_id;
        const skChanged = secretKey.trim().length > 0;
        if (akChanged || skChanged) {
          const rotated = await rotateUserKeys(
            user.id,
            accessKeyId.trim() || user.access_key_id,
            skChanged ? secretKey.trim() : undefined,
          );
          const browserAk = localStorage.getItem('dg-access-key-id');
          // Compare against both old AK and new AK to avoid self-lockout when user changes their own AK
          if ((browserAk === user.access_key_id || browserAk === accessKeyId.trim()) && rotated.secret_access_key) {
            setCredentials(rotated.access_key_id, rotated.secret_access_key);
          }
          setSavedCredentials({ ak: rotated.access_key_id, sk: rotated.secret_access_key ?? '' });
        }
        onSaved();
      } else {
        const req: CreateUserRequest = {
          name: name.trim(),
          enabled,
          permissions: rowsToPermissions(permissions),
          ...(accessKeyId.trim() ? { access_key_id: accessKeyId.trim() } : {}),
          ...(secretKey.trim() ? { secret_access_key: secretKey.trim() } : {}),
        };
        const created = await createUser(req);
        onSaved();
        onCreated?.(created.access_key_id, created.secret_access_key ?? '');
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Operation failed');
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (!user || deleting) return;
    setDeleting(true);
    try {
      await deleteUser(user.id);
      onDeleted?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Delete failed');
    } finally {
      setDeleting(false);
    }
  };

  const hasKeyChanges = isEdit && (
    (accessKeyId.trim() && accessKeyId.trim() !== user?.access_key_id) ||
    secretKey.trim().length > 0
  );

  const label = (text: string, hint?: string) => (
    <div style={{ marginBottom: 4 }}>
      <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5, fontWeight: 600 }}>{text}</Text>
      {hint && <Text type="secondary" style={{ fontSize: 10, fontWeight: 400, marginLeft: 6 }}>{hint}</Text>}
    </div>
  );

  return (
    <div style={{ padding: '24px 28px', maxWidth: 600, overflow: 'auto', height: '100%' }}>
      <div style={{ marginBottom: 20 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
          <Title level={5} style={{ margin: 0, fontFamily: 'var(--font-ui)' }}>
            {isEdit ? `Edit: ${user?.name}` : 'Create New User'}
          </Title>
          {isEdit && (
            <>
              <span style={{
                display: 'inline-flex', alignItems: 'center', gap: 4,
                fontSize: 11, fontWeight: 600, padding: '2px 8px', borderRadius: 10,
                background: enabled ? '#52c41a18' : '#00000010',
                color: enabled ? '#52c41a' : '#999',
                border: `1px solid ${enabled ? '#52c41a30' : '#00000015'}`,
              }}>
                {enabled
                  ? <><CheckCircleFilled style={{ fontSize: 10 }} /> Active</>
                  : <><MinusCircleFilled style={{ fontSize: 10 }} /> Disabled</>
                }
              </span>
              {user?.permissions.some(p => p.actions.includes('*') && p.resources.includes('*') && p.effect !== 'Deny') && (
                <span style={{
                  display: 'inline-flex', alignItems: 'center', gap: 4,
                  fontSize: 11, fontWeight: 600, padding: '2px 8px', borderRadius: 10,
                  background: '#faad1418', color: '#faad14',
                  border: '1px solid #faad1430',
                }}>
                  <CrownFilled style={{ fontSize: 10 }} /> Admin
                </span>
              )}
            </>
          )}
        </div>
        {isEdit && user?.access_key_id && (
          <Text style={{ fontSize: 12, fontFamily: 'var(--font-mono)', color: colors.TEXT_MUTED, marginTop: 4, display: 'block' }}>
            {user.access_key_id}
          </Text>
        )}
      </div>

      {savedCredentials && (
        <Alert
          type="success"
          showIcon
          closable
          onClose={() => setSavedCredentials(null)}
          message={isEdit ? 'Credentials updated' : 'User created'}
          description={
            <div style={{ marginTop: 8 }}>
              <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase' }}>Access Key</Text>
              <div><Text code copyable style={{ fontFamily: 'var(--font-mono)' }}>{savedCredentials.ak}</Text></div>
              <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', marginTop: 8, display: 'block' }}>Secret Key</Text>
              <div><Text code copyable style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{savedCredentials.sk}</Text></div>
              <Text type="warning" style={{ fontSize: 11, marginTop: 8, display: 'block' }}>The secret will not be shown again.</Text>
            </div>
          }
          style={{ marginBottom: 20, borderRadius: 8 }}
        />
      )}

      {error && <Alert type="error" message={error} showIcon closable onClose={() => setError('')} style={{ marginBottom: 16, borderRadius: 8 }} />}

      <div style={{ marginBottom: 16 }}>
        {label('Name')}
        <Input value={name} onChange={e => setName(e.target.value)} placeholder="e.g. ci-bot" style={{ ...inputRadius }} />
      </div>

      <div style={{ marginBottom: 16 }}>
        {label('Access Key ID', isEdit ? undefined : '(auto-generated if empty)')}
        <Space.Compact style={{ width: '100%' }}>
          <Input
            value={accessKeyId}
            onChange={e => setAccessKeyId(e.target.value)}
            placeholder={isEdit ? user?.access_key_id : 'e.g. user@company.com'}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          {!isEdit && (
            <Button icon={<ThunderboltOutlined />} onClick={() => setAccessKeyId(generateId())} title="Generate random key" />
          )}
        </Space.Compact>
      </div>

      <div style={{ marginBottom: 16 }}>
        {label('Secret Access Key', isEdit ? '(leave empty to keep current)' : '(auto-generated if empty)')}
        <Space.Compact style={{ width: '100%' }}>
          <Input.Password
            value={secretKey}
            onChange={e => setSecretKey(e.target.value)}
            placeholder={isEdit ? 'Enter new secret or leave empty' : 'e.g. mysecretkey or leave empty'}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          <Button icon={<ThunderboltOutlined />} onClick={() => setSecretKey(generateSecret())} title="Generate random secret" />
        </Space.Compact>
      </div>

      <div style={{ marginBottom: 20, display: 'flex', alignItems: 'center', gap: 12 }}>
        {label('Enabled')}
        <Switch checked={enabled} onChange={setEnabled} size="small" />
      </div>

      {isEdit && (() => {
        const memberGroups = userGroups.filter(g => user && g.member_ids.includes(user.id));
        if (memberGroups.length === 0) return null;
        return (
          <div style={{ marginBottom: 16 }}>
            {label('Member of')}
            <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
              {memberGroups.map(g => (
                <Tag
                  key={g.id}
                  color="blue"
                  style={{ borderRadius: 6, fontSize: 12, cursor: onNavigateToGroup ? 'pointer' : undefined }}
                  onClick={() => onNavigateToGroup?.(g.id)}
                >
                  {g.name} ({g.member_ids.length})
                </Tag>
              ))}
            </div>
          </div>
        );
      })()}

      <Divider style={{ margin: '20px 0 12px' }}>Permissions</Divider>

      {/* Presets as compact pill buttons */}
      <div style={{ marginBottom: 12, display: 'flex', flexWrap: 'wrap', gap: 6 }}>
        {(cannedPolicies.length > 0 ? cannedPolicies : Object.entries(FALLBACK_PRESETS).map(([name, perms]) => ({
          name,
          description: '',
          permissions: perms.map(p => ({ id: 0, effect: p.effect, actions: p.actions, resources: p.resources.split(',').map(s => s.trim()).filter(Boolean) })),
        }))).map(policy => {
          return (
              <Tag
                key={policy.name}
                color="blue"
                style={{ cursor: 'pointer', borderRadius: 10, fontSize: 12, padding: '2px 10px', margin: 0, userSelect: 'none' }}
                onClick={() => {
                  const hasExisting = permissions.some(r => r.actions.length > 0 || r.resources.trim() !== '');
                  if (hasExisting && !window.confirm('Replace existing permissions?')) return;
                  setPermissions(permissionsToRows(policy.permissions));
                }}
              >
                {policy.name}
              </Tag>
            );
          })}
      </div>

      <PermissionEditor permissions={permissions} onChange={setPermissions} />

      {isEdit && (() => {
        const memberGroups = userGroups.filter(g => user && g.member_ids.includes(user.id));
        const inheritedPerms = memberGroups.flatMap(g => g.permissions.map(p => ({ ...p, groupName: g.name })));
        if (inheritedPerms.length === 0) return null;
        return (
          <div style={{ marginBottom: 24 }}>
            <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5, fontWeight: 600, display: 'block', marginBottom: 8 }}>
              Inherited from Groups
            </Text>
            {inheritedPerms.map((perm, i) => {
              const isDeny = perm.effect === 'Deny';
              return (
                <div key={i} style={{
                  border: `1px solid ${isDeny ? '#ff4d4f20' : colors.BORDER}`,
                  borderLeft: isDeny ? '3px solid #ff4d4f60' : `1px solid ${colors.BORDER}`,
                  borderRadius: 8,
                  padding: 10,
                  marginBottom: 6,
                  opacity: 0.6,
                  background: isDeny ? '#ff4d4f05' : colors.BG_BASE,
                }}>
                  <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 4 }}>
                    <Tag color={isDeny ? 'red' : 'green'} style={{ fontSize: 10, borderRadius: 4 }}>{perm.effect}</Tag>
                    <Tag style={{ fontSize: 10, borderRadius: 4 }}>{perm.groupName}</Tag>
                  </div>
                  <Text style={{ fontSize: 11 }}>
                    {perm.actions.join(', ')} on {perm.resources.join(', ')}
                  </Text>
                </div>
              );
            })}
          </div>
        );
      })()}

      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <div>
          {isEdit && (
            <Button danger loading={deleting} disabled={deleting} onClick={async () => {
              if (!window.confirm(`Delete "${user?.name}"? This cannot be undone.`)) return;
              await handleDelete();
            }}>Delete User</Button>
          )}
          {!isEdit && onCancel && <Button onClick={onCancel}>Cancel</Button>}
        </div>
        {hasKeyChanges ? (
          <Button type="primary" loading={saving} onClick={async () => {
            if (!window.confirm('Update credentials? The new secret will be shown once — make sure to save it.')) return;
            await handleSave();
          }}>{isEdit ? 'Save' : 'Create User'}</Button>
        ) : (
          <Button type="primary" onClick={handleSave} loading={saving}>{isEdit ? 'Save' : 'Create User'}</Button>
        )}
      </div>
    </div>
  );
}
