import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
  Modal,
  Space,
  Switch,
  Tag,
  Typography,
  message,
} from 'antd';
import {
  ClockCircleOutlined,
  DeleteOutlined,
  EyeOutlined,
  PlusOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import type {
  LifecycleConfig,
  LifecycleFailureEntry,
  LifecycleHistoryEntry,
  LifecycleRuleConfig,
  LifecycleRuleOverview,
  LifecycleRunOutcome,
  SectionApplyResponse,
  StorageSectionBody,
} from '../adminApi';
import {
  getLifecycleFailures,
  getLifecycleHistory,
  getLifecycleOverview,
  getSection,
  previewLifecycleRule,
  putSection,
  runLifecycleNow,
  validateSection,
} from '../adminApi';
import { listBuckets } from '../s3client';
import { useColors } from '../ThemeContext';
import { useApplyHandler, useDirtySection } from '../useDirtySection';
import { formatBytes } from '../utils';
import ApplyDialog from './ApplyDialog';
import SectionHeader from './SectionHeader';
import SimpleAutoComplete from './SimpleAutoComplete';
import SimpleSelect from './SimpleSelect';
import { useCardStyles } from './shared-styles';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

const DEFAULT_LIFECYCLE: LifecycleConfig = {
  enabled: false,
  tick_interval: '1h',
  max_failures_retained: 100,
  rules: [],
};

function emptyRule(existing: LifecycleRuleConfig[]): LifecycleRuleConfig {
  let n = existing.length + 1;
  let name = `expire-old-${n}`;
  while (existing.some((r) => r.name === name)) {
    n += 1;
    name = `expire-old-${n}`;
  }
  return {
    name,
    enabled: false,
    bucket: '',
    prefix: '',
    action: 'delete',
    expire_after: '30d',
    include_globs: [],
    exclude_globs: ['.deltaglider/**'],
    batch_size: 100,
  };
}

function normalizeLifecycle(input: Partial<LifecycleConfig> | undefined): LifecycleConfig {
  const cfg = { ...DEFAULT_LIFECYCLE, ...(input || {}) };
  return {
    ...cfg,
    rules: (cfg.rules || []).map((rule) => ({
      ...emptyRule([]),
      ...rule,
      action: normalizeAction(rule.action),
      prefix: rule.prefix || '',
      include_globs: rule.include_globs || [],
      exclude_globs: rule.exclude_globs || ['.deltaglider/**'],
      batch_size: rule.batch_size || 100,
    })),
  };
}

function lineList(value: string): string[] {
  return value
    .split('\n')
    .map((s) => s.trim())
    .filter(Boolean);
}

function lines(value: string[]): string {
  return value.join('\n');
}

function normalizePrefix(value: string): string {
  const parts = value.split('/').map((part) => part.trim()).filter(Boolean);
  return parts.length === 0 ? '' : `${parts.join('/')}/`;
}

function actionKind(action: LifecycleRuleConfig['action']): 'delete' | 'transition' {
  return typeof action === 'object' && action?.type ? 'transition' : 'delete';
}

function normalizeAction(action: LifecycleRuleConfig['action']): LifecycleRuleConfig['action'] {
  if (actionKind(action) === 'delete' || typeof action !== 'object') return 'delete';
  return {
    type: 'transition',
    destination: {
      bucket: action.destination?.bucket?.trim() || '',
      prefix: normalizePrefix(action.destination?.prefix || ''),
    },
    delete_source_after_success: Boolean(action.delete_source_after_success),
  };
}

function actionLabel(action: LifecycleRuleConfig['action'] | string | undefined): string {
  return actionKind(action as LifecycleRuleConfig['action']) === 'transition' ? 'archive/move' : 'delete';
}

function fmtUnix(ts: number | null | undefined): string {
  if (!ts) return 'never';
  return new Date(ts * 1000).toLocaleString();
}

function fmtDate(value: string): string {
  const d = new Date(value);
  return Number.isNaN(d.getTime()) ? value : d.toLocaleString();
}

function statusTone(status: string, enabled: boolean): 'success' | 'warning' | 'error' | 'default' {
  if (!enabled) return 'warning';
  if (status === 'failed') return 'error';
  if (status === 'succeeded') return 'success';
  return 'default';
}

export default function LifecyclePanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();
  const {
    value: lifecycle,
    setValue: setLifecycle,
    isDirty,
    discard,
    markApplied,
    resetWith,
  } = useDirtySection<LifecycleConfig>('storage', DEFAULT_LIFECYCLE);

  const [overview, setOverview] = useState<LifecycleRuleOverview[]>([]);
  const [history, setHistory] = useState<LifecycleHistoryEntry[]>([]);
  const [failures, setFailures] = useState<LifecycleFailureEntry[]>([]);
  const [runtimeError, setRuntimeError] = useState<string | null>(null);
  const [buckets, setBuckets] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  const [runLoading, setRunLoading] = useState(false);
  const [previews, setPreviews] = useState<Record<string, LifecycleRunOutcome>>({});
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(null);
  const [pendingBody, setPendingBody] = useState<StorageSectionBody | null>(null);
  const [applying, setApplying] = useState(false);

  const selectedRule = lifecycle.rules.find((r) => r.name === selected) || lifecycle.rules[0] || null;
  const selectedRuleName = selectedRule?.name;
  const selectedRuntime = selectedRule
    ? overview.find((r) => r.name === selectedRule.name)
    : null;
  const selectedPreview = selectedRuleName ? previews[selectedRuleName] : undefined;
  const enabledRuleCount = lifecycle.rules.filter((rule) => rule.enabled).length;
  const failedRuleCount = overview.filter((rule) => rule.last_status === 'failed').length;
  const lifetimeAffected = overview.reduce((sum, rule) => sum + rule.objects_affected_lifetime, 0);
  const lifetimeBytes = overview.reduce((sum, rule) => sum + rule.bytes_affected_lifetime, 0);

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const [section, lifecycleOverview, realBuckets] = await Promise.all([
        getSection<StorageSectionBody>('storage'),
        getLifecycleOverview().catch(() => null),
        listBuckets().catch(() => [] as Array<{ name: string }>),
      ]);
      const next = normalizeLifecycle(section.lifecycle);
      resetWith(next);
      setOverview(lifecycleOverview?.rules || []);
      setBuckets(realBuckets.map((b) => b.name));
      setSelected((cur) => {
        if (cur && next.rules.some((r) => r.name === cur)) return cur;
        return next.rules[0]?.name || null;
      });
      setError(null);
    } catch (e) {
      if (e instanceof Error && e.message.includes('401')) {
        onSessionExpired?.();
        return;
      }
      setError(e instanceof Error ? e.message : 'Failed to load lifecycle');
    } finally {
      setLoading(false);
    }
  }, [onSessionExpired, resetWith]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!selectedRuleName) {
      setHistory([]);
      setFailures([]);
      setRuntimeError(null);
      return;
    }
    let cancelled = false;
    (async () => {
      try {
        const [h, f] = await Promise.all([
          getLifecycleHistory(selectedRuleName).catch((err) => {
            throw err;
          }),
          getLifecycleFailures(selectedRuleName).catch((err) => {
            throw err;
          }),
        ]);
        if (!cancelled) {
          setHistory(h.runs);
          setFailures(f.failures);
          setRuntimeError(null);
        }
      } catch (e) {
        if (!cancelled) {
          setHistory([]);
          setFailures([]);
          setRuntimeError(
            e instanceof Error
              ? e.message
              : 'Lifecycle history/failures are unavailable from this API instance.'
          );
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selectedRuleName]);

  const updateConfig = (patch: Partial<LifecycleConfig>) => {
    setLifecycle({ ...lifecycle, ...patch });
  };

  const updateRule = (name: string, patch: Partial<LifecycleRuleConfig>) => {
    setLifecycle({
      ...lifecycle,
      rules: lifecycle.rules.map((rule) =>
        rule.name === name ? { ...rule, ...patch } : rule
      ),
    });
  };

  const addRule = () => {
    const rule = emptyRule(lifecycle.rules);
    setLifecycle({ ...lifecycle, rules: [...lifecycle.rules, rule] });
    setSelected(rule.name);
  };

  const removeRule = (name: string) => {
    const next = lifecycle.rules.filter((r) => r.name !== name);
    setLifecycle({ ...lifecycle, rules: next });
    setSelected(next[0]?.name || null);
    setPreviews((prev) => {
      const rest = { ...prev };
      delete rest[name];
      return rest;
    });
  };

  const confirmRemoveRule = (name: string) => {
    Modal.confirm({
      title: `Remove lifecycle rule ${name}?`,
      okText: 'Remove rule',
      okButtonProps: { danger: true },
      content: (
        <Text type="secondary">
          This only removes the YAML-backed rule draft. It does not delete objects.
        </Text>
      ),
      onOk: () => removeRule(name),
    });
  };

  const buildPayload = useCallback((): StorageSectionBody | null => {
    const normalizedRules = lifecycle.rules.map((rule) => ({
      ...rule,
      action: normalizeAction(rule.action),
      name: rule.name.trim(),
      bucket: rule.bucket.trim(),
      prefix: normalizePrefix(rule.prefix),
      expire_after: rule.expire_after.trim(),
      batch_size: rule.batch_size || 100,
    }));
    const names = normalizedRules.map((r) => r.name).filter(Boolean);
    const duplicate = names.find((name, idx) => names.indexOf(name) !== idx);
    if (duplicate) {
      message.error(`Duplicate rule name: ${duplicate}`);
      return null;
    }
    for (const rule of normalizedRules) {
      if (!rule.name) {
        message.error('Every lifecycle rule needs a name.');
        return null;
      }
      if (!/^[A-Za-z0-9_.-]{1,64}$/.test(rule.name)) {
        message.error(`Rule ${rule.name}: names must match [A-Za-z0-9_.-]{1,64}.`);
        return null;
      }
      if (!rule.bucket) {
        message.error(`Rule ${rule.name}: bucket is required.`);
        return null;
      }
      if (!rule.expire_after) {
        message.error(`Rule ${rule.name}: expire_after is required.`);
        return null;
      }
      if (actionKind(rule.action) === 'transition') {
        const action = rule.action as Exclude<LifecycleRuleConfig['action'], 'delete' | undefined>;
        if (!action.destination.bucket.trim()) {
          message.error(`Rule ${rule.name}: transition destination bucket is required.`);
          return null;
        }
      }
    }
    return {
      lifecycle: {
        ...lifecycle,
        rules: normalizedRules,
      },
    };
  }, [lifecycle]);

  const runApply = useCallback(async () => {
    const body = buildPayload();
    if (!body) return;
    try {
      const resp = await validateSection('storage', body);
      setApplyResponse(resp);
      setPendingBody(body);
      setApplyOpen(true);
    } catch (e) {
      message.error(`Validate failed: ${e instanceof Error ? e.message : 'unknown'}`);
    }
  }, [buildPayload]);

  const confirmApply = useCallback(async () => {
    if (!pendingBody) return;
    setApplying(true);
    try {
      const resp = await putSection('storage', pendingBody);
      if (!resp.ok) {
        message.error(resp.error || 'Apply failed');
        return;
      }
      message.success(resp.persisted_path ? `Applied + persisted to ${resp.persisted_path}` : 'Applied');
      markApplied();
      setApplyOpen(false);
      setPendingBody(null);
      setPreviews({});
      await refresh();
    } catch (e) {
      message.error(`Apply failed: ${e instanceof Error ? e.message : 'unknown'}`);
      setApplyOpen(false);
      setPendingBody(null);
      await refresh();
    } finally {
      setApplying(false);
    }
  }, [markApplied, pendingBody, refresh]);

  const previewRule = async (name: string) => {
    setPreviewLoading(true);
    try {
      const result = await previewLifecycleRule(name);
      setPreviews((prev) => ({ ...prev, [name]: result }));
      const label = result.candidates.some((c) => c.action === 'transition') ? 'transition' : 'delete';
      message.success(
        `Preview scanned ${result.objects_scanned}, would ${label} ${result.objects_affected} objects (${formatBytes(result.bytes_affected)}).`
      );
    } catch (e) {
      message.error(e instanceof Error ? e.message : 'Preview failed');
    } finally {
      setPreviewLoading(false);
    }
  };

  const confirmRunNow = (rule: LifecycleRuleConfig, preview: LifecycleRunOutcome) => {
    const isTransition = actionKind(rule.action) === 'transition';
    const transitionDeletesSource =
      isTransition &&
      typeof rule.action === 'object' &&
      Boolean(rule.action.delete_source_after_success);
    Modal.confirm({
      title: `${isTransition ? 'Transition' : 'Delete'} expired objects for ${rule.name}?`,
      icon: <WarningOutlined />,
      okText: isTransition ? 'Run transition now' : 'Run delete now',
      okButtonProps: { danger: !isTransition || transitionDeletesSource },
      content: (
        <div>
          <p>
            This executes the configured lifecycle rule against <Text code>{rule.bucket}/{rule.prefix || '*'}</Text>.
            The latest preview found <Text strong>{preview.objects_affected}</Text> {isTransition ? 'transition' : 'delete'} candidate{preview.objects_affected === 1 ? '' : 's'}
            {' '}({formatBytes(preview.bytes_affected)}).
          </p>
          {transitionDeletesSource ? (
            <Alert
              type="warning"
              showIcon
              message="Source delete is enabled"
              description="Lifecycle copies first and deletes the source only after the destination write verifies. Re-run Preview if the rule or bucket contents may have changed."
            />
          ) : (
            <Alert
              type={isTransition ? 'info' : 'warning'}
              showIcon
              message={isTransition ? 'Copy-first transition' : 'This is destructive'}
              description={isTransition ? 'The source object is preserved because delete_source_after_success is off.' : 'Deletes go through the DeltaGlider engine and cannot be undone from this UI.'}
            />
          )}
        </div>
      ),
      onOk: async () => {
        setRunLoading(true);
        try {
          const result = await runLifecycleNow(rule.name);
          setPreviews((prev) => ({ ...prev, [rule.name]: result }));
          message.success(
            `Lifecycle run ${result.status}: ${isTransition ? 'transitioned' : 'deleted'} ${result.objects_affected}, skipped ${result.objects_skipped}, errors ${result.errors}.`
          );
          await refresh();
        } catch (e) {
          message.error(e instanceof Error ? e.message : 'Run-now failed');
        } finally {
          setRunLoading(false);
        }
      },
    });
  };

  useApplyHandler('storage', runApply, isDirty);

  const canPreview = Boolean(selectedRule && selectedRuntime) && !isDirty;
  const canRun =
    Boolean(selectedRule && selectedRuntime && selectedPreview) &&
    !isDirty &&
    lifecycle.enabled &&
    Boolean(selectedRule?.enabled) &&
    selectedPreview!.objects_affected > 0;
  const runReason = !selectedRule
    ? 'Select a rule.'
    : !selectedRuntime
      ? 'Apply this rule before previewing or running it.'
      : isDirty
        ? 'Apply or discard pending lifecycle changes first.'
        : !selectedPreview
          ? 'Run Preview first.'
          : !lifecycle.enabled
            ? 'Global lifecycle is disabled.'
            : !selectedRule.enabled
              ? 'Rule is disabled.'
              : selectedPreview.objects_affected === 0
                ? 'Latest preview found no lifecycle candidates.'
                : `Run this ${actionLabel(selectedRule.action)} rule now.`;

  if (error) {
    return <Alert type="error" showIcon message="Failed to load lifecycle" description={error} />;
  }

  return (
    <div style={{ maxWidth: 1120, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
      {isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved lifecycle config"
          description="Lifecycle rules are YAML-backed storage config. Review the diff before enabling object deletion."
          style={{ marginBottom: 16 }}
          action={
            <Space>
              <Button size="small" onClick={discard} disabled={applying}>Discard</Button>
              <Button size="small" type="primary" onClick={runApply} loading={applying}>Review apply</Button>
            </Space>
          }
        />
      )}

      <div style={{ ...cardStyle, marginBottom: 16 }}>
        <SectionHeader
          icon={<ClockCircleOutlined />}
          title="Object lifecycle"
          description="Delete-only expiration rules. Preview is read-only; run-now is explicit and guarded."
        />
        <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap', marginTop: 14, alignItems: 'center' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <Switch checked={lifecycle.enabled} onChange={(enabled) => updateConfig({ enabled })} />
            <Text strong>Automatic scheduler</Text>
          </label>
          <Text type="secondary" style={{ fontSize: 12 }}>
            Disabled by default. Current tick: <Text code>{lifecycle.tick_interval}</Text>.
          </Text>
          <Button type="primary" onClick={runApply} disabled={!isDirty} loading={applying}>
            Review apply
          </Button>
        </div>
        <AdvancedDisclosure title="Advanced scheduler defaults">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
            <Field label="Scheduler tick">
              <Input
                value={lifecycle.tick_interval}
                onChange={(e) => updateConfig({ tick_interval: e.target.value })}
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                Default: <Text code>1h</Text>. Backend warns below 60s.
              </Text>
            </Field>
            <Field label="Failures retained">
              <InputNumber
                value={lifecycle.max_failures_retained}
                onChange={(v) => updateConfig({ max_failures_retained: Number(v) || 100 })}
                min={1}
                max={10000}
                style={{ width: '100%', ...inputRadius }}
              />
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                Also caps preview candidates returned by the API. Default: <Text code>100</Text>.
              </Text>
            </Field>
          </div>
        </AdvancedDisclosure>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 10, marginBottom: 16 }}>
        <Metric label="Rules" value={`${enabledRuleCount}/${lifecycle.rules.length} enabled`} />
        <Metric label="Scheduler" value={lifecycle.enabled ? 'enabled' : 'disabled'} tone={lifecycle.enabled ? undefined : 'warning'} />
        <Metric label="Failed rules" value={failedRuleCount} tone={failedRuleCount > 0 ? 'error' : undefined} />
        <Metric label="Lifetime affected" value={`${lifetimeAffected} · ${formatBytes(lifetimeBytes)}`} />
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: 'minmax(260px, 320px) minmax(0, 1fr)', gap: 16 }}>
        <div style={cardStyle}>
          <SectionHeader
            icon={<ClockCircleOutlined />}
            title="Rules"
            description={loading ? 'Loading...' : `${lifecycle.rules.length} configured rule${lifecycle.rules.length === 1 ? '' : 's'}.`}
          />
          <div style={{ marginTop: 12, display: 'flex', flexDirection: 'column', gap: 8 }}>
            {lifecycle.rules.map((rule) => {
              const runtime = overview.find((r) => r.name === rule.name);
              return (
                <button
                  key={rule.name}
                  onClick={() => setSelected(rule.name)}
                  style={{
                    textAlign: 'left',
                    border: `1px solid ${selectedRule?.name === rule.name ? colors.ACCENT_BLUE : colors.BORDER}`,
                    borderRadius: 10,
                    padding: 12,
                    background: selectedRule?.name === rule.name ? `${colors.ACCENT_BLUE}12` : colors.BG_ELEVATED,
                    cursor: 'pointer',
                  }}
                >
                  <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8 }}>
                    <Text strong style={{ fontSize: 13 }}>{rule.name}</Text>
                    <Tag color={statusTone(runtime?.last_status || 'idle', rule.enabled)}>
                      {rule.enabled ? runtime?.last_status || 'idle' : 'disabled'}
                    </Tag>
                  </div>
                  <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                    {rule.bucket || 'bucket'} / {rule.prefix || 'all'} · older than {rule.expire_after || '—'}
                  </Text>
                  <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
                    Lifetime affected: {runtime?.objects_affected_lifetime || 0} objects · {formatBytes(runtime?.bytes_affected_lifetime || 0)}
                  </Text>
                </button>
              );
            })}
            <Button icon={<PlusOutlined />} type="dashed" onClick={addRule} block>
              Add rule
            </Button>
          </div>
        </div>

        <div style={cardStyle}>
          {!selectedRule ? (
            <EmptyLifecycleState onAdd={addRule} />
          ) : (
            <>
              <RuleEditor
                rule={selectedRule}
                runtime={selectedRuntime || null}
                buckets={buckets}
                inputRadius={inputRadius}
                onChange={(patch) => updateRule(selectedRule.name, patch)}
                onRename={(nextName) => {
                  updateRule(selectedRule.name, { name: nextName });
                  setSelected(nextName);
                }}
              />

              <div style={{ marginTop: 16, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <Button
                  icon={<EyeOutlined />}
                  disabled={!canPreview}
                  title={canPreview ? 'Preview matching expired objects without deleting.' : runReason}
                  loading={previewLoading}
                  onClick={() => selectedRule && void previewRule(selectedRule.name)}
                >
                  Preview
                </Button>
                <Button
                  danger
                  type="primary"
                  icon={<DeleteOutlined />}
                  disabled={!canRun}
                  title={runReason}
                  loading={runLoading}
                  onClick={() => selectedRule && selectedPreview && confirmRunNow(selectedRule, selectedPreview)}
                >
                  Run {actionKind(selectedRule.action) === 'transition' ? 'transition' : 'delete'} now
                </Button>
                <Button danger onClick={() => confirmRemoveRule(selectedRule.name)}>
                  Remove rule
                </Button>
              </div>

              <PreviewPanel outcome={selectedPreview} maxCandidates={lifecycle.max_failures_retained} />
              <RuntimeDetails history={history} failures={failures} runtimeError={runtimeError} />
            </>
          )}
        </div>
      </div>

      <ApplyDialog
        open={applyOpen}
        section="storage"
        response={applyResponse}
        onApply={confirmApply}
        onCancel={() => {
          setApplyOpen(false);
          setPendingBody(null);
        }}
        loading={applying}
        summary={
          pendingBody?.lifecycle ? (
            <LifecycleApplySummary lifecycle={pendingBody.lifecycle} />
          ) : undefined
        }
      />
    </div>
  );
}

function LifecycleApplySummary({ lifecycle }: { lifecycle: LifecycleConfig }) {
  const colors = useColors();
  return (
    <div
      style={{
        border: `1px solid ${colors.BORDER}`,
        borderRadius: 8,
        padding: 12,
        background: 'var(--input-bg)',
      }}
    >
      <Text strong>Lifecycle plan</Text>
      <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 4 }}>
        Scheduler {lifecycle.enabled ? 'enabled' : 'disabled'} · tick {lifecycle.tick_interval} · {lifecycle.rules.length} rule{lifecycle.rules.length === 1 ? '' : 's'}
      </Text>
      <div style={{ marginTop: 10, display: 'flex', flexDirection: 'column', gap: 8 }}>
        {lifecycle.rules.length === 0 ? (
          <Text type="secondary" style={{ fontSize: 12 }}>No rules configured.</Text>
        ) : lifecycle.rules.map((rule) => (
          <div key={rule.name} style={{ fontSize: 12, lineHeight: 1.6 }}>
            <Text code>{rule.name}</Text>{' '}
            <Tag color={rule.enabled ? 'warning' : 'default'}>
              {rule.enabled ? `${actionLabel(rule.action)} enabled` : 'disabled'}
            </Tag>
            <div>
              <Text strong>Scope:</Text> {rule.bucket}/{rule.prefix || '*'} · older than {rule.expire_after}
            </div>
            {actionKind(rule.action) === 'transition' && typeof rule.action === 'object' && (
              <div>
                <Text strong>Destination:</Text> {rule.action.destination.bucket}/{rule.action.destination.prefix || '*'}
                {rule.action.delete_source_after_success ? ' · deletes source after verified copy' : ' · keeps source'}
              </div>
            )}
            {(rule.include_globs.length > 0 || rule.exclude_globs.length > 0) && (
              <div>
                Include: {rule.include_globs.length ? rule.include_globs.join(', ') : 'all'} · Exclude: {rule.exclude_globs.length ? rule.exclude_globs.join(', ') : 'none'}
              </div>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

function EmptyLifecycleState({ onAdd }: { onAdd: () => void }) {
  return (
    <div style={{ textAlign: 'center', padding: '48px 24px' }}>
      <ClockCircleOutlined style={{ fontSize: 32, opacity: 0.6 }} />
      <div style={{ marginTop: 12 }}><Text strong>No lifecycle rules</Text></div>
      <Text type="secondary" style={{ display: 'block', marginTop: 6 }}>
        Add a disabled draft rule, preview it, then explicitly enable deletion.
      </Text>
      <Button type="primary" icon={<PlusOutlined />} onClick={onAdd} style={{ marginTop: 16 }}>
        Add lifecycle rule
      </Button>
    </div>
  );
}

function RuleEditor({
  rule,
  runtime,
  buckets,
  inputRadius,
  onChange,
  onRename,
}: {
  rule: LifecycleRuleConfig;
  runtime: LifecycleRuleOverview | null;
  buckets: string[];
  inputRadius: { borderRadius: number };
  onChange: (patch: Partial<LifecycleRuleConfig>) => void;
  onRename: (nextName: string) => void;
}) {
  const transitionAction =
    actionKind(rule.action) === 'transition' && typeof rule.action === 'object'
      ? rule.action
      : null;
  const updateTransition = (
    patch: Partial<Exclude<LifecycleRuleConfig['action'], 'delete' | undefined>>
  ) => {
    const current = transitionAction || {
      type: 'transition' as const,
      destination: { bucket: '', prefix: 'archive/' },
      delete_source_after_success: false,
    };
    onChange({ action: { ...current, ...patch } });
  };

  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12, alignItems: 'flex-start' }}>
        <div>
          <Text strong style={{ fontSize: 16 }}>{rule.name}</Text>
          <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 2 }}>
            Last run: {fmtUnix(runtime?.last_run_at)} · Next due: {fmtUnix(runtime?.next_due_at)}
          </Text>
        </div>
        <Tag color={statusTone(runtime?.last_status || 'idle', rule.enabled)}>
          {rule.enabled ? runtime?.last_status || 'idle' : 'disabled'}
        </Tag>
      </div>

      <Alert
        type="warning"
        showIcon
        message="Lifecycle actions"
        description="Delete removes expired candidates. Archive/move copies through the same DeltaGlider engine path as replication, then optionally deletes the source after the copy verifies."
        style={{ marginTop: 14 }}
      />

      <div style={{ marginTop: 16, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 12 }}>
        <Field label="Rule name">
          <Input
            value={rule.name}
            onChange={(e) => onRename(e.target.value.replace(/[^A-Za-z0-9_.-]/g, '').slice(0, 64))}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </Field>
        <Field label="Enabled">
          <Switch checked={rule.enabled} onChange={(enabled) => onChange({ enabled })} />
          <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
            Per-rule delete switch. Global scheduler must also be enabled.
          </Text>
        </Field>
        <Field label="Bucket">
          <SimpleAutoComplete
            value={rule.bucket}
            onChange={(bucket) => onChange({ bucket })}
            options={buckets}
            placeholder="prod-artifacts"
          />
        </Field>
        <Field label="Prefix">
          <Input
            value={rule.prefix}
            onChange={(e) => onChange({ prefix: e.target.value })}
            onBlur={(e) => onChange({ prefix: normalizePrefix(e.target.value) })}
            placeholder="releases/"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          <PrefixHelp value={rule.prefix} />
        </Field>
        <Field label="Expire after">
          <Input
            value={rule.expire_after}
            onChange={(e) => onChange({ expire_after: e.target.value })}
            placeholder="30d"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
            Humantime duration, e.g. <Text code>30d</Text>, <Text code>12h</Text>, <Text code>90d</Text>.
          </Text>
        </Field>
        <Field label="Action">
          <SimpleSelect
            value={actionKind(rule.action)}
            onChange={(value) => {
              if (value === 'transition') {
                onChange({
                  action: {
                    type: 'transition',
                    destination: { bucket: '', prefix: 'archive/' },
                    delete_source_after_success: false,
                  },
                });
              } else {
                onChange({ action: 'delete' });
              }
            }}
            options={[
              { value: 'delete', label: 'Delete', sublabel: 'Expire source objects' },
              { value: 'transition', label: 'Archive / move', sublabel: 'Copy first, optional source delete' },
            ]}
            style={{ width: '100%', ...inputRadius }}
          />
        </Field>
      </div>

      {transitionAction && (
        <div style={{ marginTop: 12, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 12 }}>
          <Field label="Destination bucket">
            <SimpleAutoComplete
              value={transitionAction.destination?.bucket || ''}
              onChange={(bucket) => updateTransition({
                destination: { ...transitionAction.destination, bucket },
              })}
              options={buckets}
              placeholder="archive-artifacts"
            />
          </Field>
          <Field label="Destination prefix">
            <Input
              value={transitionAction.destination?.prefix || ''}
              onChange={(e) => updateTransition({
                destination: { ...transitionAction.destination, prefix: e.target.value },
              })}
              onBlur={(e) => updateTransition({
                destination: { ...transitionAction.destination, prefix: normalizePrefix(e.target.value) },
              })}
              placeholder="archive/"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </Field>
          <Field label="Delete source after copy">
            <Switch
              checked={Boolean(transitionAction.delete_source_after_success)}
              onChange={(checked) => updateTransition({ delete_source_after_success: checked })}
            />
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
              Off archives by copying only. On makes it a move, but source delete only happens after verified copy success.
            </Text>
          </Field>
        </div>
      )}

      <AdvancedDisclosure title="Filters and batch size">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))', gap: 12 }}>
          <Field label="Include globs">
            <Input.TextArea
              value={lines(rule.include_globs)}
              onChange={(e) => onChange({ include_globs: lineList(e.target.value) })}
              rows={3}
              placeholder={'*.zip\nreleases/**'}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
              Empty means include every key under the prefix.
            </Text>
          </Field>
          <Field label="Exclude globs">
            <Input.TextArea
              value={lines(rule.exclude_globs)}
              onChange={(e) => onChange({ exclude_globs: lineList(e.target.value) })}
              rows={3}
              placeholder=".deltaglider/**"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </Field>
          <Field label="Batch size">
            <InputNumber
              value={rule.batch_size}
              onChange={(batch_size) => onChange({ batch_size: Number(batch_size) || 100 })}
              min={1}
              max={10000}
              style={{ width: '100%', ...inputRadius }}
            />
            <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
              Worker/listing page size. Default: <Text code>100</Text>.
            </Text>
          </Field>
        </div>
      </AdvancedDisclosure>
    </div>
  );
}

function PreviewPanel({
  outcome,
  maxCandidates,
}: {
  outcome: LifecycleRunOutcome | undefined;
  maxCandidates: number;
}) {
  if (!outcome) {
    return (
      <Alert
        type="info"
        showIcon
        message="Preview before deleting"
        description="Preview is read-only and is required before Run delete now becomes available."
        style={{ marginTop: 18 }}
      />
    );
  }

  return (
    <div style={{ marginTop: 18 }}>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8, alignItems: 'center' }}>
        <Text strong>Latest preview/run result</Text>
        <Tag color={outcome.errors > 0 ? 'error' : 'processing'}>{outcome.status}</Tag>
      </div>
      <div style={{ marginTop: 8, display: 'flex', flexWrap: 'wrap', gap: 8 }}>
        <Metric label="Scanned" value={outcome.objects_scanned} />
        <Metric label="Would affect / affected" value={outcome.objects_affected} />
        <Metric label="Skipped" value={outcome.objects_skipped} />
        <Metric label="Bytes affected" value={formatBytes(outcome.bytes_affected)} />
        <Metric label="Errors" value={outcome.errors} tone={outcome.errors > 0 ? 'error' : undefined} />
      </div>
      <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 8 }}>
        Candidate list is capped by max failures/candidates (<Text code>{maxCandidates}</Text>).
      </Text>
      <div style={{ marginTop: 10, display: 'grid', gridTemplateColumns: '1fr', gap: 8, maxHeight: 300, overflow: 'auto' }}>
        {outcome.candidates.length === 0 ? (
          <Text type="secondary" style={{ fontSize: 12 }}>No lifecycle candidates returned.</Text>
        ) : outcome.candidates.map((obj) => (
          <div
            key={`${obj.bucket}/${obj.key}`}
            style={{
              border: '1px solid var(--border)',
              borderRadius: 10,
              padding: '8px 10px',
              background: 'var(--input-bg)',
              fontSize: 12,
              display: 'grid',
              gridTemplateColumns: 'minmax(0, 1fr) 160px 90px',
              gap: 10,
              alignItems: 'center',
            }}
          >
            <div>
              <Text code style={{ wordBreak: 'break-word' }}>{obj.bucket}/{obj.key}</Text>
              <div style={{ marginTop: 4 }}>
                <Tag color={obj.action === 'transition' ? 'processing' : 'warning'}>{obj.action}</Tag>
                {obj.destination_bucket && obj.destination_key && (
                  <Text type="secondary">
                    → <Text code>{obj.destination_bucket}/{obj.destination_key}</Text>
                    {obj.delete_source_after_success ? ' · delete source after copy' : ' · keep source'}
                  </Text>
                )}
              </div>
            </div>
            <Text type="secondary">{fmtDate(obj.created_at)}</Text>
            <Text>{formatBytes(obj.size)}</Text>
          </div>
        ))}
      </div>
      {outcome.failures.length > 0 && (
        <Alert
          type="error"
          showIcon
          message={`${outcome.failures.length} preview/run failure${outcome.failures.length === 1 ? '' : 's'}`}
          description={outcome.failures[0].error}
          style={{ marginTop: 10 }}
        />
      )}
    </div>
  );
}

function RuntimeDetails({
  history,
  failures,
  runtimeError,
}: {
  history: LifecycleHistoryEntry[];
  failures: LifecycleFailureEntry[];
  runtimeError: string | null;
}) {
  const latestRunId = history[0]?.id ?? null;
  const latestRunFailures = latestRunId == null
    ? failures
    : failures.filter((failure) => failure.run_id === latestRunId);
  const olderFailures = latestRunId == null
    ? []
    : failures.filter((failure) => failure.run_id !== latestRunId);

  return (
    <div style={{ marginTop: 18, display: 'flex', flexDirection: 'column', gap: 14 }}>
      {runtimeError && (
        <Alert
          type="info"
          showIcon
          message="Lifecycle runtime history unavailable"
          description={`The config API returned no history/failure data for this rule: ${runtimeError}`}
        />
      )}
      <div>
        <Text strong>Recent runs</Text>
        <div style={{ marginTop: 8, display: 'flex', flexWrap: 'wrap', gap: 8 }}>
          {history.length === 0 ? (
            <Text type="secondary" style={{ fontSize: 12 }}>No runs recorded.</Text>
          ) : history.map((run) => (
            <div
              key={run.id}
              style={{
                fontSize: 12,
                border: '1px solid var(--border)',
                borderRadius: 999,
                padding: '6px 10px',
                background: 'var(--input-bg)',
                whiteSpace: 'nowrap',
              }}
            >
              <Tag color={run.status === 'failed' ? 'error' : 'success'}>{run.status}</Tag>
              <Tag color="processing">{run.triggered_by}</Tag>
              {fmtUnix(run.started_at)} · affected {run.objects_affected}/{run.objects_scanned} · errors {run.errors}
            </div>
          ))}
        </div>
      </div>

      <FailureSection
        title={latestRunId == null ? 'Failures' : `Failures from latest run #${latestRunId}`}
        failures={latestRunFailures}
        emptyText={latestRunId == null ? 'No failures recorded.' : 'No failures recorded for the latest run.'}
        prominent
      />
      {olderFailures.length > 0 && (
        <FailureSection
          title="Older failures"
          failures={olderFailures}
          emptyText="No older failures."
        />
      )}
    </div>
  );
}

function FailureSection({
  title,
  failures,
  emptyText,
  prominent = false,
}: {
  title: string;
  failures: LifecycleFailureEntry[];
  emptyText: string;
  prominent?: boolean;
}) {
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 8, alignItems: 'center' }}>
        <Text strong>{title}</Text>
        {failures.length > 0 && <Tag color="error">{failures.length} shown</Tag>}
      </div>
      {prominent && failures.length > 0 && (
        <Alert
          type="error"
          showIcon
          style={{ marginTop: 8 }}
          message={failures[0].error_message}
          description={<Text code>{failures[0].bucket}/{failures[0].object_key}</Text>}
        />
      )}
      <div style={{ marginTop: 8, display: 'grid', gridTemplateColumns: '1fr', gap: 8, maxHeight: 300, overflow: 'auto' }}>
        {failures.length === 0 ? (
          <Text type="secondary" style={{ fontSize: 12 }}>{emptyText}</Text>
        ) : failures.map((failure) => (
          <div
            key={failure.id}
            style={{
              fontSize: 12,
              border: '1px solid var(--border)',
              borderRadius: 10,
              padding: '8px 10px',
              background: 'var(--input-bg)',
              display: 'grid',
              gridTemplateColumns: '160px minmax(0, 1fr) minmax(220px, 0.8fr)',
              gap: 10,
              alignItems: 'start',
            }}
          >
            <div>
              <WarningOutlined style={{ color: '#d1617a', marginRight: 6 }} />
              <Text type="secondary">{fmtUnix(failure.occurred_at)}</Text>
              {failure.run_id != null && <Tag>run #{failure.run_id}</Tag>}
            </div>
            <div style={{ wordBreak: 'break-word' }}>
              <Text code>{failure.bucket}/{failure.object_key || '(operation)'}</Text>
            </div>
            <Text type="secondary" style={{ display: 'block' }}>
              {failure.error_message}
            </Text>
          </div>
        ))}
      </div>
    </div>
  );
}

function Metric({
  label,
  value,
  tone,
}: {
  label: string;
  value: string | number;
  tone?: 'error' | 'warning';
}) {
  return (
    <div
      style={{
        border: '1px solid var(--border)',
        borderRadius: 10,
        padding: '8px 10px',
        background: 'var(--input-bg)',
        minWidth: 120,
      }}
    >
      <Text type="secondary" style={{ display: 'block', fontSize: 11 }}>{label}</Text>
      <Text strong type={tone === 'error' ? 'danger' : tone}>{value}</Text>
    </div>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label style={{ display: 'block' }}>
      <Text type="secondary" style={{ display: 'block', fontSize: 11, fontWeight: 700, textTransform: 'uppercase', letterSpacing: 0.5, marginBottom: 6 }}>
        {label}
      </Text>
      {children}
    </label>
  );
}

function AdvancedDisclosure({ title, children }: { title: string; children: React.ReactNode }) {
  const { BORDER, TEXT_SECONDARY } = useColors();
  return (
    <details
      style={{
        marginTop: 16,
        borderTop: `1px solid ${BORDER}`,
        paddingTop: 12,
      }}
    >
      <summary
        style={{
          cursor: 'pointer',
          color: TEXT_SECONDARY,
          fontSize: 12,
          fontWeight: 700,
          letterSpacing: 0.5,
          textTransform: 'uppercase',
          userSelect: 'none',
        }}
      >
        {title}
      </summary>
      <div style={{ marginTop: 12 }}>
        {children}
      </div>
    </details>
  );
}

function PrefixHelp({ value }: { value: string }) {
  const normalized = normalizePrefix(value);
  return (
    <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
      {normalized
        ? <>Canonical path: <Text code>{normalized}</Text></>
        : 'Leave empty for the whole bucket. Slashes are normalized automatically.'}
    </Text>
  );
}
