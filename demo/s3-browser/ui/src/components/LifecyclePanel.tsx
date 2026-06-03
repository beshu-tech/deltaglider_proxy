import { useCallback, useEffect, useState } from 'react';
import {
  Alert,
  Button,
  Input,
  InputNumber,
  Modal,
  Switch,
  Tag,
  Typography,
  message,
} from 'antd';
import {
  ClockCircleOutlined,
  DeleteOutlined,
  EyeOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import type {
  LifecycleConfig,
  LifecycleFailureEntry,
  LifecycleHistoryEntry,
  LifecycleRuleConfig,
  LifecycleRuleOverview,
  LifecycleRunOutcome,
  StorageSectionBody,
} from '../adminApi';
import {
  getLifecycleFailures,
  getLifecycleHistory,
  getLifecycleOverview,
  previewLifecycleRule,
  runLifecycleNow,
} from '../adminApi';
import { listBuckets } from '../s3client';
import { useApplyHandler } from '../useDirtySection';
import { useSectionEditor } from '../useSectionEditor';
import { formatBytes } from '../utils';
import ApplyDialog from './ApplyDialog';
import FormField from './FormField';
import StickyDirtyBar from './StickyDirtyBar';
import { AdvancedDisclosure } from './ruleEditorFields';
import { formRow } from './ruleEditorHelpers';
import RuleListEditor, { RuleRowLine, RuleRowTitle } from './RuleListEditor';
import SectionHeader from './SectionHeader';
import { useCardStyles } from './shared-styles';
import {
  actionKind,
  actionLabel,
  buildLifecyclePayload,
  DEFAULT_LIFECYCLE,
  emptyRule,
  normalizeLifecycle,
} from './lifecyclePayload';
import { statusTone } from './lifecycleHelpers';
import Metric from './LifecycleMetric';
import RuleEditor from './LifecycleRuleFields';
import { PreviewPanel, RuntimeDetails } from './LifecycleRuntimeDetails';
import { EmptyLifecycleState, LifecycleApplySummary } from './LifecycleSummary';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
}

export default function LifecyclePanel({ onSessionExpired }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const {
    value: lifecycle,
    setValue: setLifecycle,
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
  } = useSectionEditor<StorageSectionBody, LifecycleConfig>({
    section: 'storage',
    dirtyKey: 'configuration/storage/lifecycle',
    initial: DEFAULT_LIFECYCLE,
    onSessionExpired,
    noun: 'lifecycle',
    pick: (body) => normalizeLifecycle(body.lifecycle),
    // The guarded `runApply` below blocks the apply on validation
    // failure, so this only runs for a valid config; `{}` is an
    // unreachable type-non-null fallback.
    toPayload: (v) => {
      const res = buildLifecyclePayload(v);
      return res.ok ? res.body : {};
    },
  });

  const [overview, setOverview] = useState<LifecycleRuleOverview[]>([]);
  const [history, setHistory] = useState<LifecycleHistoryEntry[]>([]);
  const [failures, setFailures] = useState<LifecycleFailureEntry[]>([]);
  const [runtimeError, setRuntimeError] = useState<string | null>(null);
  const [buckets, setBuckets] = useState<string[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [previewLoading, setPreviewLoading] = useState(false);
  const [runLoading, setRunLoading] = useState(false);
  const [previews, setPreviews] = useState<Record<string, LifecycleRunOutcome>>({});

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

  // Runtime data (overview + buckets) is NOT part of the storage
  // section body — load it independently. Reloaded after apply / run-now
  // exactly where the old monolithic refresh() did. `selected` tracks
  // the section rules, which the section editor now owns.
  const reloadRuntime = useCallback(async () => {
    const [lifecycleOverview, realBuckets] = await Promise.all([
      getLifecycleOverview().catch(() => null),
      listBuckets().catch(() => [] as Array<{ name: string }>),
    ]);
    setOverview(lifecycleOverview?.rules || []);
    setBuckets(realBuckets.map((b) => b.name));
  }, []);

  useEffect(() => {
    void reloadRuntime();
  }, [reloadRuntime]);

  // Keep `selected` aligned with the rules the section editor loaded.
  useEffect(() => {
    setSelected((cur) => {
      if (cur && lifecycle.rules.some((r) => r.name === cur)) return cur;
      return lifecycle.rules[0]?.name || null;
    });
  }, [lifecycle.rules]);

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

  // Guarded apply: run the client-side validation first and surface the
  // first error, otherwise delegate to the section editor's validate →
  // ApplyDialog → PUT flow (which re-derives the same body via
  // `toPayload`, keeping the validate/PUT body byte-identical).
  const runApply = useCallback(async () => {
    const res = buildLifecyclePayload(lifecycle);
    if (!res.ok) {
      message.error(res.error);
      return;
    }
    await editorRunApply();
  }, [lifecycle, editorRunApply]);

  // On a successful apply, reload runtime overview/buckets and clear
  // stale previews — exactly the side effects the old confirmApply ran.
  const confirmApplyAndReload = useCallback(async () => {
    await confirmApply();
    setPreviews({});
    await reloadRuntime();
  }, [confirmApply, reloadRuntime]);

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
          await reloadRuntime();
        } catch (e) {
          message.error(e instanceof Error ? e.message : 'Run-now failed');
        } finally {
          setRunLoading(false);
        }
      },
    });
  };

  useApplyHandler('configuration/storage/lifecycle', runApply, isDirty);

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
      <div style={{ ...cardStyle, marginBottom: 16 }}>
        <SectionHeader
          icon={<ClockCircleOutlined />}
          title="Object lifecycle"
          description="Delete-only expiration rules. Preview is read-only; run-now is explicit and guarded."
        />
        <div style={formRow(16, { flexWrap: 'wrap' })}>
          <label style={formRow(8)}>
            <Switch checked={lifecycle.enabled} onChange={(enabled) => updateConfig({ enabled })} />
            <Text strong>Automatic scheduler</Text>
          </label>
          <Text type="secondary" style={{ fontSize: 12 }}>
            Disabled by default. Current tick: <Text code>{lifecycle.tick_interval}</Text>.
          </Text>
        </div>
        <AdvancedDisclosure title="Advanced scheduler defaults">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(180px, 1fr))', gap: 12 }}>
            <FormField
              label="Scheduler tick"
              yamlPath="storage.lifecycle.tick_interval"
              helpText="How often the scheduler wakes to check for due rules. Humantime duration; default 1h. The backend warns below 60s."
            >
              <Input
                value={lifecycle.tick_interval}
                onChange={(e) => updateConfig({ tick_interval: e.target.value })}
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
            </FormField>
            <FormField
              label="Failures retained"
              yamlPath="storage.lifecycle.max_failures_retained"
              helpText="Per-run cap on returned failure entries. Also caps preview candidates returned by the API. Default 100."
            >
              <InputNumber
                value={lifecycle.max_failures_retained}
                onChange={(v) => updateConfig({ max_failures_retained: Number(v) || 100 })}
                min={1}
                max={10000}
                style={{ width: '100%', ...inputRadius }}
              />
            </FormField>
          </div>
        </AdvancedDisclosure>
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 10, marginBottom: 16 }}>
        <Metric label="Rules" value={`${enabledRuleCount}/${lifecycle.rules.length} enabled`} />
        <Metric label="Scheduler" value={lifecycle.enabled ? 'enabled' : 'disabled'} tone={lifecycle.enabled ? undefined : 'warning'} />
        <Metric label="Failed rules" value={failedRuleCount} tone={failedRuleCount > 0 ? 'error' : undefined} />
        <Metric label="Lifetime affected" value={`${lifetimeAffected} · ${formatBytes(lifetimeBytes)}`} />
      </div>

      <RuleListEditor
        rules={lifecycle.rules}
        selectedName={selected}
        getName={(rule) => rule.name}
        onSelect={setSelected}
        onAdd={addRule}
        icon={<ClockCircleOutlined />}
        loading={loading}
        listColumn="minmax(260px, 320px)"
        emptyState={<EmptyLifecycleState onAdd={addRule} />}
        renderListItem={(rule) => {
          const runtime = overview.find((r) => r.name === rule.name);
          return (
            <>
              <RuleRowTitle
                name={rule.name}
                status={
                  <Tag color={statusTone(runtime?.last_status || 'idle', rule.enabled)}>
                    {rule.enabled ? runtime?.last_status || 'idle' : 'disabled'}
                  </Tag>
                }
              />
              <RuleRowLine>
                {rule.bucket || 'bucket'} / {rule.prefix || 'all'} · older than {rule.expire_after || '—'}
              </RuleRowLine>
              <RuleRowLine marginTop={2}>
                Lifetime affected: {runtime?.objects_affected_lifetime || 0} objects · {formatBytes(runtime?.bytes_affected_lifetime || 0)}
              </RuleRowLine>
            </>
          );
        }}
        renderDetail={(rule) => (
          <>
            <RuleEditor
              rule={rule}
              runtime={selectedRuntime || null}
              buckets={buckets}
              inputRadius={inputRadius}
              onChange={(patch) => updateRule(rule.name, patch)}
              onRename={(nextName) => {
                updateRule(rule.name, { name: nextName });
                setSelected(nextName);
              }}
            />

            <div style={{ marginTop: 16, display: 'flex', gap: 8, flexWrap: 'wrap' }}>
              <Button
                icon={<EyeOutlined />}
                disabled={!canPreview}
                title={canPreview ? 'Preview matching expired objects without deleting.' : runReason}
                loading={previewLoading}
                onClick={() => void previewRule(rule.name)}
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
                onClick={() => selectedPreview && confirmRunNow(rule, selectedPreview)}
              >
                Run {actionKind(rule.action) === 'transition' ? 'transition' : 'delete'} now
              </Button>
              <Button danger onClick={() => confirmRemoveRule(rule.name)}>
                Remove rule
              </Button>
            </div>

            <PreviewPanel outcome={selectedPreview} maxCandidates={lifecycle.max_failures_retained} />
            <RuntimeDetails history={history} failures={failures} runtimeError={runtimeError} />
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
          pendingBody?.lifecycle ? (
            <LifecycleApplySummary lifecycle={pendingBody.lifecycle} />
          ) : undefined
        }
      />
    </div>
  );
}


