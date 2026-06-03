import { Alert, Input, InputNumber, Switch, Tag, Typography } from 'antd';
import type {
  LifecycleRuleConfig,
  LifecycleRuleOverview,
} from '../adminApi';
import BucketPrefixInput from './BucketPrefixInput';
import FormField from './FormField';
import { AdvancedDisclosure } from './ruleEditorFields';
import { fmtUnix, lineList, lines } from './ruleEditorHelpers';
import SimpleSelect from './SimpleSelect';
import { actionKind } from './lifecyclePayload';
import { statusTone } from './lifecycleHelpers';

const { Text } = Typography;

/**
 * Per-rule field editor body for the Lifecycle panel. Passed as the
 * `renderDetail` content to RuleListEditor by the parent LifecyclePanel.
 */
export default function RuleEditor({
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

      <div style={{ marginTop: 16, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 14 }}>
        <FormField
          label="Rule name"
          yamlPath="storage.lifecycle.rules[].name"
          helpText="Unique identifier for this rule. ASCII letters, digits, dot, dash, underscore; max 64 chars."
        >
          <Input
            value={rule.name}
            onChange={(e) => onRename(e.target.value.replace(/[^A-Za-z0-9_.-]/g, '').slice(0, 64))}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </FormField>
        <FormField
          label="Enabled"
          yamlPath="storage.lifecycle.rules[].enabled"
          helpText="Per-rule delete switch. The global scheduler must also be enabled for this rule to run automatically."
        >
          <Switch checked={rule.enabled} onChange={(enabled) => onChange({ enabled })} />
        </FormField>
        <FormField
          label="Scope"
          yamlPath="storage.lifecycle.rules[].bucket"
          helpText="Bucket and optional prefix to scan for expired objects. An empty prefix scans the whole bucket."
        >
          <BucketPrefixInput
            value={{ bucket: rule.bucket, prefix: rule.prefix }}
            onChange={(scope) => onChange({ bucket: scope.bucket, prefix: scope.prefix })}
            buckets={buckets}
            bucketPlaceholder="prod-artifacts"
            prefixPlaceholder="builds/releases/"
          />
        </FormField>
        <FormField
          label="Expire after"
          yamlPath="storage.lifecycle.rules[].expire_after"
          helpText="Objects whose created_at is older than this age become candidates. Humantime duration, e.g. 30d, 12h, 90d."
        >
          <Input
            value={rule.expire_after}
            onChange={(e) => onChange({ expire_after: e.target.value })}
            placeholder="30d"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </FormField>
        <FormField
          label="Action"
          yamlPath="storage.lifecycle.rules[].action"
          helpText="What to do with expired candidates: delete them, or archive/move them through the engine to another bucket."
        >
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
        </FormField>
      </div>

      {transitionAction && (
        <div style={{ marginTop: 14, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 14 }}>
          <FormField
            label="Destination"
            yamlPath="storage.lifecycle.rules[].action.destination"
            helpText="Bucket and prefix that archived objects are copied into."
          >
            <BucketPrefixInput
              value={{
                bucket: transitionAction.destination?.bucket || '',
                prefix: transitionAction.destination?.prefix || '',
              }}
              onChange={(destination) => updateTransition({ destination })}
              buckets={buckets}
              bucketPlaceholder="archive-artifacts"
              prefixPlaceholder="archive/releases/"
            />
          </FormField>
          <FormField
            label="Delete source after copy"
            yamlPath="storage.lifecycle.rules[].action.delete_source_after_success"
            helpText="Off archives by copying only. On makes it a move — the source is deleted, but only after the destination copy verifies."
          >
            <Switch
              checked={Boolean(transitionAction.delete_source_after_success)}
              onChange={(checked) => updateTransition({ delete_source_after_success: checked })}
            />
          </FormField>
        </div>
      )}

      <AdvancedDisclosure title="Filters and batch size">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 14 }}>
          <FormField
            label="Include globs"
            yamlPath="storage.lifecycle.rules[].include_globs"
            helpText="One glob per line. If non-empty, only matching keys are candidates. Empty means every key under the prefix."
          >
            <Input.TextArea
              value={lines(rule.include_globs)}
              onChange={(e) => onChange({ include_globs: lineList(e.target.value) })}
              rows={3}
              placeholder={'*.zip\nreleases/**'}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </FormField>
          <FormField
            label="Exclude globs"
            yamlPath="storage.lifecycle.rules[].exclude_globs"
            helpText="One glob per line. Keys matching any pattern are skipped. Defaults protect DeltaGlider's config-sync prefix."
          >
            <Input.TextArea
              value={lines(rule.exclude_globs)}
              onChange={(e) => onChange({ exclude_globs: lineList(e.target.value) })}
              rows={3}
              placeholder=".deltaglider/**"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </FormField>
          <FormField
            label="Batch size"
            yamlPath="storage.lifecycle.rules[].batch_size"
            helpText="Objects per listing page / worker batch. Default 100."
          >
            <InputNumber
              value={rule.batch_size}
              onChange={(batch_size) => onChange({ batch_size: Number(batch_size) || 100 })}
              min={1}
              max={10000}
              style={{ width: '100%', ...inputRadius }}
            />
          </FormField>
        </div>
      </AdvancedDisclosure>
    </div>
  );
}
