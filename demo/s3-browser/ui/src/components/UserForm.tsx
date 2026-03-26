import { useState, useEffect } from 'react';
import { Input, Switch, Select, Button, Alert, Space, Divider, Typography, Popconfirm } from 'antd';
import { PlusOutlined, DeleteOutlined, ThunderboltOutlined } from '@ant-design/icons';
import type { IamUser, IamPermission, CreateUserRequest, UpdateUserRequest } from '../adminApi';
import { createUser, updateUser, deleteUser, rotateUserKeys } from '../adminApi';
import { setCredentials } from '../s3client';
import { useCardStyles } from './shared-styles';
import { useColors } from '../ThemeContext';

const { Text, Title } = Typography;

const ACTION_OPTIONS = [
  { label: 'Read (GET/HEAD)', value: 'read' },
  { label: 'Write (PUT)', value: 'write' },
  { label: 'Delete (DELETE)', value: 'delete' },
  { label: 'List (ListObjects)', value: 'list' },
  { label: 'Admin (Bucket ops)', value: 'admin' },
  { label: 'All (*)', value: '*' },
];

interface PermissionRow {
  actions: string[];
  resources: string;
}

const PRESETS: Record<string, PermissionRow[]> = {
  'Full Admin': [{ actions: ['*'], resources: '*' }],
  'Read/Write': [{ actions: ['read', 'write', 'list'], resources: '*' }],
  'Read Only': [{ actions: ['read', 'list'], resources: '*' }],
};

function permissionsToRows(perms: IamPermission[]): PermissionRow[] {
  return perms.map(p => ({ actions: [...p.actions], resources: p.resources.join(', ') }));
}

function rowsToPermissions(rows: PermissionRow[]): IamPermission[] {
  return rows
    .filter(r => r.actions.length > 0 && r.resources.trim() !== '')
    .map(r => ({
      id: 0,
      actions: r.actions,
      resources: r.resources.split(',').map(s => s.trim()).filter(Boolean),
    }));
}

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
}

export default function UserForm({ user, onSaved, onDeleted, onCancel }: UserFormProps) {
  const isEdit = user !== null;
  const { inputRadius } = useCardStyles();
  const colors = useColors();

  const [name, setName] = useState('');
  const [accessKeyId, setAccessKeyId] = useState('');
  const [secretKey, setSecretKey] = useState('');
  const [enabled, setEnabled] = useState(true);
  const [permissions, setPermissions] = useState<PermissionRow[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [savedCredentials, setSavedCredentials] = useState<{ ak: string; sk: string } | null>(null);

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
      setPermissions([{ actions: ['*'], resources: '*' }]);
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
          if (browserAk === user.access_key_id && rotated.secret_access_key) {
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
        setSavedCredentials({ ak: created.access_key_id, sk: created.secret_access_key ?? '' });
        onSaved();
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Operation failed');
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (!user) return;
    try {
      await deleteUser(user.id);
      onDeleted?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Delete failed');
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

  // After successful CREATE: show only credentials, nothing else
  if (savedCredentials && !isEdit) {
    return (
      <div style={{ padding: '24px 28px', maxWidth: 600 }}>
        <Alert
          type="success"
          showIcon
          message="User created"
          description={
            <div style={{ marginTop: 8 }}>
              <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase' }}>Access Key</Text>
              <div><Text code copyable style={{ fontFamily: 'var(--font-mono)' }}>{savedCredentials.ak}</Text></div>
              <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', marginTop: 8, display: 'block' }}>Secret Key</Text>
              <div><Text code copyable style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{savedCredentials.sk}</Text></div>
              <Text type="warning" style={{ fontSize: 11, marginTop: 8, display: 'block' }}>The secret will not be shown again.</Text>
            </div>
          }
          style={{ borderRadius: 8 }}
        />
        <div style={{ marginTop: 16, textAlign: 'right' }}>
          <Button type="primary" onClick={() => { setSavedCredentials(null); onCancel?.(); }}>Done</Button>
        </div>
      </div>
    );
  }

  return (
    <div style={{ padding: '24px 28px', maxWidth: 600, overflow: 'auto', height: '100%' }}>
      <Title level={5} style={{ margin: '0 0 20px', fontFamily: 'var(--font-ui)' }}>
        {isEdit ? `Edit: ${user?.name}` : 'Create New User'}
      </Title>

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

      <Divider style={{ margin: '16px 0 12px' }}>Permissions</Divider>

      <Space style={{ marginBottom: 12 }} wrap>
        {Object.keys(PRESETS).map(preset => (
          <Button key={preset} size="small" onClick={() => setPermissions([...PRESETS[preset]])} style={{ borderRadius: 6 }}>
            {preset}
          </Button>
        ))}
      </Space>

      {permissions.map((row, i) => (
        <div key={i} style={{
          border: `1px solid ${colors.BORDER}`,
          borderRadius: 8,
          padding: 12,
          marginBottom: 8,
          background: colors.BG_BASE,
        }}>
          <div style={{ marginBottom: 8 }}>
            <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', letterSpacing: 0.5 }}>Actions</Text>
            <Select
              mode="multiple"
              value={row.actions}
              onChange={v => {
                const updated = [...permissions];
                updated[i] = { ...updated[i], actions: v };
                setPermissions(updated);
              }}
              options={ACTION_OPTIONS}
              style={{ width: '100%', marginTop: 2 }}
              placeholder="Select actions..."
            />
          </div>
          <div style={{ marginBottom: 4 }}>
            <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', letterSpacing: 0.5 }}>Resources</Text>
            <Input
              value={row.resources}
              onChange={e => {
                const updated = [...permissions];
                updated[i] = { ...updated[i], resources: e.target.value };
                setPermissions(updated);
              }}
              placeholder="e.g. releases/*, snapshots/*"
              style={{ ...inputRadius, marginTop: 2 }}
            />
          </div>
          <div style={{ textAlign: 'right' }}>
            <Button type="text" danger size="small" icon={<DeleteOutlined />} onClick={() => setPermissions(permissions.filter((_, j) => j !== i))}>
              Remove
            </Button>
          </div>
        </div>
      ))}

      <Button type="dashed" icon={<PlusOutlined />} onClick={() => setPermissions([...permissions, { actions: [], resources: '' }])} block style={{ borderRadius: 8, marginBottom: 24 }}>
        Add Permission Rule
      </Button>

      <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
        <div>
          {isEdit && (
            <Popconfirm title={`Delete "${user?.name}"?`} description="This cannot be undone." onConfirm={handleDelete} okText="Delete" okButtonProps={{ danger: true }}>
              <Button danger>Delete User</Button>
            </Popconfirm>
          )}
          {!isEdit && onCancel && <Button onClick={onCancel}>Cancel</Button>}
        </div>
        {hasKeyChanges ? (
          <Popconfirm
            title="Update credentials?"
            description="The new secret will be shown once — make sure to save it."
            onConfirm={handleSave}
            okText="Yes, update"
            okButtonProps={{ loading: saving }}
          >
            <Button type="primary" loading={saving}>{isEdit ? 'Save' : 'Create User'}</Button>
          </Popconfirm>
        ) : (
          <Button type="primary" onClick={handleSave} loading={saving}>{isEdit ? 'Save' : 'Create User'}</Button>
        )}
      </div>
    </div>
  );
}
