import { useState, useEffect } from 'react';
import { Modal, Input, Switch, Select, Button, Alert, Space, Divider, Typography, message } from 'antd';
import { PlusOutlined, DeleteOutlined, CopyOutlined, ReloadOutlined } from '@ant-design/icons';
import type { IamUser, IamPermission, CreateUserRequest, UpdateUserRequest } from '../adminApi';
import { createUser, updateUser, rotateUserKeys } from '../adminApi';
import { useCardStyles } from './shared-styles';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

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

interface UserModalProps {
  open: boolean;
  user: IamUser | null; // null = create mode
  onClose: () => void;
  onSaved: () => void;
  onSessionExpired?: () => void;
}

export default function UserModal({ open, user, onClose, onSaved }: UserModalProps) {
  const isEdit = user !== null;
  const { inputRadius } = useCardStyles();
  const colors = useColors();

  const [name, setName] = useState('');
  const [enabled, setEnabled] = useState(true);
  const [permissions, setPermissions] = useState<PermissionRow[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [newSecret, setNewSecret] = useState<string | null>(null);
  const [newAccessKey, setNewAccessKey] = useState<string | null>(null);

  useEffect(() => {
    if (open) {
      if (user) {
        setName(user.name);
        setEnabled(user.enabled);
        setPermissions(permissionsToRows(user.permissions));
      } else {
        setName('');
        setEnabled(true);
        setPermissions([]);
      }
      setError('');
      setNewSecret(null);
      setNewAccessKey(null);
    }
  }, [open, user]);

  const handleSave = async () => {
    if (!name.trim()) { setError('Name is required'); return; }
    setSaving(true);
    setError('');
    try {
      if (isEdit) {
        const req: UpdateUserRequest = {
          name: name.trim(),
          enabled,
          permissions: rowsToPermissions(permissions),
        };
        await updateUser(user.id, req);
        onSaved();
        onClose();
      } else {
        const req: CreateUserRequest = {
          name: name.trim(),
          enabled,
          permissions: rowsToPermissions(permissions),
        };
        const created = await createUser(req);
        setNewSecret(created.secret_access_key ?? null);
        setNewAccessKey(created.access_key_id);
        onSaved();
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Operation failed');
    } finally {
      setSaving(false);
    }
  };

  const handleRotateKeys = async () => {
    if (!user) return;
    setSaving(true);
    setError('');
    try {
      const rotated = await rotateUserKeys(user.id);
      setNewSecret(rotated.secret_access_key ?? null);
      setNewAccessKey(rotated.access_key_id);
      onSaved();
      message.success('Keys rotated');
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Key rotation failed');
    } finally {
      setSaving(false);
    }
  };

  const copyToClipboard = (text: string, label: string) => {
    navigator.clipboard.writeText(text).then(
      () => message.success(`${label} copied`),
      () => message.error('Copy failed'),
    );
  };

  const addPermissionRow = () => {
    setPermissions([...permissions, { actions: [], resources: '' }]);
  };

  const removePermissionRow = (index: number) => {
    setPermissions(permissions.filter((_, i) => i !== index));
  };

  const updatePermissionRow = (index: number, field: keyof PermissionRow, value: string | string[]) => {
    const updated = [...permissions];
    if (field === 'actions') {
      updated[index] = { ...updated[index], actions: value as string[] };
    } else {
      updated[index] = { ...updated[index], resources: value as string };
    }
    setPermissions(updated);
  };

  const applyPreset = (presetName: string) => {
    setPermissions([...PRESETS[presetName]]);
  };

  // After create: show the secret once
  if (newSecret && !isEdit) {
    return (
      <Modal
        open={open}
        title="User Created"
        onCancel={onClose}
        footer={<Button type="primary" onClick={onClose}>Done</Button>}
      >
        <Alert
          type="success"
          showIcon
          message="Save these credentials — the secret will not be shown again."
          style={{ marginBottom: 16, borderRadius: 8 }}
        />
        <div style={{ marginBottom: 12 }}>
          <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5 }}>Access Key ID</Text>
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginTop: 4 }}>
            <Text code copyable style={{ fontFamily: 'var(--font-mono)' }}>{newAccessKey}</Text>
          </div>
        </div>
        <div>
          <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5 }}>Secret Access Key</Text>
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginTop: 4 }}>
            <Text code copyable style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{newSecret}</Text>
          </div>
        </div>
      </Modal>
    );
  }

  return (
    <Modal
      open={open}
      title={isEdit ? `Edit User: ${user?.name}` : 'Create User'}
      onCancel={onClose}
      width={600}
      footer={
        <Space>
          <Button onClick={onClose}>Cancel</Button>
          <Button type="primary" onClick={handleSave} loading={saving}>
            {isEdit ? 'Save' : 'Create'}
          </Button>
        </Space>
      }
    >
      {error && <Alert type="error" message={error} showIcon closable onClose={() => setError('')} style={{ marginBottom: 16, borderRadius: 8 }} />}

      {/* After key rotation: show new secret */}
      {newSecret && isEdit && (
        <Alert
          type="success"
          showIcon
          message="New keys generated — save the secret, it will not be shown again."
          description={
            <div style={{ marginTop: 8 }}>
              <Text code copyable style={{ fontFamily: 'var(--font-mono)' }}>{newAccessKey}</Text>
              <br />
              <Text code copyable style={{ fontFamily: 'var(--font-mono)', wordBreak: 'break-all' }}>{newSecret}</Text>
            </div>
          }
          style={{ marginBottom: 16, borderRadius: 8 }}
        />
      )}

      <div style={{ marginBottom: 16 }}>
        <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5, fontWeight: 600 }}>Name</Text>
        <Input value={name} onChange={e => setName(e.target.value)} placeholder="e.g. ci-bot" style={{ ...inputRadius, marginTop: 4 }} />
      </div>

      {isEdit && (
        <div style={{ marginBottom: 16 }}>
          <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5, fontWeight: 600 }}>Access Key</Text>
          <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginTop: 4 }}>
            <Text code style={{ fontFamily: 'var(--font-mono)' }}>{user?.access_key_id}</Text>
            <Button size="small" icon={<CopyOutlined />} onClick={() => copyToClipboard(user?.access_key_id ?? '', 'Access Key')} />
            <Button size="small" icon={<ReloadOutlined />} onClick={handleRotateKeys} loading={saving}>Rotate Keys</Button>
          </div>
        </div>
      )}

      <div style={{ marginBottom: 16, display: 'flex', alignItems: 'center', gap: 12 }}>
        <Text type="secondary" style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 0.5, fontWeight: 600 }}>Enabled</Text>
        <Switch checked={enabled} onChange={setEnabled} size="small" />
      </div>

      <Divider style={{ margin: '16px 0 12px' }}>Permissions</Divider>

      <Space style={{ marginBottom: 12 }} wrap>
        {Object.keys(PRESETS).map(preset => (
          <Button key={preset} size="small" onClick={() => applyPreset(preset)} style={{ borderRadius: 6 }}>
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
              onChange={v => updatePermissionRow(i, 'actions', v)}
              options={ACTION_OPTIONS}
              style={{ width: '100%', marginTop: 2 }}
              placeholder="Select actions..."
            />
          </div>
          <div style={{ marginBottom: 4 }}>
            <Text type="secondary" style={{ fontSize: 10, textTransform: 'uppercase', letterSpacing: 0.5 }}>Resources</Text>
            <Input
              value={row.resources}
              onChange={e => updatePermissionRow(i, 'resources', e.target.value)}
              placeholder="e.g. releases/*, snapshots/*"
              style={{ ...inputRadius, marginTop: 2 }}
            />
          </div>
          <div style={{ textAlign: 'right' }}>
            <Button type="text" danger size="small" icon={<DeleteOutlined />} onClick={() => removePermissionRow(i)}>
              Remove
            </Button>
          </div>
        </div>
      ))}

      <Button type="dashed" icon={<PlusOutlined />} onClick={addPermissionRow} block style={{ borderRadius: 8 }}>
        Add Permission Rule
      </Button>
    </Modal>
  );
}
