import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
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
} from '@ant-design/icons';
import type {
  ReplicationConfig,
  ReplicationFailureEntry,
  ReplicationHistoryEntry,
  ReplicationRuleConfig,
  ReplicationRuleOverview,
  StorageSectionBody,
} from '../adminApi';
import {
  getReplicationFailures,
  getReplicationHistory,
  getReplicationOverview,
  pauseReplicationRule,
  resumeReplicationRule,
  runReplicationNow,
} from '../adminApi';
import { listBuckets } from '../s3client';
import { useCardStyles } from './shared-styles';
import RuleListEditor, { RuleRowLine, RuleRowTitle } from './RuleListEditor';
import SectionHeader from './SectionHeader';
import ApplyDialog from './ApplyDialog';
import FormField from './FormField';
import StickyDirtyBar from './StickyDirtyBar';
import { AdvancedDisclosure } from './ruleEditorFields';
import { useApplyHandler } from '../useDirtySection';
import { useSectionEditor } from '../useSectionEditor';
import {
  buildReplicationPayload,
  DEFAULT_REPLICATION,
  emptyRule,
  normalizeReplication,
} from './replicationPayload';
import ReplicationRuleFields from './ReplicationRuleFields';
import ReplicationRuntimeDetails from './ReplicationRuntimeDetails';
import ReplicationApplySummary from './ReplicationApplySummary';
import { statusTone } from './replicationStatus';
import { EmptyState } from './StatePlaceholders';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function ReplicationPanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const {
    value: replication,
    setValue: setReplication,
    isDirty,
    discard,
    loading,
    error,
    applyOpen,
    applyResponse,
    applying,
    pendingBody,
    runApply: editorRunApply,
    cancelApply,
    confirmApply,
  } = useSectionEditor<StorageSectionBody, ReplicationConfig>({
    section: 'storage',
    dirtyKey: 'configuration/storage/replication',
    initial: DEFAULT_REPLICATION,
    onSessionExpired,
    noun: 'replication',
    pick: (body) => normalizeReplication(body.replication),
    // The guarded `runApply` below blocks the apply on validation
    // failure, so this only runs for a valid config; `{}` is an
    // unreachable type-non-null fallback.
    toPayload: (v) => {
      const res = buildReplicationPayload(v);
      return res.ok ? res.body : {};
    },
  });

  const [overview, setOverview] = useState<ReplicationRuleOverview[]>([]);
  const [history, setHistory] = useState<ReplicationHistoryEntry[]>([]);
  const [failures, setFailures] = useState<ReplicationFailureEntry[]>([]);
  const [buckets, setBuckets] = useState<string[]>([]);
  // Selection is by ARRAY INDEX, not name: a rule's name is edited live and
  // isn't unique mid-rename, so name-keyed selection could retarget the wrong
  // rule. Index is stable across renames; add/remove reset it below.
  const [selectedIndex, setSelectedIndex] = useState<number | null>(null);
  const [actionLoading, setActionLoading] = useState<string | null>(null);

  const selectedRule =
    selectedIndex != null && selectedIndex >= 0 && selectedIndex < replication.rules.length
      ? replication.rules[selectedIndex]
      : replication.rules[0] || null;
  const selectedRuleName = selectedRule?.name;
  const selectedRuntime = selectedRule
    ? overview.find((r) => r.name === selectedRule.name)
    : null;

  // Runtime data (overview + buckets) is NOT part of the storage section
  // body — load it independently. Reloaded after apply / run-now exactly
  // where the old monolithic refresh() did.
  const reloadRuntime = useCallback(async () => {
    const [repl, realBuckets] = await Promise.all([
      getReplicationOverview().catch(() => null),
      listBuckets().catch(() => [] as Array<{ name: string }>),
    ]);
    setOverview(repl?.rules || []);
    setBuckets(realBuckets.map((b) => b.name));
  }, []);

  useEffect(() => {
    void reloadRuntime();
  }, [reloadRuntime]);

  // Keep `selectedIndex` in range as the section editor (re)loads rules.
  // Clamp out-of-range to the first rule; null when the list is empty.
  useEffect(() => {
    setSelectedIndex((cur) => {
      if (replication.rules.length === 0) return null;
      if (cur != null && cur >= 0 && cur < replication.rules.length) return cur;
      return 0;
    });
  }, [replication.rules]);

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

  // Mutations target the rule by INDEX so a live rename (which changes the
  // name) can't retarget or collide with another rule.
  const updateRule = (index: number, patch: Partial<ReplicationRuleConfig>) => {
    setReplication({
      ...replication,
      rules: replication.rules.map((rule, i) => (i === index ? { ...rule, ...patch } : rule)),
    });
  };

  const addRule = () => {
    const rule = emptyRule(replication.rules);
    const next = [...replication.rules, rule];
    setReplication({ ...replication, rules: next });
    setSelectedIndex(next.length - 1);
  };

  const removeRule = (index: number) => {
    const next = replication.rules.filter((_, i) => i !== index);
    setReplication({ ...replication, rules: next });
    setSelectedIndex(next.length > 0 ? Math.min(index, next.length - 1) : null);
  };

  // Guarded apply: run the client-side validation first and surface the
  // first error, otherwise delegate to the section editor's validate →
  // ApplyDialog → PUT flow (which re-derives the same body via
  // `toPayload`, keeping the validate/PUT body byte-identical).
  const runApply = useCallback(async () => {
    const res = buildReplicationPayload(replication);
    if (!res.ok) {
      message.error(res.error);
      return;
    }
    await editorRunApply();
  }, [replication, editorRunApply]);

  // On a successful apply, reload runtime overview/buckets — the same
  // side effect the old confirmApply ran via its bundled refresh().
  const confirmApplyAndReload = useCallback(async () => {
    await confirmApply();
    await reloadRuntime();
  }, [confirmApply, reloadRuntime]);

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
      await reloadRuntime();
    } catch (e) {
      message.error(e instanceof Error ? e.message : `${action} failed`);
    } finally {
      setActionLoading(null);
    }
  };

  useApplyHandler('configuration/storage/replication', runApply, isDirty);

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
      <div style={{ ...cardStyle, marginBottom: 16 }}>
        <SectionHeader
          icon={<SyncOutlined />}
          title="Object replication"
          description="Copy object data between buckets or prefixes through the DeltaGlider engine, preserving encryption and compression transparency."
        />
        <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap', alignItems: 'center' }}>
          <label style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <Switch checked={replication.enabled} onChange={(v) => updateConfig({ enabled: v })} />
            <Text strong>Automatic scheduler</Text>
          </label>
          <Text type="secondary" style={{ fontSize: 12 }}>
            Uses sane defaults: tick {replication.tick_interval}, failover {replication.lease_ttl}, heartbeat {replication.heartbeat_interval}.
          </Text>
        </div>
        <AdvancedDisclosure title="Advanced scheduler settings">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
            <FormField
              label="Scheduler tick"
              yamlPath="storage.replication.tick_interval"
              helpText="How often the scheduler wakes to check for due rules. Humantime duration; default 30s (5s minimum)."
            >
              <Input
                value={replication.tick_interval}
                onChange={(e) => updateConfig({ tick_interval: e.target.value })}
                placeholder="30s"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
            </FormField>
            <FormField
              label="Lease TTL"
              yamlPath="storage.replication.lease_ttl"
              helpText="Single-flight lease lifetime. A dead runner's rule can be picked up by another instance after this expires. Default 60s."
            >
              <Input
                value={replication.lease_ttl}
                onChange={(e) => updateConfig({ lease_ttl: e.target.value })}
                placeholder="60s"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
            </FormField>
            <FormField
              label="Heartbeat"
              yamlPath="storage.replication.heartbeat_interval"
              helpText="How often a long-running rule renews its lease while copying. Default 20s."
            >
              <Input
                value={replication.heartbeat_interval}
                onChange={(e) => updateConfig({ heartbeat_interval: e.target.value })}
                placeholder="20s"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
            </FormField>
            <FormField
              label="Failures retained"
              yamlPath="storage.replication.max_failures_retained"
              helpText="Per-rule failure ring bound kept in the config DB. Older entries are dropped past this. Default 100."
            >
              <InputNumber
                value={replication.max_failures_retained}
                onChange={(v) => updateConfig({ max_failures_retained: v ?? 100 })}
                min={1}
                max={10000}
                style={{ width: '100%', ...inputRadius }}
              />
            </FormField>
          </div>
        </AdvancedDisclosure>
      </div>

      <RuleListEditor
        rules={replication.rules}
        selectedIndex={selectedIndex}
        onSelect={setSelectedIndex}
        onAdd={addRule}
        icon={<SyncOutlined />}
        loading={loading}
        emptyState={<EmptyReplicationState onAdd={addRule} />}
        renderListItem={(rule) => {
          const runtime = overview.find((r) => r.name === rule.name);
          return (
            <>
              <RuleRowTitle
                name={rule.name}
                status={
                  <Tag color={statusTone(runtime?.last_status || 'idle', runtime?.paused || false, rule.enabled)}>
                    {runtime?.paused ? 'paused' : rule.enabled ? runtime?.last_status || 'idle' : 'disabled'}
                  </Tag>
                }
              />
              <RuleRowLine>
                {rule.source.bucket || 'source'} / {rule.source.prefix || 'all'} → {rule.destination.bucket || 'destination'} / {rule.destination.prefix || 'same'}
              </RuleRowLine>
              <RuleRowLine marginTop={2}>
                Every {rule.interval || '—'} · {rule.conflict}
              </RuleRowLine>
            </>
          );
        }}
        renderDetail={(rule, index) => (
          <>
            <ReplicationRuleFields
              rule={rule}
              runtime={selectedRuntime || null}
              buckets={buckets}
              inputRadius={inputRadius}
              onChange={(patch) => updateRule(index, patch)}
              onRename={(nextName) => updateRule(index, { name: nextName })}
            />

            <div style={{ marginTop: 16, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
              <Button
                type="primary"
                icon={<RocketOutlined />}
                disabled={!canRun}
                title={runReason}
                loading={actionLoading === `run:${rule.name}`}
                onClick={() => void runAction(rule.name, 'run')}
              >
                Run now
              </Button>
              {selectedRuntime?.paused ? (
                <Button
                  icon={<PlayCircleOutlined />}
                  loading={actionLoading === `resume:${rule.name}`}
                  onClick={() => void runAction(rule.name, 'resume')}
                >
                  Resume
                </Button>
              ) : (
                <Button
                  icon={<PauseCircleOutlined />}
                  loading={actionLoading === `pause:${rule.name}`}
                  onClick={() => void runAction(rule.name, 'pause')}
                >
                  Pause
                </Button>
              )}
              <Button danger onClick={() => removeRule(index)}>
                Remove rule
              </Button>
            </div>

            {isDirty && (
              <Alert
                type="info"
                showIcon
                style={{ marginTop: 16 }}
                message="Runtime stats reflect the applied configuration"
                description="The status, recent runs, and failures below are for the last applied config — not your pending edits. Apply to refresh them."
              />
            )}

            <ReplicationRuntimeDetails history={history} failures={failures} />
          </>
        )}
      />

      <StickyDirtyBar
        visible={isDirty}
        applying={applying}
        onDiscard={discard}
        onApply={runApply}
        floating
      />

      <ApplyDialog
        open={applyOpen}
        section="storage"
        response={applyResponse}
        onApply={confirmApplyAndReload}
        onCancel={cancelApply}
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

function EmptyReplicationState({ onAdd }: { onAdd: () => void }) {
  return (
    <EmptyState
      icon={<SyncOutlined />}
      title="No object replication rules"
      hint="Add a source → destination rule to copy object data through the engine."
      action={
        <Space>
          <Button type="primary" icon={<PlusOutlined />} onClick={onAdd}>
            Add replication rule
          </Button>
          <Button href="/_/docs/reference-replication">
            Read replication docs
          </Button>
        </Space>
      }
    />
  );
}
