import { Input, Modal, Radio, Switch, Typography } from 'antd';
import type { ReplicationRuleConfig } from '../adminApi';
import BucketPrefixInput from './BucketPrefixInput';
import FormField from './FormField';
import { AdvancedDisclosure, RuleNumberInput } from './ruleEditorFields';
import RuleFilterFields from './RuleFilterFields';
import { ruleFolderPrefix } from '../ruleFilterPreview';
import type { RuleFieldErrors } from './jobs/ruleFieldErrors';
import { SPACE } from './shared-styles';

const { Text } = Typography;

const GRID = (min: number) => ({ display: 'grid', gridTemplateColumns: `repeat(auto-fit, minmax(${min}px, 1fr))`, gap: SPACE.md });

/**
 * Per-rule field editor body (name / enabled / source / destination /
 * interval / delete replication, then batch size and conflicts under
 * Advanced). The parent renders it inside the rule drawer's Definition tab
 * and owns all state.
 *
 * `errors` are the field errors to SHOW now (the parent checks every field
 * with `replicationRuleErrors` and shows an error once its field lost focus,
 * or after Review & apply); `onFieldBlur` tells the parent a field lost focus.
 */
export default function ReplicationRuleFields({
  rule,
  buckets,
  inputRadius,
  onChange,
  onRename,
  errors = {},
  onFieldBlur = () => {},
}: {
  rule: ReplicationRuleConfig;
  buckets: string[];
  inputRadius: { borderRadius: number };
  onChange: (patch: Partial<ReplicationRuleConfig>) => void;
  onRename: (nextName: string) => void;
  errors?: RuleFieldErrors;
  onFieldBlur?: (field: string) => void;
}) {
  const blur = (field: string) => () => onFieldBlur(field);
  return (
    <div>
      <div style={GRID(260)}>
        <FormField
          label="Rule name"
          yamlPath="storage.replication.rules[].name"
          helpText="A name that is unique on this page: letters, digits, dots, dashes and underscores, 64 characters at most."
          error={errors.name}
        >
          <Input
            value={rule.name}
            onChange={(e) => onRename(e.target.value)}
            onBlur={blur('name')}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </FormField>
        <FormField
          label="Enabled"
          yamlPath="storage.replication.rules[].enabled"
          helpText="Per-rule switch. The rule runs only when “Run replication rules”, at the top of this page, is also on."
        >
          <Switch checked={rule.enabled} onChange={(enabled) => onChange({ enabled })} />
        </FormField>
        <FormField
          label="Source"
          yamlPath="storage.replication.rules[].source"
          helpText="Bucket and optional prefix to copy objects from."
          error={errors.source}
        >
          <div onBlur={blur('source')}>
            <BucketPrefixInput
              value={rule.source}
              onChange={(source) => onChange({ source })}
              buckets={buckets}
              bucketPlaceholder="prod-artifacts"
              prefixPlaceholder="builds/releases/"
            />
          </div>
        </FormField>
        <FormField
          label="Destination"
          yamlPath="storage.replication.rules[].destination"
          helpText="Bucket and optional prefix to copy objects into. An empty prefix mirrors the source key path."
          error={errors.destination}
        >
          <div onBlur={blur('destination')}>
            <BucketPrefixInput
              value={rule.destination}
              onChange={(destination) => onChange({ destination })}
              buckets={buckets}
              bucketPlaceholder="backup-artifacts"
              prefixPlaceholder="mirror/releases/"
            />
          </div>
        </FormField>
        <FormField
          label="Interval"
          yamlPath="storage.replication.rules[].interval"
          helpText="How often the full sweep of this rule runs. Events copy changes as they happen; the sweep copies what an event missed. A duration such as 6h or 24h (the default). The minimum is 30s."
          error={errors.interval}
        >
          <Input
            value={rule.interval}
            onChange={(e) => onChange({ interval: e.target.value })}
            onBlur={blur('interval')}
            placeholder="24h"
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </FormField>
        <FormField
          label="Delete replication"
          yamlPath="storage.replication.rules[].replicate_deletes"
          helpText="On makes the destination a mirror: a run deletes each destination object that is not on the source, also one that another tool wrote. Objects that the rule's filters exclude stay. Give the destination to this rule only, or exclude what it must keep."
        >
          <label style={{ display: 'inline-flex', alignItems: 'center', gap: SPACE.xs, cursor: 'pointer' }}>
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

      <AdvancedDisclosure title="Advanced" hint="Batch size and conflicts" hasError={!!errors.batch_size}>
        <div style={GRID(240)}>
          <FormField
            label="Batch size"
            yamlPath="storage.replication.rules[].batch_size"
            helpText="Objects per listing page and copy batch, from 1 to 10,000. Larger values copy faster but use more memory. The default is 100."
            error={errors.batch_size}
          >
            <RuleNumberInput
              value={rule.batch_size}
              onChange={(batch_size) => onChange({ batch_size: batch_size as number })}
              onBlur={blur('batch_size')}
              placeholder="100"
              style={{ width: '100%', ...inputRadius }}
            />
          </FormField>
          <FormField
            label="Conflict policy"
            yamlPath="storage.replication.rules[].conflict"
            helpText="What the rule does when the destination object already exists."
          >
            <Radio.Group
              value={rule.conflict}
              onChange={(e) => onChange({ conflict: e.target.value })}
              style={{ display: 'flex', flexDirection: 'column', gap: SPACE.xs }}
            >
              <Radio value="newer-wins">Newer wins — safest default</Radio>
              <Radio value="content-diff">Content diff — mirror (copy only when bytes differ)</Radio>
              <Radio value="skip-if-dest-exists">Skip existing destination objects</Radio>
            </Radio.Group>
          </FormField>
        </div>
      </AdvancedDisclosure>

      <Text type="secondary" style={{ display: 'block', marginTop: SPACE.sm, fontSize: 11 }}>
        Replication rules target bucket names. Bucket policies decide backend routing and encryption.
      </Text>
    </div>
  );
}
