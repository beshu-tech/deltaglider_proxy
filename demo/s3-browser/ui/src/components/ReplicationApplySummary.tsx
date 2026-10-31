import { Tag, Typography } from 'antd';
import type { ReplicationConfig } from '../adminApi';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

/**
 * Plan summary rendered inside the ApplyDialog before a replication PUT.
 * Extracted verbatim from ReplicationPanel; pure presentation over the pending
 * config the section editor computed.
 */
export default function ReplicationApplySummary({ replication }: { replication: ReplicationConfig }) {
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
      <div style={{ marginTop: 12, display: 'flex', flexDirection: 'column', gap: 8 }}>
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
