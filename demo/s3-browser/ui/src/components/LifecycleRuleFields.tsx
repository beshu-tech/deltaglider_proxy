import { Alert, Input, InputNumber, Select, Switch, Typography } from 'antd';
import type { LifecycleAction, LifecycleRuleConfig } from '../adminApi';
import BucketPrefixInput from './BucketPrefixInput';
import FormField from './FormField';
import { AdvancedDisclosure } from './ruleEditorFields';
import RuleFilterFields from './RuleFilterFields';
import { ruleFolderPrefix } from '../ruleFilterPreview';
import { actionKind } from './lifecyclePayload';
import { ruleGlobError } from './jobs/lifecycleText';

const { Text } = Typography;

type RetainAction = Extract<LifecycleAction, { type: 'retain-newest' }>;
type TransitionAction = Extract<LifecycleAction, { type: 'transition' | 'archive' }>;


/**
 * Per-rule field editor body for the Lifecycle panel. Passed as the
 * Definition-tab content of the Jobs drawer; the parent owns state.
 */
export default function RuleEditor({
  rule,
  buckets,
  inputRadius,
  onChange,
  onRename,
}: {
  rule: LifecycleRuleConfig;
  buckets: string[];
  inputRadius: { borderRadius: number };
  onChange: (patch: Partial<LifecycleRuleConfig>) => void;
  onRename: (nextName: string) => void;
}) {
  const kind = actionKind(rule.action);
  const transitionAction =
    kind === 'transition' && typeof rule.action === 'object'
      ? (rule.action as TransitionAction)
      : null;
  const retainAction =
    kind === 'retain-newest' && typeof rule.action === 'object'
      ? (rule.action as RetainAction)
      : null;
  const updateTransition = (patch: Partial<TransitionAction>) => {
    const current: TransitionAction = transitionAction || {
      type: 'transition',
      destination: { bucket: '', prefix: 'archive/' },
      delete_source_after_success: false,
    };
    onChange({ action: { ...current, ...patch } });
  };
  const updateRetain = (patch: Partial<RetainAction>) => {
    const current: RetainAction = retainAction || { type: 'retain-newest', count: 2 };
    onChange({ action: { ...current, ...patch } });
  };
  const updateQualify = (patch: { min_size_bytes?: number; min_age?: string }) => {
    const current: RetainAction = retainAction || { type: 'retain-newest', count: 2 };
    updateRetain({ qualify: { ...current.qualify, ...patch } });
  };

  const includeError = ruleGlobError(rule.name, rule.include_globs);
  const excludeError = ruleGlobError(rule.name, rule.exclude_globs);

  return (
    <div>
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
          helpText="Per-rule switch. The rule runs only when “Run lifecycle rules on schedule”, at the top of this page, is also on."
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
        {kind !== 'retain-newest' && (
          <FormField
            label="Expire after"
            yamlPath="storage.lifecycle.rules[].expire_after"
            helpText="Objects whose creation time is older than this age become candidates. The proxy stamps the creation time when it stores an object. For an object that was written without the proxy (a restore, a copy straight to the disk or the backend), it uses the file's modification time instead. A duration such as 12h, 30d or 90d."
          >
            <Input
              value={rule.expire_after || ''}
              onChange={(e) => onChange({ expire_after: e.target.value })}
              placeholder="30d"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </FormField>
        )}
        <FormField
          label="Action"
          yamlPath="storage.lifecycle.rules[].action"
          helpText="What to do with candidates. Delete removes them. Keep newest N keeps the newest N objects and deletes the rest. Archive / move copies them to another bucket the same way replication does, and can then delete the source after the copy is verified."
        >
          <Select
            value={kind}
            onChange={(value) => {
              if (value === 'transition') {
                onChange({
                  action: {
                    type: 'transition',
                    destination: { bucket: '', prefix: 'archive/' },
                    delete_source_after_success: false,
                  },
                });
              } else if (value === 'retain-newest') {
                onChange({ action: { type: 'retain-newest', count: 2 } });
              } else {
                onChange({ action: 'delete' });
              }
            }}
            options={[
              { value: 'delete', label: 'Delete', sublabel: 'Expire source objects by age' },
              { value: 'retain-newest', label: 'Keep newest N', sublabel: 'Count-based — keep the latest, delete the rest' },
              { value: 'transition', label: 'Archive / move', sublabel: 'Copy first, optional source delete' },
            ]}
            optionRender={(opt) => (
              <div>
                <div>{opt.data.label}</div>
                {opt.data.sublabel && (
                  <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>{opt.data.sublabel}</Text>
                )}
              </div>
            )}
            style={{ width: '100%', ...inputRadius }}
          />
        </FormField>
      </div>

      {retainAction && (
        <>
          <Alert
            type="info"
            showIcon
            title="Keep the newest N — count-based retention"
            description="Ranks objects newest-first and keeps the latest N qualifying ones; the rest are deleted. The qualify filters below are an eligibility gate, not a delete guard: a file that fails them is ignored entirely — never counted toward N, never deleted — so an accidental empty or half-written file can't push a real backup out of the keep set."
            style={{ marginTop: 14 }}
          />
          <div style={{ marginTop: 14, display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 14 }}>
            <FormField
              label="Keep newest"
              yamlPath="storage.lifecycle.rules[].action.count"
              helpText="How many of the newest qualifying objects to keep. Everything else is deleted. Must be at least 1."
            >
              <InputNumber
                value={retainAction.count}
                onChange={(count) => updateRetain({ count: Math.max(1, Math.floor(Number(count) || 1)) })}
                min={1}
                max={100000}
                style={{ width: '100%', ...inputRadius }}
              />
            </FormField>
            <FormField
              label="Ignore objects smaller than"
              yamlPath="storage.lifecycle.rules[].action.qualify.min_size_bytes"
              helpText="Bytes. Objects below this ORIGINAL size are ignored — never kept, never deleted — so empty/truncated junk can't anchor the keep set. Leave 0 for no size filter."
            >
              <InputNumber
                value={retainAction.qualify?.min_size_bytes ?? 0}
                onChange={(v) => {
                  const n = Math.max(0, Math.floor(Number(v) || 0));
                  updateQualify({ min_size_bytes: n > 0 ? n : undefined });
                }}
                min={0}
                step={1048576}
                style={{ width: '100%', ...inputRadius }}
                suffix="bytes"
              />
            </FormField>
            <FormField
              label="Ignore objects younger than"
              yamlPath="storage.lifecycle.rules[].action.qualify.min_age"
              helpText="A duration such as 1h. The rule ignores objects younger than this, because they can still be uploading. Leave it empty for no age filter."
            >
              <Input
                value={retainAction.qualify?.min_age || ''}
                onChange={(e) => updateQualify({ min_age: e.target.value || undefined })}
                placeholder="1h"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
            </FormField>
          </div>
          <AdvancedDisclosure title="Delete-side guard" hint="Never delete objects younger than an age">
            <FormField
              label="Don't delete anything younger than"
              yamlPath="storage.lifecycle.rules[].action.protect_younger_than"
              helpText="A duration such as 1h. This run does not delete a selected object that is younger than this. The filters above are different: they remove objects from the count. Most rules leave this empty."
            >
              <Input
                value={retainAction.protect_younger_than || ''}
                onChange={(e) => updateRetain({ protect_younger_than: e.target.value || undefined })}
                placeholder="7d"
                style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
              />
            </FormField>
          </AdvancedDisclosure>
        </>
      )}

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

      <RuleFilterFields
        bucket={rule.bucket}
        prefix={ruleFolderPrefix(rule.prefix)}
        include={rule.include_globs}
        exclude={rule.exclude_globs}
        onChange={onChange}
        yamlBase="storage.lifecycle.rules[]"
        verb="are candidates"
        excludeHelp="One glob per line. The rule never acts on a matching object. The proxy never acts on its own .deltaglider/ and .dg/ keys, also when you replace the default pattern."
        includeError={includeError}
        excludeError={excludeError}
        inputRadius={inputRadius}
      />

      <AdvancedDisclosure title="Batch size" hint="Objects per page of a run">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 14 }}>
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
