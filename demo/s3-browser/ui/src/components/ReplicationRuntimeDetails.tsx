import { Alert, Tag, Typography } from 'antd';
import { WarningOutlined } from '@ant-design/icons';
import type { ReplicationFailureEntry, ReplicationHistoryEntry } from '../adminApi';
import { fmtUnix } from './ruleEditorHelpers';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

/**
 * Runtime overview for the selected rule: recent runs + latest/older failure
 * detail panels. Extracted verbatim from ReplicationPanel; pure presentation
 * over the history/failures props the parent fetches.
 */
export default function ReplicationRuntimeDetails({
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
  const { ACCENT_RED } = useColors();
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
              <WarningOutlined style={{ color: ACCENT_RED, marginRight: 6 }} />
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
