import { Alert, Input, InputNumber, Modal, Radio, Switch, Typography } from 'antd';
import type { ReplicationRuleConfig } from '../adminApi';
import BucketPrefixInput from './BucketPrefixInput';
import FormField from './FormField';
import { AdvancedDisclosure } from './ruleEditorFields';
import RuleFilterFields from './RuleFilterFields';
import { ruleFolderPrefix } from '../ruleFilterPreview';

const { Text } = Typography;

/**
 * Per-rule field editor body (name / enabled / source / destination + advanced
 * behaviour). Extracted verbatim from ReplicationPanel; the parent renders it
 * inside the Jobs drawer's Definition tab; the parent owns all state.
 */
export default function ReplicationRuleFields({
  rule,
  buckets,
  inputRadius,
  onChange,
  onRename,
}: {
  rule: ReplicationRuleConfig;
  buckets: string[];
  inputRadius: { borderRadius: number };
  onChange: (patch: Partial<ReplicationRuleConfig>) => void;
  onRename: (nextName: string) => void;
}) {
  return (
    <div>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(260px, 1fr))', gap: 14 }}>
        <FormField
          label="Rule name"
          yamlPath="storage.replication.rules[].name"
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
          yamlPath="storage.replication.rules[].enabled"
          helpText="Per-rule switch. The rule runs on its own only while replication is on for the proxy: storage.replication.enabled, which is on by default and is set only in YAML."
        >
          <Switch checked={rule.enabled} onChange={(enabled) => onChange({ enabled })} />
        </FormField>
        <FormField
          label="Source"
          yamlPath="storage.replication.rules[].source"
          helpText="Bucket and optional prefix to copy objects from."
        >
          <BucketPrefixInput
            value={rule.source}
            onChange={(source) => onChange({ source })}
            buckets={buckets}
            bucketPlaceholder="prod-artifacts"
            prefixPlaceholder="builds/releases/"
          />
        </FormField>
        <FormField
          label="Destination"
          yamlPath="storage.replication.rules[].destination"
          helpText="Bucket and optional prefix to copy objects into. An empty prefix mirrors the source key path."
        >
          <BucketPrefixInput
            value={rule.destination}
            onChange={(destination) => onChange({ destination })}
            buckets={buckets}
            bucketPlaceholder="backup-artifacts"
            prefixPlaceholder="mirror/releases/"
          />
        </FormField>
      </div>

      <RuleFilterFields
        bucket={rule.source.bucket}
        prefix={ruleFolderPrefix(rule.source.prefix)}
        include={rule.include_globs}
        exclude={rule.exclude_globs}
        onChange={onChange}
        yamlBase="storage.replication.rules[]"
        verb="replicate"
        excludeHelp="One glob per line. A matching object is not copied, and a run never deletes its copy on the destination."
        inputRadius={inputRadius}
      />

      <AdvancedDisclosure title="Advanced rule behavior">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 14 }}>
          <FormField
            label="Interval"
            yamlPath="storage.replication.rules[].interval"
            helpText="How often the full reconcile sweep runs for this rule. Events replicate changes as they happen; the sweep catches anything an event missed. Humantime duration, e.g. 6h or 24h (the default). Minimum 30s."
          >
            <Input
              value={rule.interval}
              onChange={(e) => onChange({ interval: e.target.value })}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </FormField>
          <FormField
            label="Batch size"
            yamlPath="storage.replication.rules[].batch_size"
            helpText="Objects per listing page / copy batch. Larger values copy faster but use more memory. Default 100."
          >
            <InputNumber
              value={rule.batch_size}
              onChange={(batch_size) => onChange({ batch_size: batch_size || 100 })}
              min={1}
              max={10000}
              style={{ width: '100%', ...inputRadius }}
            />
          </FormField>
          <FormField
            label="Conflict policy"
            yamlPath="storage.replication.rules[].conflict"
            helpText="How to resolve when the destination object already exists."
          >
            <Radio.Group
              value={rule.conflict}
              onChange={(e) => onChange({ conflict: e.target.value })}
              style={{ display: 'flex', flexDirection: 'column', gap: 6 }}
            >
              <Radio value="newer-wins">Newer wins — safest default</Radio>
              <Radio value="content-diff">Content diff — mirror (copy only when bytes differ)</Radio>
              <Radio value="skip-if-dest-exists">Skip existing destination objects</Radio>
            </Radio.Group>
          </FormField>
          <FormField
            label="Delete replication"
            yamlPath="storage.replication.rules[].replicate_deletes"
            helpText="Make the destination a faithful mirror: an object that is not on the source is deleted, unless the rule's filters exclude it."
          >
            <label style={{ display: 'inline-flex', alignItems: 'center', gap: 8, cursor: 'pointer' }}>
              <Switch
                aria-label="Delete objects on the destination that are not on the source"
                checked={rule.replicate_deletes}
                onChange={(on) => {
                  // Turning it off is safe; turning it on can delete data on
                  // the destination, so it asks first.
                  if (!on) {
                    onChange({ replicate_deletes: false });
                    return;
                  }
                  Modal.confirm({
                    title: 'Delete objects on the destination?',
                    content: `When this rule runs, it deletes every object under ${rule.destination.bucket || 'the destination'}${rule.destination.prefix ? '/' + rule.destination.prefix : ''} that is not on the source, also objects that other tools wrote there. Objects that the rule's filters exclude stay.`,
                    okText: 'Turn on delete replication',
                    okButtonProps: { danger: true },
                    onOk: () => onChange({ replicate_deletes: true }),
                  });
                }}
              />
              <span>{rule.replicate_deletes ? 'On: the destination mirrors deletes' : 'Off: deletes are not copied'}</span>
            </label>
            <Alert
              type="warning"
              showIcon
              title="Deletes are destructive"
              description="When enabled, the destination is a faithful mirror of the source: a destination object absent at source is deleted, also one that another tool wrote. Only objects that the rule's filters exclude stay. Dedicate the destination to this rule, or exclude what it must keep."
              style={{ marginTop: 8 }}
            />
          </FormField>
        </div>
      </AdvancedDisclosure>

      <Text type="secondary" style={{ display: 'block', marginTop: 12, fontSize: 11 }}>
        Replication rules target bucket names. Bucket policies decide backend routing and encryption.
      </Text>
    </div>
  );
}
