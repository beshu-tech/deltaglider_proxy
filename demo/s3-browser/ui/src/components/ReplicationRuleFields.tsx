import { Alert, Input, InputNumber, Radio, Switch, Typography } from 'antd';
import type { ReplicationRuleConfig } from '../adminApi';
import BucketPrefixInput from './BucketPrefixInput';
import FormField from './FormField';
import { AdvancedDisclosure } from './ruleEditorFields';
import GlobListTextArea from './GlobListTextArea';

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
          helpText="Per-rule toggle. The global scheduler must also be enabled for this rule to run automatically."
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

      <AdvancedDisclosure title="Advanced rule behavior">
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(240px, 1fr))', gap: 14 }}>
          <FormField
            label="Interval"
            yamlPath="storage.replication.rules[].interval"
            helpText="How often the scheduler runs this rule. Humantime duration, e.g. 5m, 1h, 24h."
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
            helpText="Make the destination a faithful mirror: any object not present at source is deleted."
          >
            <Switch
              checked={rule.replicate_deletes}
              onChange={(replicate_deletes) => onChange({ replicate_deletes })}
            />
            <Alert
              type="warning"
              showIcon
              message="Deletes are destructive"
              description="When enabled, the destination is a faithful mirror of the source: ANY destination object absent at source is deleted — including objects written by other tools. The destination bucket must be dedicated to this rule."
              style={{ marginTop: 8 }}
            />
          </FormField>
          <FormField
            label="Include globs"
            yamlPath="storage.replication.rules[].include_globs"
            helpText="One glob per line. If non-empty, only matching source keys are replicated. Empty means everything under the source prefix."
          >
            <GlobListTextArea
              value={rule.include_globs}
              onChange={(v) => onChange({ include_globs: v })}
              rows={3}
              placeholder={'*.zip\nreleases/**'}
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
            />
          </FormField>
          <FormField
            label="Exclude globs"
            yamlPath="storage.replication.rules[].exclude_globs"
            helpText="One glob per line. Source keys matching any pattern are skipped."
          >
            <GlobListTextArea
              value={rule.exclude_globs}
              onChange={(v) => onChange({ exclude_globs: v })}
              rows={3}
              placeholder=".dg/*"
              style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
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
