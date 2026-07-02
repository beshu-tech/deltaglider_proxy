import { useEffect, useState } from 'react';
import { Input, Button, Typography, Segmented, Alert } from 'antd';
import { PlusOutlined, DeleteOutlined, FilterOutlined } from '@ant-design/icons';
import { useCardStyles, usePermissionStyles } from './shared-styles';
import { useColors } from '../ThemeContext';
import { listBuckets } from '../s3client';
import { parseResourcePattern } from '../storagePath';
import type { PermissionRow } from './permissionRows';
import { freshPermissionRowId } from './permissionRows';
import {
  getConditionValue,
  setConditionValue,
  getConditionArray,
  setConditionArray,
  hasConditions,
} from './permissionConditions';
import { unknownBucketWarnings, invalidPatternWarnings } from './permissionWarnings';
import ActionChips from './ActionChips';
import { grantSummary, reconcileActionsForScope, effectiveActions, isPrefixScoped } from './permissionActions';
import ResourcePatternInput from './ResourcePatternInput';
import ConditionPrefixInput from './ConditionPrefixInput';

const { Text } = Typography;

function firstConcreteResourceBucket(resources: string[]): string {
  for (const part of resources) {
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
  const { condLabelStyle, monoTextStyle } = usePermissionStyles();
  const colors = useColors();
  const [bucketNames, setBucketNames] = useState<string[]>([]);
  /**
   * Ids of rows whose optional-filters pane was explicitly opened without data
   * yet. Keyed by the STABLE `_uiId` (not array index) so the expanded state
   * follows the row through reorder/delete instead of leaking onto whatever
   * row shifts into the old index.
   */
  const [expandedConditions, setExpandedConditions] = useState<Set<string>>(() => new Set());

  // Adopt externally-supplied rows the component doesn't control the creation
  // of (GroupsPanel's literal initial row, a backup-restored grant, hand-edited
  // YAML loaded into the DB). Two normalizations the source can't do for us:
  //   1. assign a stable `_uiId` to any id-less row, so every mutation below can
  //      key by id (never array index) — this is the single source of "creation"
  //      for rows that enter via the prop instead of the Add button.
  //   2. strip a phantom `admin` from a prefix-scoped grant. The Admin chip is
  //      disabled at prefix scope, so an `admin` (or `*`) that arrived that way
  //      could never be revoked interactively. The WHERE handler reconciles on
  //      every resource edit; this catches grants that load already-bad.
  // Idempotent — both checks no-op once a row is clean, so it can't loop. Rows
  // added via the Add button already carry an id, so they pass through untouched.
  useEffect(() => {
    const needsAdoption = permissions.some(
      (p) => !p._uiId || reconcileActionsForScope(p.actions, isPrefixScoped(p.resources)) !== p.actions,
    );
    if (needsAdoption) {
      onChange(
        permissions.map((p) => {
          const withId = p._uiId ? p : { ...p, _uiId: freshPermissionRowId() };
          const actions = reconcileActionsForScope(withId.actions, isPrefixScoped(withId.resources));
          return actions === withId.actions ? withId : { ...withId, actions };
        }),
      );
    }
  }, [permissions, onChange]);

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

  // Id-keyed mutation helpers — never index. The `permissions` prop IS the
  // state (controlled), so each helper maps/filters by `_uiId` and pushes the
  // result up via onChange. Index-based writes (`updated[i] = …`) re-associate
  // edits to whatever row shifts into a slot after a reorder/delete; keying by
  // the stable id is the whole point of `_uiId`.
  const updateRow = (id: string | undefined, change: Partial<PermissionRow>) => {
    if (!id) return;
    onChange(permissions.map((p) => (p._uiId === id ? { ...p, ...change } : p)));
  };

  const removeRow = (id: string | undefined) => {
    if (!id) return;
    onChange(permissions.filter((p) => p._uiId !== id));
    // Prune the deleted row's expanded-state inline (was a whole-list GC effect)
    // so a delete can't leak an open pane onto a surviving sibling.
    setExpandedConditions((prev) => {
      if (!prev.has(id)) return prev;
      const next = new Set(prev);
      next.delete(id);
      return next;
    });
  };

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
            Variables: <span style={monoTextStyle}>{'${iam:username}'}</span> and <span style={monoTextStyle}>{'${iam:access_key_id}'}</span> expand per authenticated user.
          </div>
        </div>
      </details>
      {permissions.map((row) => {
        const id = row._uiId;
        const isDeny = row.effect === 'Deny';
        const hasCond = hasConditions(row.conditions);
        const conditionsVisible = (row._uiId ? expandedConditions.has(row._uiId) : false) || hasCond;
        const prefixVal = getConditionArray(row.conditions, 'StringLike', 's3:prefix');
        const ipVal = getConditionValue(row.conditions, 'IpAddress', 'aws:SourceIp');
        const bucketWarnings = unknownBucketWarnings(row.resources, bucketNames);
        const patternErrors = invalidPatternWarnings(row.resources);
        // A rule is incomplete (and silently dropped on save by rowsToPermissions)
        // if it has no actions OR no resource. Surface it so the drop is never a
        // surprise — the operator sees exactly which half is missing.
        const noActions = effectiveActions(row.actions).size === 0;
        const noResource = row.resources.every((r) => r.trim() === '');
        const isIncomplete = noActions || noResource;

        return (
          <div key={id} style={{
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
                    onChange={v => updateRow(id, { effect: v as string })}
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
                <Button type="text" danger size="small" icon={<DeleteOutlined />} onClick={() => removeRow(id)}>
                  Remove
                </Button>
              </div>
            </div>

            {/* WHERE (resources) + CAN DO (action chips) — one tidy block.
                On wide screens WHERE and CAN DO sit side by side; below 720px
                they stack. The chips are a horizontal multi-select strip (not a
                cumulative ladder), so write-without-delete stays expressible. */}
            <div style={{
              display: 'flex',
              flexWrap: 'wrap',
              gap: 14,
              alignItems: 'flex-start',
              marginBottom: conditionsVisible ? 12 : 4,
            }}>
              <div style={{ flex: '1 1 280px', minWidth: 240 }}>
                <Text type="secondary" style={{ fontSize: 11, fontWeight: 600, letterSpacing: 0.4 }}>WHERE</Text>
                <ResourcePatternInput
                value={row.resources}
                onChange={value => {
                  // Reconcile actions to the new scope: narrowing from a bucket
                  // to a prefix invalidates `admin` (bucket-only op), so strip it
                  // here — the disabled Admin chip can no longer revoke it.
                  const actions = reconcileActionsForScope(row.actions, isPrefixScoped(value));
                  updateRow(id, { resources: value, actions });
                }}
                buckets={bucketNames}
                style={{ ...inputRadius, marginTop: 4 }}
              />
              {bucketWarnings.length > 0 && (
                <Alert
                  type="warning"
                  showIcon
                  style={{ marginTop: 6 }}
                  message={
                    <span style={{ fontSize: 12 }}>
                      {bucketWarnings.map((w) => (
                        <div key={w.resource}>
                          <span style={monoTextStyle}>{w.resource}</span> targets bucket{' '}
                          <span style={monoTextStyle}>{w.bucket}</span>, which doesn&apos;t exist
                          {w.suggestion ? (
                            <> — did you mean <span style={monoTextStyle}>{w.suggestion}</span>?</>
                          ) : (
                            <> — this rule will never match.</>
                          )}
                        </div>
                      ))}
                    </span>
                  }
                />
              )}
              {patternErrors.length > 0 && (
                <Alert
                  type="error"
                  showIcon
                  style={{ marginTop: 6 }}
                  message={
                    <span style={{ fontSize: 12 }}>
                      {patternErrors.map((msg, idx) => (
                        <div key={idx}>{msg}</div>
                      ))}
                    </span>
                  }
                />
              )}
              </div>

              <div style={{ flex: '1 1 360px', minWidth: 300 }}>
                <div style={{ display: 'flex', alignItems: 'center', gap: 10, minHeight: 18 }}>
                  <Text type="secondary" style={{ fontSize: 11, fontWeight: 600, letterSpacing: 0.4 }}>CAN DO</Text>
                  {effectiveActions(row.actions).size > 1 && (
                    <button
                      type="button"
                      title="Turn off all actions for this grant"
                      onClick={() => updateRow(id, { actions: [] })}
                      style={{
                        border: 'none',
                        background: 'transparent',
                        padding: 0,
                        cursor: 'pointer',
                        fontSize: 11,
                        fontWeight: 600,
                        color: colors.TEXT_SECONDARY,
                        textDecoration: 'underline',
                        textUnderlineOffset: 2,
                      }}
                    >
                      Clear all
                    </button>
                  )}
                </div>
                <div style={{ marginTop: 6 }}>
                  <ActionChips
                    actions={row.actions}
                    prefixScoped={isPrefixScoped(row.resources)}
                    onChange={next => updateRow(id, { actions: next })}
                  />
                  <div style={{
                    fontSize: 11,
                    color: row.actions.length === 0 ? colors.ACCENT_AMBER : colors.TEXT_MUTED,
                    marginTop: 6,
                    lineHeight: 1.45,
                  }}>
                    {grantSummary(row.actions)}
                  </div>
                  {row.actions.includes('*') && (
                    <Alert
                      type="warning"
                      showIcon
                      style={{ marginTop: 8 }}
                      message={
                        <span style={{ fontSize: 12 }}>
                          <strong>Administrative access.</strong> This grants full control
                          {isPrefixScoped(row.resources) ? ' of the targeted prefix' : ' of the bucket'},
                          including create/delete bucket operations. It overrides every narrower limit.
                        </span>
                      }
                    />
                  )}
                </div>
              </div>
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
                      onChange={value => updateRow(id, {
                        conditions: setConditionArray(row.conditions, 'StringLike', 's3:prefix', value),
                      })}
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
                      onChange={e => updateRow(id, {
                        conditions: setConditionValue(row.conditions, 'IpAddress', 'aws:SourceIp', e.target.value),
                      })}
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

            {isIncomplete && (
              <div style={{
                marginTop: 10,
                fontSize: 11,
                fontWeight: 600,
                color: colors.ACCENT_AMBER,
                display: 'flex',
                alignItems: 'center',
                gap: 6,
              }}>
                <FilterOutlined style={{ transform: 'rotate(180deg)' }} />
                {noResource && noActions
                  ? 'Incomplete — add a resource and at least one action, or this rule is dropped on save.'
                  : noResource
                    ? 'No resource — pick a bucket/prefix, or this rule is dropped on save.'
                    : 'No actions — turn on at least one, or this rule is dropped on save.'}
              </div>
            )}
          </div>
        );
      })}

      <Button type="dashed" icon={<PlusOutlined />} onClick={() => onChange([...permissions, { _uiId: freshPermissionRowId(), effect: 'Allow', actions: [], resources: [] }])} block style={{ borderRadius: 8, marginBottom: 16 }}>
        Add Permission Rule
      </Button>
    </>
  );
}
