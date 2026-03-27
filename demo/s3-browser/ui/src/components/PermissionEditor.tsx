import { Input, Button, Typography, Segmented, Tooltip, Checkbox } from 'antd';
import { PlusOutlined, DeleteOutlined } from '@ant-design/icons';
import type { IamPermission } from '../adminApi';
import { useCardStyles } from './shared-styles';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export const ACTION_OPTIONS = [
  { label: 'Read (GET/HEAD)', value: 'read' },
  { label: 'Write (PUT)', value: 'write' },
  { label: 'Delete (DELETE)', value: 'delete' },
  { label: 'List (ListObjects)', value: 'list' },
  { label: 'Admin (Bucket ops)', value: 'admin' },
  { label: 'All (*)', value: '*' },
];

export interface PermissionRow {
  effect: string; // "Allow" or "Deny"
  actions: string[];
  resources: string;
}

export function permissionsToRows(perms: IamPermission[]): PermissionRow[] {
  return perms.map(p => ({ effect: p.effect || 'Allow', actions: [...p.actions], resources: p.resources.join(', ') }));
}

export function rowsToPermissions(rows: PermissionRow[]): IamPermission[] {
  return rows
    .filter(r => r.actions.length > 0 && r.resources.trim() !== '')
    .map(r => ({
      id: 0,
      effect: r.effect || 'Allow',
      actions: r.actions,
      resources: r.resources.split(',').map(s => s.trim()).filter(Boolean),
    }));
}

interface PermissionEditorProps {
  permissions: PermissionRow[];
  onChange: (perms: PermissionRow[]) => void;
}

export default function PermissionEditor({ permissions, onChange }: PermissionEditorProps) {
  const { inputRadius } = useCardStyles();
  const colors = useColors();

  return (
    <>
      {permissions.map((row, i) => {
        const isDeny = row.effect === 'Deny';
        return (
          <div key={i} style={{
            border: `1px solid ${isDeny ? '#ff4d4f40' : colors.BORDER}`,
            borderLeft: isDeny ? '3px solid #ff4d4f' : `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            padding: 12,
            marginBottom: 8,
            background: isDeny ? '#ff4d4f08' : colors.BG_BASE,
          }}>
            <div style={{ marginBottom: 8, display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <Tooltip title="Deny rules override Allow rules">
                <Segmented
                  size="small"
                  value={row.effect || 'Allow'}
                  onChange={v => {
                    const updated = [...permissions];
                    updated[i] = { ...updated[i], effect: v as string };
                    onChange(updated);
                  }}
                  options={[
                    { label: 'Allow', value: 'Allow' },
                    { label: <span style={{ color: isDeny ? '#ff4d4f' : undefined, fontWeight: isDeny ? 600 : undefined }}>Deny</span>, value: 'Deny' },
                  ]}
                />
              </Tooltip>
              <Button type="text" danger size="small" icon={<DeleteOutlined />} onClick={() => onChange(permissions.filter((_, j) => j !== i))}>
                Remove
              </Button>
            </div>
            <div style={{ marginBottom: 8 }}>
              <Text type="secondary" style={{ fontSize: 12, fontWeight: 500 }}>Actions</Text>
              <Checkbox.Group
                value={row.actions}
                onChange={v => {
                  const updated = [...permissions];
                  updated[i] = { ...updated[i], actions: v as string[] };
                  onChange(updated);
                }}
                style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginTop: 4 }}
              >
                {ACTION_OPTIONS.map(opt => (
                  <Checkbox key={opt.value} value={opt.value} style={{ fontSize: 12 }}>{opt.label}</Checkbox>
                ))}
              </Checkbox.Group>
            </div>
            <div style={{ marginBottom: 4 }}>
              <Text type="secondary" style={{ fontSize: 12, fontWeight: 500 }}>Resources</Text>
              <Input
                value={row.resources}
                onChange={e => {
                  const updated = [...permissions];
                  updated[i] = { ...updated[i], resources: e.target.value };
                  onChange(updated);
                }}
                placeholder="e.g. my-bucket/*, my-bucket/releases/*"
                style={{ ...inputRadius, marginTop: 2 }}
              />
              <div style={{ fontSize: 11, color: colors.TEXT_MUTED, marginTop: 6, display: 'flex', flexWrap: 'wrap', gap: '4px 12px' }}>
                {[
                  ['*', 'all buckets & keys'],
                  ['my-bucket/*', 'everything in one bucket'],
                  ['my-bucket/builds/*', 'one prefix only'],
                ].map(([pattern, desc]) => (
                  <span key={pattern} style={{ whiteSpace: 'nowrap' }}>
                    <code style={{ background: 'var(--input-bg)', border: `1px solid ${colors.BORDER}`, padding: '1px 5px', borderRadius: 3, fontFamily: 'var(--font-mono)', fontSize: 10, color: colors.ACCENT_BLUE }}>{pattern}</code>
                    <span style={{ margin: '0 3px', opacity: 0.4 }}>{'\u2192'}</span>
                    <span style={{ fontSize: 10 }}>{desc}</span>
                  </span>
                ))}
              </div>
            </div>
          </div>
        );
      })}

      <Button type="dashed" icon={<PlusOutlined />} onClick={() => onChange([...permissions, { effect: 'Allow', actions: [], resources: '' }])} block style={{ borderRadius: 8, marginBottom: 16 }}>
        Add Permission Rule
      </Button>
    </>
  );
}
