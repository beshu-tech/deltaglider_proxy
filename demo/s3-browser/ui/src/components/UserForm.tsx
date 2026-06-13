import { useState } from 'react';
import { Input, Switch, Button, Alert, Space, Divider, Typography, Tag } from 'antd';
import { ThunderboltOutlined, CheckCircleFilled, MinusCircleFilled, CrownFilled } from '@ant-design/icons';
import type { IamUser, CreateUserRequest, UpdateUserRequest } from '../adminApi';
import { useCreateUser, useUpdateUser, useDeleteUser, useRotateUserKeys } from '../queries/users';
import { useCannedPolicies } from '../queries/cannedPolicies';
import { useGroups } from '../queries/groups';
import { setCredentials, getCredentials } from '../s3client';
import { useCardStyles } from './shared-styles';
import FormLabel from './FormLabel';
import { useColors } from '../ThemeContext';
import PermissionEditor from './PermissionEditor';
import PermissionSummarySection from './PermissionSummarySection';
import {
  freshPermissionRowId,
  permissionsToRows,
  rowsToPermissions,
  type PermissionRow,
} from './permissionRows';
import CredentialsBanner from './CredentialsBanner';
import { generateId, generateSecret } from '../credentialGeneration';

const { Text, Title } = Typography;

// Fallback presets used if the API is unavailable
const FALLBACK_PRESETS: Record<string, PermissionRow[]> = {
  'Full Admin': [{ effect: 'Allow', actions: ['*'], resources: '*' }],
  'Read/Write': [{ effect: 'Allow', actions: ['read', 'write', 'list'], resources: '*' }],
  'Read Only': [{ effect: 'Allow', actions: ['read', 'list'], resources: '*' }],
};

interface UserFormProps {
  user: IamUser | null; // null = create mode
  /**
   * Declarative IAM: render the form as a disabled, view-only snapshot of the
   * reconciled YAML state. All inputs are disabled and the Save/Delete action
   * row is hidden (the admin API would 403 anyway).
   */
  readOnly?: boolean;
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

export default function UserForm({ user, readOnly = false, onSaved, onDeleted, onCancel, onCreated, onSavingChange, onNavigateToGroup }: UserFormProps) {
  const isEdit = user !== null;
  const { inputRadius } = useCardStyles();
  const colors = useColors();

  // Initialize from `user` once. The form is remounted with a `key` per user
  // (see render site), so a keyed remount resets all fields from these
  // initializers — no prop→state sync effect (which was a redundant mirror).
  const [name, setName] = useState(() => user?.name ?? '');
  const [accessKeyId, setAccessKeyId] = useState(() => user?.access_key_id ?? '');
  const [secretKey, setSecretKey] = useState('');
  const [enabled, setEnabled] = useState(() => user?.enabled ?? true);
  const [permissions, setPermissions] = useState<PermissionRow[]>(() =>
    user
      ? permissionsToRows(user.permissions)
      : [{ _uiId: freshPermissionRowId(), effect: 'Allow', actions: ['*'], resources: '*' }],
  );
  const [saving, setSavingState] = useState(false);
  const [deleting, setDeletingState] = useState(false);
  const [error, setError] = useState('');
  const [savedCredentials, setSavedCredentials] = useState<{ ak: string; sk: string } | null>(null);

  // Presets + groups are read from the shared query cache (no per-mount fetch
  // effect). `?? []` keeps the render paths below identical to the old state.
  const cannedPolicies = useCannedPolicies().data ?? [];
  const userGroups = useGroups().data ?? [];

  // Mutations close the cache loop: each invalidates qk.users.list() on success
  // so the panel list refreshes without a manual reload callback.
  const createUserMutation = useCreateUser();
  const updateUserMutation = useUpdateUser();
  const deleteUserMutation = useDeleteUser();
  const rotateKeysMutation = useRotateUserKeys();

  const setSaving = (v: boolean) => { setSavingState(v); onSavingChange?.(v); };
  const setDeleting = (v: boolean) => { setDeletingState(v); onSavingChange?.(v); };


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
        await updateUserMutation.mutateAsync({ id: user.id, patch: req });

        const akChanged = accessKeyId.trim() && accessKeyId.trim() !== user.access_key_id;
        const skChanged = secretKey.trim().length > 0;
        if (akChanged || skChanged) {
          const rotated = await rotateKeysMutation.mutateAsync({
            id: user.id,
            accessKeyId: accessKeyId.trim() || user.access_key_id,
            secretAccessKey: skChanged ? secretKey.trim() : undefined,
          });
          const browserAk = getCredentials().accessKeyId;
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
        const created = await createUserMutation.mutateAsync(req);
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
      await deleteUserMutation.mutateAsync(user.id);
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

  return (
    <div style={{ padding: '24px 28px', maxWidth: 600, overflow: 'auto', height: '100%' }}>
      <div style={{ marginBottom: 20 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
          <Title level={5} style={{ margin: 0, fontFamily: 'var(--font-ui)' }}>
            {isEdit ? `${readOnly ? 'View' : 'Edit'}: ${user?.name}` : 'Create New User'}
          </Title>
          {isEdit && (
            <>
              <span style={{
                display: 'inline-flex', alignItems: 'center', gap: 4,
                fontSize: 11, fontWeight: 600, padding: '2px 8px', borderRadius: 10,
                background: enabled ? `${colors.ACCENT_GREEN}18` : `${colors.TEXT_MUTED}10`,
                color: enabled ? colors.ACCENT_GREEN : colors.TEXT_MUTED,
                border: `1px solid ${enabled ? `${colors.ACCENT_GREEN}30` : `${colors.TEXT_MUTED}15`}`,
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
                  background: `${colors.ACCENT_AMBER}18`, color: colors.ACCENT_AMBER,
                  border: `1px solid ${colors.ACCENT_AMBER}30`,
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
        <div style={{ marginBottom: 20 }}>
          <CredentialsBanner
            accessKey={savedCredentials.ak}
            secretKey={savedCredentials.sk}
            message={isEdit ? 'Credentials updated' : 'User created'}
            onClose={() => setSavedCredentials(null)}
          />
        </div>
      )}

      {error && <Alert type="error" message={error} showIcon closable onClose={() => setError('')} style={{ marginBottom: 16, borderRadius: 8 }} />}

      <div style={{ marginBottom: 16 }}>
        <FormLabel text="Name" />
        <Input data-testid="user-name" aria-label="User name" value={name} onChange={e => setName(e.target.value)} placeholder="e.g. ci-bot" disabled={readOnly} style={{ ...inputRadius }} />
      </div>

      <div style={{ marginBottom: 16 }}>
        <FormLabel text="Access Key ID" hint={isEdit ? undefined : '(auto-generated if empty)'} />
        <Space.Compact style={{ width: '100%' }}>
          <Input
            value={accessKeyId}
            onChange={e => setAccessKeyId(e.target.value)}
            placeholder={isEdit ? user?.access_key_id : 'e.g. user@company.com'}
            disabled={readOnly}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          {!isEdit && !readOnly && (
            <Button icon={<ThunderboltOutlined />} onClick={() => setAccessKeyId(generateId())} title="Generate random key" />
          )}
        </Space.Compact>
      </div>

      {!readOnly && (
        <div style={{ marginBottom: 16 }}>
          <FormLabel text="Secret Access Key" hint={isEdit ? '(leave empty to keep current)' : '(auto-generated if empty)'} />
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
      )}

      <div style={{ marginBottom: 20, display: 'flex', alignItems: 'center', gap: 12 }}>
        <FormLabel text="Enabled" />
        <Switch checked={enabled} onChange={setEnabled} size="small" disabled={readOnly} />
      </div>

      <Divider style={{ margin: '20px 0 12px' }}>Permissions</Divider>

      {/* Presets as compact pill buttons (hidden in read-only — nothing to apply). */}
      {!readOnly && (
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
      )}

      {/* Native <fieldset disabled> disables every descendant form control; the
          pointer-events guard also neutralises AntD's non-form clickable spans
          (Segmented, Tag, the "Clear all"/"Add Rule" buttons) so the whole
          editor is a true read-only snapshot without threading `disabled`
          through PermissionEditor's deep control tree. */}
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

      {isEdit && user && (
        <PermissionSummarySection
          user={user}
          permissions={permissions}
          userGroups={userGroups}
          onNavigateToGroup={onNavigateToGroup}
        />
      )}

      {/* Action row is hidden in read-only mode — the admin API would 403 and
          the IamSourceBanner already explains the YAML-apply workflow. */}
      {!readOnly && (
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
            <Button data-testid="user-save" aria-label={isEdit ? 'Save user' : 'Create user'} type="primary" loading={saving} onClick={async () => {
              if (!window.confirm('Update credentials? The new secret will be shown once — make sure to save it.')) return;
              await handleSave();
            }}>{isEdit ? 'Save' : 'Create User'}</Button>
          ) : (
            <Button data-testid="user-save" aria-label={isEdit ? 'Save user' : 'Create user'} type="primary" onClick={handleSave} loading={saving}>{isEdit ? 'Save' : 'Create User'}</Button>
          )}
        </div>
      )}
    </div>
  );
}
