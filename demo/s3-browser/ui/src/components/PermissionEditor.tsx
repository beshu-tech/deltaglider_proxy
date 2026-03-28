import { useState } from 'react';
import { Input, Button, Typography, Segmented, Tooltip, Checkbox } from 'antd';
import { PlusOutlined, DeleteOutlined, FilterOutlined } from '@ant-design/icons';
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
  effect: string;
  actions: string[];
  resources: string;
  conditions?: Record<string, Record<string, string | string[]>>;
}

export function permissionsToRows(perms: IamPermission[]): PermissionRow[] {
  return perms.map(p => ({
    effect: p.effect || 'Allow',
    actions: [...p.actions],
    resources: p.resources.join(', '),
    conditions: p.conditions,
  }));
}

export function rowsToPermissions(rows: PermissionRow[]): IamPermission[] {
  return rows
    .filter(r => r.actions.length > 0 && r.resources.trim() !== '')
    .map(r => {
      const perm: IamPermission = {
        id: 0,
        effect: r.effect || 'Allow',
        actions: r.actions,
        resources: r.resources.split(',').map(s => s.trim()).filter(Boolean),
      };
      // Only include conditions if at least one is non-empty
      if (r.conditions && Object.keys(r.conditions).length > 0) {
        const cleaned: Record<string, Record<string, string | string[]>> = {};
        for (const [op, kv] of Object.entries(r.conditions)) {
          const cleanedKv: Record<string, string | string[]> = {};
          for (const [k, v] of Object.entries(kv)) {
            if (typeof v === 'string' ? v.trim() : v.length > 0) {
              cleanedKv[k] = v;
            }
          }
          if (Object.keys(cleanedKv).length > 0) {
            cleaned[op] = cleanedKv;
          }
        }
        if (Object.keys(cleaned).length > 0) {
          perm.conditions = cleaned;
        }
      }
      return perm;
    });
}

/** Extract a simple condition value for UI display */
function getConditionValue(
  conditions: Record<string, Record<string, string | string[]>> | undefined,
  operator: string,
  key: string,
): string {
  if (!conditions) return '';
  const opBlock = conditions[operator];
  if (!opBlock) return '';
  const val = opBlock[key];
  if (Array.isArray(val)) return val.join(', ');
  return val || '';
}

/** Set a condition value, creating operator/key structure as needed */
function setConditionValue(
  conditions: Record<string, Record<string, string | string[]>> | undefined,
  operator: string,
  key: string,
  value: string,
): Record<string, Record<string, string | string[]>> {
  const result = conditions ? { ...conditions } : {};
  if (!value.trim()) {
    // Remove the key
    if (result[operator]) {
      const { [key]: _, ...rest } = result[operator];
      if (Object.keys(rest).length === 0) {
        delete result[operator];
      } else {
        result[operator] = rest;
      }
    }
    return result;
  }
  result[operator] = { ...(result[operator] || {}), [key]: value.trim() };
  return result;
}

/** Check if a rule has any conditions set */
function hasConditions(conditions?: Record<string, Record<string, string | string[]>>): boolean {
  if (!conditions) return false;
  return Object.values(conditions).some(kv => Object.values(kv).some(v =>
    typeof v === 'string' ? v.trim() !== '' : v.length > 0
  ));
}

interface PermissionEditorProps {
  permissions: PermissionRow[];
  onChange: (perms: PermissionRow[]) => void;
}

export default function PermissionEditor({ permissions, onChange }: PermissionEditorProps) {
  const { inputRadius } = useCardStyles();
  const colors = useColors();
  const [expandedConditions, setExpandedConditions] = useState<Set<number>>(
    () => new Set(permissions.map((p, i) => hasConditions(p.conditions) ? i : -1).filter(i => i >= 0))
  );

  const toggleConditions = (index: number) => {
    setExpandedConditions(prev => {
      const next = new Set(prev);
      if (next.has(index)) next.delete(index);
      else next.add(index);
      return next;
    });
  };

  const condLabelStyle: React.CSSProperties = {
    fontSize: 10,
    fontWeight: 600,
    letterSpacing: 0.5,
    textTransform: 'uppercase',
    color: colors.TEXT_MUTED,
    fontFamily: 'var(--font-ui)',
  };

  return (
    <>
      {permissions.map((row, i) => {
        const isDeny = row.effect === 'Deny';
        const showCond = expandedConditions.has(i);
        const hasCond = hasConditions(row.conditions);
        const prefixVal = getConditionValue(row.conditions, 'StringLike', 's3:prefix');
        const ipVal = getConditionValue(row.conditions, 'IpAddress', 'aws:SourceIp');

        return (
          <div key={i} style={{
            border: `1px solid ${isDeny ? '#ff4d4f40' : colors.BORDER}`,
            borderLeft: isDeny ? '3px solid #ff4d4f' : `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            padding: 12,
            marginBottom: 8,
            background: isDeny ? '#ff4d4f08' : colors.BG_BASE,
          }}>
            {/* Header: Allow/Deny + Conditions toggle + Remove */}
            <div style={{ marginBottom: 8, display: 'flex', alignItems: 'center', justifyContent: 'space-between' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
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
                {hasCond && !showCond && (
                  <span style={{
                    fontSize: 10,
                    color: colors.ACCENT_PURPLE,
                    background: 'rgba(167, 139, 250, 0.1)',
                    border: '1px solid rgba(167, 139, 250, 0.25)',
                    padding: '1px 6px',
                    borderRadius: 4,
                    fontFamily: 'var(--font-mono)',
                  }}>
                    {prefixVal && `prefix: ${prefixVal}`}
                    {prefixVal && ipVal && ' + '}
                    {ipVal && `ip: ${ipVal}`}
                  </span>
                )}
              </div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                <Tooltip title="Add conditions (prefix restriction, IP filtering)">
                  <Button
                    type="text"
                    size="small"
                    icon={<FilterOutlined />}
                    onClick={() => toggleConditions(i)}
                    style={{
                      opacity: showCond || hasCond ? 1 : 0.4,
                      color: hasCond ? colors.ACCENT_PURPLE : undefined,
                      padding: '2px 6px',
                      minWidth: 0,
                    }}
                  />
                </Tooltip>
                <Button type="text" danger size="small" icon={<DeleteOutlined />} onClick={() => onChange(permissions.filter((_, j) => j !== i))}>
                  Remove
                </Button>
              </div>
            </div>

            {/* Actions */}
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

            {/* Resources */}
            <div style={{ marginBottom: showCond ? 8 : 4 }}>
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

            {/* Conditions — collapsible */}
            {showCond && (
              <div style={{
                borderTop: `1px solid ${colors.BORDER}`,
                paddingTop: 10,
                marginTop: 4,
              }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 8 }}>
                  <FilterOutlined style={{ fontSize: 11, color: colors.ACCENT_PURPLE }} />
                  <Text style={{ fontSize: 12, fontWeight: 500, color: colors.ACCENT_PURPLE }}>Conditions</Text>
                  <span style={{ fontSize: 10, color: colors.TEXT_MUTED }}>
                    {'\u2014'} rule applies only when conditions match
                  </span>
                </div>

                <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                  {/* s3:prefix condition */}
                  <div>
                    <div style={condLabelStyle}>
                      Prefix restriction
                      <span style={{ fontWeight: 400, textTransform: 'none', marginLeft: 6, opacity: 0.6 }}>
                        StringLike on s3:prefix
                      </span>
                    </div>
                    <Input
                      value={prefixVal}
                      onChange={e => {
                        const updated = [...permissions];
                        updated[i] = {
                          ...updated[i],
                          conditions: setConditionValue(row.conditions, 'StringLike', 's3:prefix', e.target.value),
                        };
                        onChange(updated);
                      }}
                      placeholder="e.g. .* (dotfiles), uploads/* (one prefix)"
                      style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 12 }}
                    />
                    <div style={{ fontSize: 10, color: colors.TEXT_MUTED, marginTop: 4 }}>
                      Matches the <code style={{ fontSize: 10, fontFamily: 'var(--font-mono)' }}>prefix=</code> query parameter on LIST requests.
                      Use <code style={{ fontSize: 10, fontFamily: 'var(--font-mono)', color: colors.ACCENT_BLUE }}>.*</code> to match all dotfiles/folders.
                    </div>
                  </div>

                  {/* aws:SourceIp condition */}
                  <div>
                    <div style={condLabelStyle}>
                      IP restriction
                      <span style={{ fontWeight: 400, textTransform: 'none', marginLeft: 6, opacity: 0.6 }}>
                        IpAddress on aws:SourceIp
                      </span>
                    </div>
                    <Input
                      value={ipVal}
                      onChange={e => {
                        const updated = [...permissions];
                        updated[i] = {
                          ...updated[i],
                          conditions: setConditionValue(row.conditions, 'IpAddress', 'aws:SourceIp', e.target.value),
                        };
                        onChange(updated);
                      }}
                      placeholder="e.g. 192.168.0.0/16, 10.0.0.0/8"
                      style={{ ...inputRadius, fontFamily: 'var(--font-mono)', fontSize: 12 }}
                    />
                    <div style={{ fontSize: 10, color: colors.TEXT_MUTED, marginTop: 4 }}>
                      CIDR notation. Only requests from matching IPs will trigger this rule.
                    </div>
                  </div>
                </div>
              </div>
            )}
          </div>
        );
      })}

      <Button type="dashed" icon={<PlusOutlined />} onClick={() => onChange([...permissions, { effect: 'Allow', actions: [], resources: '' }])} block style={{ borderRadius: 8, marginBottom: 16 }}>
        Add Permission Rule
      </Button>
    </>
  );
}
