import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
  Radio,
  Space,
  Switch,
  Tag,
  Typography,
  message,
} from 'antd';
import {
  PauseCircleOutlined,
  PlayCircleOutlined,
  PlusOutlined,
  RocketOutlined,
  SyncOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import type {
  ReplicationConfig,
  ReplicationFailureEntry,
  ReplicationHistoryEntry,
  ReplicationRuleConfig,
  ReplicationRuleOverview,
  StorageSectionBody,
  SectionApplyResponse,
} from '../adminApi';
import {
  getReplicationFailures,
  getReplicationHistory,
  getReplicationOverview,
  getSection,
  pauseReplicationRule,
  putSection,
  resumeReplicationRule,
  runReplicationNow,
  validateSection,
} from '../adminApi';
import { listBuckets } from '../s3client';
import { useColors } from '../ThemeContext';
import { useCardStyles } from './shared-styles';
import SectionHeader from './SectionHeader';
import SimpleAutoComplete from './SimpleAutoComplete';
import ApplyDialog from './ApplyDialog';
import { useApplyHandler, useDirtySection } from '../useDirtySection';
import { formatBytes } from '../utils';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

const DEFAULT_REPLICATION: ReplicationConfig = {
  enabled: true,
  tick_interval: '30s',
  lease_ttl: '60s',
  heartbeat_interval: '20s',
  max_failures_retained: 100,
  rules: [],
};

function emptyRule(existing: ReplicationRuleConfig[]): ReplicationRuleConfig {
  let n = existing.length + 1;
  let name = `rule-${n}`;
  while (existing.some((r) => r.name === name)) {
    n += 1;
    name = `rule-${n}`;
  }
  return {
    name,
    enabled: true,
    source: { bucket: '', prefix: '' },
    destination: { bucket: '', prefix: '' },
    interval: '15m',
    batch_size: 100,
    replicate_deletes: false,
    conflict: 'newer-wins',
    include_globs: [],
    exclude_globs: ['.dg/*'],
  };
}

function normalizeReplication(input: Partial<ReplicationConfig> | undefined): ReplicationConfig {
  const cfg = { ...DEFAULT_REPLICATION, ...(input || {}) };
  return {
    ...cfg,
    rules: (cfg.rules || []).map((r) => ({
      ...emptyRule([]),
      ...r,
      source: { bucket: r.source?.bucket || '', prefix: r.source?.prefix || '' },
      destination: {
        bucket: r.destination?.bucket || '',
        prefix: r.destination?.prefix || '',
      },
      include_globs: r.include_globs || [],
      exclude_globs: r.exclude_globs || ['.dg/*'],
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

function fmtUnix(ts: number | null | undefined): string {
  if (!ts) return 'never';
  return new Date(ts * 1000).toLocaleString();
}

function statusTone(status: string, paused: boolean, enabled: boolean): 'success' | 'warning' | 'error' | 'default' {
  if (paused || !enabled) return 'warning';
  if (status === 'failed') return 'error';
  if (status === 'succeeded') return 'success';
  return 'default';
}

export default function ReplicationPanel({ onSessionExpired }: Props) {
  const colors = useColors();
  const { cardStyle, inputRadius } = useCardStyles();
  const {
    value: replication,
    setValue: setReplication,
    isDirty,
    discard,
    markApplied,
    resetWith,
  } = useDirtySection<ReplicationConfig>('storage', DEFAULT_REPLICATION);

  const [overview, setOverview] = useState<ReplicationRuleOverview[]>([]);
  const [history, setHistory] = useState<ReplicationHistoryEntry[]>([]);
  const [failures, setFailures] = useState<ReplicationFailureEntry[]>([]);
  const [buckets, setBuckets] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [actionLoading, setActionLoading] = useState<string | null>(null);
  const [applyOpen, setApplyOpen] = useState(false);
  const [applyResponse, setApplyResponse] = useState<SectionApplyResponse | null>(null);
  const [pendingBody, setPendingBody] = useState<StorageSectionBody | null>(null);
  const [applying, setApplying] = useState(false);

  const selectedRule = replication.rules.find((r) => r.name === selected) || replication.rules[0] || null;
  const selectedRuleName = selectedRule?.name;
  const selectedRuntime = selectedRule
    ? overview.find((r) => r.name === selectedRule.name)
    : null;

  const refresh = useCallback(async () => {
    try {
      setLoading(true);
      const [section, repl, realBuckets] = await Promise.all([
        getSection<StorageSectionBody>('storage'),
        getReplicationOverview().catch(() => null),
        listBuckets().catch(() => [] as Array<{ name: string }>),
      ]);
      const next = normalizeReplication(section.replication);
      resetWith(next);
      setOverview(repl?.rules || []);
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
      setError(e instanceof Error ? e.message : 'Failed to load replication');
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
      return;
    }
    let cancelled = false;
    (async () => {
      const [h, f] = await Promise.all([
        getReplicationHistory(selectedRuleName).catch(() => ({ runs: [] })),
        getReplicationFailures(selectedRuleName).catch(() => ({ failures: [] })),
      ]);
      if (!cancelled) {
        setHistory(h.runs);
        setFailures(f.failures);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selectedRuleName]);

  const updateConfig = (patch: Partial<ReplicationConfig>) => {
    setReplication({ ...replication, ...patch });
  };

  const updateRule = (name: string, patch: Partial<ReplicationRuleConfig>) => {
    setReplication({
      ...replication,
      rules: replication.rules.map((rule) =>
        rule.name === name ? { ...rule, ...patch } : rule
      ),
    });
  };

  const addRule = () => {
    const rule = emptyRule(replication.rules);
    setReplication({ ...replication, rules: [...replication.rules, rule] });
    setSelected(rule.name);
  };

  const removeRule = (name: string) => {
    const next = replication.rules.filter((r) => r.name !== name);
    setReplication({ ...replication, rules: next });
    setSelected(next[0]?.name || null);
  };

  const buildPayload = useCallback((): StorageSectionBody | null => {
    const normalizedRules = replication.rules.map((rule) => ({
      ...rule,
      source: { ...rule.source, prefix: normalizePrefix(rule.source.prefix) },
      destination: { ...rule.destination, prefix: normalizePrefix(rule.destination.prefix) },
    }));
    const names = normalizedRules.map((r) => r.name.trim()).filter(Boolean);
    const duplicate = names.find((name, idx) => names.indexOf(name) !== idx);
    if (duplicate) {
      message.error(`Duplicate rule name: ${duplicate}`);
      return null;
    }
    for (const rule of normalizedRules) {
      if (!rule.name.trim()) {
        message.error('Every replication rule needs a name.');
        return null;
      }
      if (!rule.source.bucket || !rule.destination.bucket) {
        message.error(`Rule ${rule.name}: source and destination buckets are required.`);
        return null;
      }
    }
    return { replication: { ...replication, rules: normalizedRules } };
  }, [replication]);

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

  const runAction = async (name: string, action: 'run' | 'pause' | 'resume') => {
    setActionLoading(`${action}:${name}`);
    try {
      if (action === 'run') {
        const result = await runReplicationNow(name);
        message.success(`Run ${result.status}: copied ${result.objects_copied}, skipped ${result.objects_skipped}, errors ${result.errors}`);
      } else if (action === 'pause') {
        await pauseReplicationRule(name);
        message.success(`Paused ${name}`);
      } else {
        await resumeReplicationRule(name);
        message.success(`Resumed ${name}`);
      }
      await refresh();
    } catch (e) {
      message.error(e instanceof Error ? e.message : `${action} failed`);
    } finally {
      setActionLoading(null);
    }
  };

  useApplyHandler('storage', runApply, isDirty);

  const canRun =
    Boolean(selectedRule && selectedRuntime) &&
    !isDirty &&
    replication.enabled &&
    Boolean(selectedRule?.enabled) &&
    !selectedRuntime?.paused;
  const runReason = !selectedRule
    ? 'Select a rule.'
    : !selectedRuntime
      ? 'Apply this rule before running it.'
      : isDirty
        ? 'Apply or discard pending replication changes before running.'
        : !replication.enabled
          ? 'Global replication is disabled.'
          : !selectedRule.enabled
            ? 'Rule is disabled.'
            : selectedRuntime?.paused
              ? 'Rule is paused.'
              : 'Run this rule now.';

  if (error) {
    return <Alert type="error" showIcon message="Failed to load replication" description={error} />;
  }

  return (
    <div style={{ maxWidth: 1120, margin: '0 auto', padding: 'clamp(16px, 3vw, 24px)' }}>
      {isDirty && (
        <Alert
          type="warning"
          showIcon
          message="Unsaved replication config"
          description="Replication rules are YAML-backed storage config. Review the section diff before applying."
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
          icon={<SyncOutlined />}
          title="Object replication"
          description="Copy object data between buckets or prefixes through the DeltaGlider engine, preserving encryption and compression transparency."
        />
        <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap', marginTop: 14, alignItems: 'center' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <Switch checked={replication.enabled} onChange={(v) => updateConfig({ enabled: v })} />
            <Text strong>Automatic scheduler</Text>
          </label>
          <Text type="secondary" style={{ fontSize: 12 }}>
            Uses sane defaults: tick {replication.tick_interval}, failover {replication.lease_ttl}, heartbeat {replication.heartbeat_interval}.
          </Text>
          <Button type="primary" onClick={runApply} disabled={!isDirty} loading={applying}>
            Review apply
          </Button>
        </div>
        <AdvancedDisclosure title="Advanced scheduler settings">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
            <Field label="Scheduler tick">
              <Input
                value={replication.tick_interval}
                onChange={(e) => updateConfig({ tick_interval: e.target.value })}
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                How often the scheduler checks for due rules. Default: <Text code>30s</Text>.
              </Text>
            </Field>
            <Field label="Lease TTL">
              <Input
                value={replication.lease_ttl}
                onChange={(e) => updateConfig({ lease_ttl: e.target.value })}
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                Dead-runner failover window. Default: <Text code>60s</Text>.
              </Text>
            </Field>
            <Field label="Heartbeat">
              <Input
                value={replication.heartbeat_interval}
                onChange={(e) => updateConfig({ heartbeat_interval: e.target.value })}
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                Lease renewal cadence. Default: <Text code>20s</Text>.
              </Text>
            </Field>
            <Field label="Failures retained">
              <InputNumber
                value={replication.max_failures_retained}
                onChange={(v) => updateConfig({ max_failures_retained: v ?? 100 })}
                min={1}
                max={10000}
                style={{ width: '100%', ...inputRadius }}
              />
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                Per-rule failure history kept in the config DB. Default: <Text code>100</Text>.
              </Text>
            </Field>
          </div>
        </AdvancedDisclosure>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: '320px minmax(0, 1fr)', gap: 16 }}>
        <div style={cardStyle}>
          <SectionHeader
            icon={<SyncOutlined />}
            title="Rules"
            description={loading ? 'Loading...' : `${replication.rules.length} configured rule${replication.rules.length === 1 ? '' : 's'}.`}
          />
          <div style={{ marginTop: 12, display: 'flex', flexDirection: 'column', gap: 8 }}>
            {replication.rules.map((rule) => {
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
                    <Tag color={statusTone(runtime?.last_status || 'idle', runtime?.paused || false, rule.enabled)}>
                      {runtime?.paused ? 'paused' : rule.enabled ? runtime?.last_status || 'idle' : 'disabled'}
                    </Tag>
                  </div>
                  <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 4 }}>
                    {rule.source.bucket || 'source'} / {rule.source.prefix || 'all'} → {rule.destination.bucket || 'destination'} / {rule.destination.prefix || 'same'}
                  </Text>
                  <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
                    Every {rule.interval || '—'} · {rule.conflict}
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
            <EmptyReplicationState onAdd={addRule} />
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
                  type="primary"
                  icon={<RocketOutlined />}
                  disabled={!canRun}
                  title={runReason}
                  loading={actionLoading === `run:${selectedRule.name}`}
                  onClick={() => void runAction(selectedRule.name, 'run')}
                >
                  Run now
                </Button>
                {selectedRuntime?.paused ? (
                  <Button
                    icon={<PlayCircleOutlined />}
                    loading={actionLoading === `resume:${selectedRule.name}`}
                    onClick={() => void runAction(selectedRule.name, 'resume')}
                  >
                    Resume
                  </Button>
                ) : (
                  <Button
                    icon={<PauseCircleOutlined />}
                    loading={actionLoading === `pause:${selectedRule.name}`}
                    onClick={() => void runAction(selectedRule.name, 'pause')}
                  >
                    Pause
                  </Button>
                )}
                <Button danger onClick={() => removeRule(selectedRule.name)}>
                  Remove rule
                </Button>
              </div>

              <RuntimeDetails history={history} failures={failures} />
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
          pendingBody?.replication ? (
            <ReplicationApplySummary replication={pendingBody.replication} />
          ) : undefined
        }
      />
    </div>
  );
}

function ReplicationApplySummary({ replication }: { replication: ReplicationConfig }) {
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
      <Text strong>Replication plan</Text>
      <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 4 }}>
        Scheduler {replication.enabled ? 'enabled' : 'disabled'} · tick {replication.tick_interval} · lease {replication.lease_ttl} · heartbeat {replication.heartbeat_interval} · {replication.rules.length} rule{replication.rules.length === 1 ? '' : 's'}
      </Text>
      <div style={{ marginTop: 10, display: 'flex', flexDirection: 'column', gap: 8 }}>
        {replication.rules.length === 0 ? (
          <Text type="secondary" style={{ fontSize: 12 }}>No rules configured.</Text>
        ) : replication.rules.map((rule) => (
          <div key={rule.name} style={{ fontSize: 12, lineHeight: 1.6 }}>
            <Text code>{rule.name}</Text>{' '}
            <Tag color={rule.enabled ? 'success' : 'default'}>{rule.enabled ? 'enabled' : 'disabled'}</Tag>
            <div>
              <Text strong>Source:</Text> {rule.source.bucket}/{rule.source.prefix || '*'} →{' '}
              <Text strong>Destination:</Text> {rule.destination.bucket}/{rule.destination.prefix || '(same key)'}
            </div>
            <div>
              Conflict: <Text code>{rule.conflict}</Text> · every {rule.interval} · batch {rule.batch_size} · deletes {rule.replicate_deletes ? 'replicated' : 'not replicated'}
            </div>
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

function EmptyReplicationState({ onAdd }: { onAdd: () => void }) {
  return (
    <div style={{ textAlign: 'center', padding: '48px 24px' }}>
      <SyncOutlined style={{ fontSize: 32, opacity: 0.6 }} />
      <div style={{ marginTop: 12 }}><Text strong>No object replication rules</Text></div>
      <Text type="secondary" style={{ display: 'block', marginTop: 6 }}>
        Add a source → destination rule to copy object data through the engine.
      </Text>
      <Space style={{ marginTop: 16 }}>
        <Button type="primary" icon={<PlusOutlined />} onClick={onAdd}>
          Add replication rule
        </Button>
        <Button href="/_/docs/reference-replication">
          Read replication docs
        </Button>
      </Space>
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
  rule: ReplicationRuleConfig;
  runtime: ReplicationRuleOverview | null;
  buckets: string[];
  inputRadius: { borderRadius: number };
  onChange: (patch: Partial<ReplicationRuleConfig>) => void;
  onRename: (nextName: string) => void;
}) {
  return (
    <div>
      <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12, alignItems: 'flex-start' }}>
        <div>
          <Text strong style={{ fontSize: 16 }}>{rule.name}</Text>
          <Text type="secondary" style={{ display: 'block', fontSize: 12, marginTop: 2 }}>
            Last run: {fmtUnix(runtime?.last_run_at)} · Lifetime copied: {formatBytes(runtime?.bytes_copied_lifetime || 0)}
          </Text>
        </div>
        <Tag color={statusTone(runtime?.last_status || 'idle', runtime?.paused || false, rule.enabled)}>
          {runtime?.paused ? 'paused' : rule.enabled ? runtime?.last_status || 'idle' : 'disabled'}
        </Tag>
      </div>

      <div style={{ marginTop: 16, display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        <Field label="Rule name">
          <Input
            value={rule.name}
            onChange={(e) => onRename(e.target.value.replace(/[^A-Za-z0-9_.-]/g, '').slice(0, 64))}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </Field>
        <Field label="Enabled">
          <Switch checked={rule.enabled} onChange={(enabled) => onChange({ enabled })} />
        </Field>
        <Field label="Source bucket">
          <SimpleAutoComplete
            value={rule.source.bucket}
            onChange={(bucket) => onChange({ source: { ...rule.source, bucket } })}
            options={buckets}
            placeholder="prod-artifacts"
          />
        </Field>
        <Field label="Source prefix">
          <Input
            value={rule.source.prefix}
            onChange={(e) => onChange({ source: { ...rule.source, prefix: e.target.value } })}
            onBlur={(e) => onChange({ source: { ...rule.source, prefix: normalizePrefix(e.target.value) } })}
            placeholder="ror/builds"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          <PrefixHelp value={rule.source.prefix} />
        </Field>
        <Field label="Destination bucket">
          <SimpleAutoComplete
            value={rule.destination.bucket}
            onChange={(bucket) => onChange({ destination: { ...rule.destination, bucket } })}
            options={buckets}
            placeholder="backup-artifacts"
          />
        </Field>
        <Field label="Destination prefix">
          <Input
            value={rule.destination.prefix}
            onChange={(e) => onChange({ destination: { ...rule.destination, prefix: e.target.value } })}
            onBlur={(e) => onChange({ destination: { ...rule.destination, prefix: normalizePrefix(e.target.value) } })}
            placeholder="lol"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
          <PrefixHelp value={rule.destination.prefix} />
        </Field>
      </div>

      <AdvancedDisclosure title="Advanced rule behavior">
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
          <Field label="Interval">
            <Input
              value={rule.interval}
              onChange={(e) => onChange({ interval: e.target.value })}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </Field>
          <Field label="Batch size">
            <InputNumber
              value={rule.batch_size}
              onChange={(batch_size) => onChange({ batch_size: batch_size || 100 })}
              min={1}
              max={10000}
              style={{ width: '100%', ...inputRadius }}
            />
          </Field>
          <Field label="Conflict policy">
            <Radio.Group
              value={rule.conflict}
              onChange={(e) => onChange({ conflict: e.target.value })}
              style={{ display: 'flex', flexDirection: 'column', gap: 6 }}
            >
              <Radio value="newer-wins">Newer wins — safest default</Radio>
              <Radio value="source-wins">Source wins — overwrite destination</Radio>
              <Radio value="skip-if-dest-exists">Skip existing destination objects</Radio>
            </Radio.Group>
          </Field>
          <Field label="Delete replication">
            <Switch
              checked={rule.replicate_deletes}
              onChange={(replicate_deletes) => onChange({ replicate_deletes })}
            />
            <Alert
              type="warning"
              showIcon
              message="Deletes are destructive"
              description="When enabled, destination objects previously written by this rule are deleted if the corresponding source key disappears. Manually-created destination objects are preserved."
              style={{ marginTop: 8 }}
            />
          </Field>
          <Field label="Include globs">
            <Input.TextArea
              value={lines(rule.include_globs)}
              onChange={(e) => onChange({ include_globs: lineList(e.target.value) })}
              rows={3}
              placeholder={'*.zip\nreleases/**'}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </Field>
          <Field label="Exclude globs">
            <Input.TextArea
              value={lines(rule.exclude_globs)}
              onChange={(e) => onChange({ exclude_globs: lineList(e.target.value) })}
              rows={3}
              placeholder=".dg/*"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </Field>
        </div>
      </AdvancedDisclosure>

      <Text type="secondary" style={{ display: 'block', marginTop: 12, fontSize: 11 }}>
        Replication rules target bucket names. Bucket policies decide backend routing and encryption.
      </Text>
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

function RuntimeDetails({
  history,
  failures,
}: {
  history: ReplicationHistoryEntry[];
  failures: ReplicationFailureEntry[];
}) {
  const latestRunId = history[0]?.id ?? null;
  const latestRunFailures = latestRunId == null
    ? []
    : failures.filter((failure) => failure.run_id === latestRunId);
  const olderFailures = latestRunId == null
    ? failures
    : failures.filter((failure) => failure.run_id !== latestRunId);

  return (
    <div style={{ marginTop: 18, display: 'flex', flexDirection: 'column', gap: 14 }}>
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
              {fmtUnix(run.started_at)} · copied {run.objects_copied}/{run.objects_scanned} · errors {run.errors}
            </div>
          ))}
        </div>
      </div>

      <div>
        <FailureSection
          title={latestRunId == null ? 'Failures' : `Failures from latest run #${latestRunId}`}
          failures={latestRunFailures}
          emptyText={latestRunId == null ? 'No failures recorded.' : 'No failures recorded for the latest run.'}
          prominent
        />
        {olderFailures.length > 0 && (
          <div style={{ marginTop: 14 }}>
            <FailureSection
              title="Older failures"
              failures={olderFailures}
              emptyText="No older failures."
            />
          </div>
        )}
      </div>
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
  failures: ReplicationFailureEntry[];
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
          description={
            <span>
              Latest failed copy: <Text code>{failures[0].source_key}</Text> →{' '}
              <Text code>{failures[0].dest_key}</Text>
            </span>
          }
        />
      )}
      <div style={{ marginTop: 8, display: 'grid', gridTemplateColumns: '1fr', gap: 8, maxHeight: 420, overflow: 'auto' }}>
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
              gridTemplateColumns: '180px minmax(0, 1fr) minmax(220px, 0.8fr)',
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
              <Text code>{failure.source_key || '(operation)'}</Text> → <Text code>{failure.dest_key || '(none)'}</Text>
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
