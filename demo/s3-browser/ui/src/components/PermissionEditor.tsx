import { useEffect, useState } from 'react';
import { Input, Button, Typography, Segmented, Checkbox } from 'antd';
import { PlusOutlined, DeleteOutlined, FilterOutlined } from '@ant-design/icons';
import { useCardStyles } from './shared-styles';
import { useColors } from '../ThemeContext';
import { listBuckets } from '../s3client';
import { parseResourcePattern } from '../storagePath';
import type { PermissionRow } from './permissionRows';
import { freshPermissionRowId } from './permissionRows';
import { getConditionValue, setConditionValue, hasConditions } from './permissionConditions';
import ResourcePatternInput from './ResourcePatternInput';
import ConditionPrefixInput from './ConditionPrefixInput';

const { Text } = Typography;

const ACTION_OPTIONS = [
  { label: 'Read (GET/HEAD)', value: 'read' },
  { label: 'Write (PUT)', value: 'write' },
  { label: 'Delete (DELETE)', value: 'delete' },
  { label: 'List (ListObjects)', value: 'list' },
  { label: 'Admin (Bucket ops)', value: 'admin' },
  { label: 'All (*)', value: '*' },
];

function firstConcreteResourceBucket(resources: string): string {
  for (const part of resources.split(',')) {
    const parsed = parseResourcePattern(part);
    if (parsed.bucket && !parsed.bucket.includes('${')) return parsed.bucket;
  }
  return '';
}

interface PermissionEditorProps {
  permissions: PermissionRow[];
  onChange: (perms: PermissionRow[]) => void;
}

export default function PermissionEditor({ permissions, onChange }: PermissionEditorProps) {
  const { inputRadius } = useCardStyles();
  const colors = useColors();
  const [bucketNames, setBucketNames] = useState<string[]>([]);
  /**
   * Ids of rows whose optional-filters pane was explicitly opened without data
   * yet. Keyed by the STABLE `_uiId` (not array index) so the expanded state
   * follows the row through reorder/delete instead of leaking onto whatever
   * row shifts into the old index.
   */
  const [expandedConditions, setExpandedConditions] = useState<Set<string>>(() => new Set());

  // Backfill stable ids on any row that arrived without one (literal presets,
  // FALLBACK_PRESETS, inline spreads). Done in an effect + propagated upward so
  // the id sticks in the controlled prop and stays constant across renders —
  // the guard keeps it from looping once every row has an id.
  useEffect(() => {
    if (permissions.some((p) => !p._uiId)) {
      onChange(
        permissions.map((p) => (p._uiId ? p : { ...p, _uiId: freshPermissionRowId() })),
      );
    }
  }, [permissions, onChange]);

  // Stable per-row key for this render even before the effect above persists
  // the id. Falls back to a deterministic index-based key ONLY for the transient
  // first render of an id-less row (the effect replaces it immediately after).
  const rowKey = (row: PermissionRow, index: number): string => row._uiId ?? `pending-${index}`;

  useEffect(() => {
    let cancelled = false;
    listBuckets()
      .then((buckets) => {
        if (!cancelled) setBucketNames(buckets.map((bucket) => bucket.name).filter(Boolean));
      })
      .catch(() => {
        if (!cancelled) setBucketNames([]);
      });

    return () => {
      cancelled = true;
    };
  }, []);

  // Drop expanded-state entries for rows that no longer exist (deleted), keyed
  // by id so a delete can't leak an expanded pane onto a surviving sibling.
  useEffect(() => {
    setExpandedConditions((prev) => {
      const liveIds = new Set(permissions.map((p) => p._uiId).filter(Boolean) as string[]);
      const next = new Set<string>();
      for (const id of prev) {
        if (liveIds.has(id)) next.add(id);
      }
      return next.size === prev.size ? prev : next;
    });
  }, [permissions]);

  const toggleConditions = (row: PermissionRow) => {
    if (!row._uiId || hasConditions(row.conditions)) {
      return;
    }
    const id = row._uiId;
    setExpandedConditions((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
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
  const monoTextStyle: React.CSSProperties = {
    fontFamily: 'var(--font-mono)',
    color: colors.TEXT_PRIMARY,
  };

  return (
    <>
      <details style={{ marginBottom: 12, border: `1px solid ${colors.BORDER}`, borderRadius: 8, padding: '8px 10px', background: colors.BG_BASE }}>
        <summary
          title="Open permission examples and variable help"
          style={{
            cursor: 'pointer',
            color: colors.TEXT_SECONDARY,
            fontSize: 12,
            fontWeight: 600,
            userSelect: 'none',
            minHeight: 28,
            lineHeight: '28px',
          }}
        >
          Examples and variables
        </summary>
        <div style={{ marginTop: 6, color: colors.TEXT_MUTED, fontSize: 12, lineHeight: 1.6 }}>
          Resources match objects: <span style={monoTextStyle}>bucket/*</span>, <span style={monoTextStyle}>bucket/releases/*</span>, or <span style={monoTextStyle}>*</span>. Prefix conditions filter LIST requests only, so pair <span style={monoTextStyle}>bucket/releases/*</span> with <span style={monoTextStyle}>releases/*</span> when users should only list that prefix.
          <div style={{ marginTop: 6 }}>
            Variables: <span style={monoTextStyle}>{'${username}'}</span> and <span style={monoTextStyle}>{'${access_key_id}'}</span> expand per authenticated user.
          </div>
        </div>
      </details>
      {permissions.map((row, i) => {
        const isDeny = row.effect === 'Deny';
        const hasCond = hasConditions(row.conditions);
        const conditionsVisible = (row._uiId ? expandedConditions.has(row._uiId) : false) || hasCond;
        const prefixVal = getConditionValue(row.conditions, 'StringLike', 's3:prefix');
        const ipVal = getConditionValue(row.conditions, 'IpAddress', 'aws:SourceIp');

        return (
          <div key={rowKey(row, i)} style={{
            border: `1px solid ${isDeny ? `${colors.ACCENT_RED}40` : colors.BORDER}`,
            borderLeft: isDeny ? `3px solid ${colors.ACCENT_RED}` : `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            padding: 14,
            marginBottom: 10,
            background: isDeny ? `${colors.ACCENT_RED}08` : colors.BG_BASE,
          }}>
            {/* Header: Allow/Deny + Conditions toggle + Remove */}
            <div style={{ marginBottom: 10, display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 10, flexWrap: 'wrap' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                <span title="Deny rules override Allow rules">
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
                      { label: <span style={{ color: isDeny ? colors.ACCENT_RED : undefined, fontWeight: isDeny ? 600 : undefined }}>Deny</span>, value: 'Deny' },
                    ]}
                  />
                </span>
              </div>
              <div style={{ display: 'flex', alignItems: 'center', gap: 4 }}>
                <Button
                  type={conditionsVisible ? 'default' : 'text'}
                  size="small"
                  icon={<FilterOutlined />}
                  title={
                    hasCond
                      ? 'Conditions are expanded while prefix or IP filters are set — clear those fields below to collapse.'
                      : 'Show optional conditions: prefix restriction and IP filtering'
                  }
                  disabled={hasCond}
                  onClick={() => toggleConditions(row)}
                  style={{
                    opacity: conditionsVisible ? 1 : 0.75,
                    color: hasCond ? colors.ACCENT_PURPLE : undefined,
                    borderColor: conditionsVisible ? `${colors.ACCENT_PURPLE}66` : undefined,
                  }}
                >
                  Conditions
                </Button>
                <Button type="text" danger size="small" icon={<DeleteOutlined />} onClick={() => onChange(permissions.filter((_, j) => j !== i))}>
                  Remove
                </Button>
              </div>
            </div>

            {/* Actions */}
            <div style={{ marginBottom: 12 }}>
              <Text type="secondary" style={{ fontSize: 12, fontWeight: 500 }}>Actions</Text>
              <Checkbox.Group
                value={row.actions}
                onChange={v => {
                  const updated = [...permissions];
                  updated[i] = { ...updated[i], actions: v as string[] };
                  onChange(updated);
                }}
                style={{ display: 'flex', flexWrap: 'wrap', gap: '6px 10px', marginTop: 6 }}
              >
                {ACTION_OPTIONS.map(opt => (
                  <Checkbox key={opt.value} value={opt.value} style={{ fontSize: 12 }}>{opt.label}</Checkbox>
                ))}
              </Checkbox.Group>
            </div>

            {/* Resources */}
            <div style={{ marginBottom: conditionsVisible ? 12 : 4 }}>
              <Text type="secondary" style={{ fontSize: 12, fontWeight: 500 }}>Resources</Text>
              <ResourcePatternInput
                value={row.resources}
                onChange={value => {
                  const updated = [...permissions];
                  updated[i] = { ...updated[i], resources: value };
                  onChange(updated);
                }}
                buckets={bucketNames}
                style={{ ...inputRadius, marginTop: 4 }}
              />
            </div>

            {/* Conditions — collapsible only when empty; persisted rules with data stay open */}
            {conditionsVisible && (
              <div style={{
                borderTop: `1px solid ${colors.BORDER}`,
                paddingTop: 12,
                marginTop: 6,
              }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 8 }}>
                  <FilterOutlined style={{ fontSize: 11, color: colors.ACCENT_PURPLE }} />
                  <Text style={{ fontSize: 12, fontWeight: 500, color: colors.ACCENT_PURPLE }}>Optional filters</Text>
                </div>

                <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
                  {/* s3:prefix condition */}
                  <div>
                    <div style={condLabelStyle}>
                      List prefix
                      <span style={{ fontWeight: 400, textTransform: 'none', marginLeft: 6, opacity: 0.6 }}>
                        s3:prefix StringLike
                      </span>
                    </div>
                    <ConditionPrefixInput
                      value={prefixVal}
                      bucket={firstConcreteResourceBucket(row.resources)}
                      onChange={value => {
                        const updated = [...permissions];
                        updated[i] = {
                          ...updated[i],
                          conditions: setConditionValue(row.conditions, 'StringLike', 's3:prefix', value),
                        };
                        onChange(updated);
                      }}
                      style={{ ...inputRadius, width: '100%', fontFamily: 'var(--font-mono)', fontSize: 12 }}
                    />
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

      <Button type="dashed" icon={<PlusOutlined />} onClick={() => onChange([...permissions, { _uiId: freshPermissionRowId(), effect: 'Allow', actions: [], resources: '' }])} block style={{ borderRadius: 8, marginBottom: 16 }}>
        Add Permission Rule
      </Button>
    </>
  );
}
