import { Alert, Tag, Typography } from 'antd';
import { WarningOutlined } from '@ant-design/icons';
import type {
  LifecycleFailureEntry,
  LifecycleHistoryEntry,
  LifecycleRunOutcome,
} from '../adminApi';
import { formatBytes } from '../utils';
import { useColors } from '../ThemeContext';
import { fmtUnix, formRow } from './ruleEditorHelpers';
import { fmtDate } from './lifecycleHelpers';
import Metric from './LifecycleMetric';

const { Text } = Typography;

/** Latest preview / run-now result detail card for the selected lifecycle rule. */
export function PreviewPanel({
  outcome,
  maxCandidates,
}: {
  outcome: LifecycleRunOutcome | undefined;
  maxCandidates: number;
}) {
  if (!outcome) {
    return (
      <Alert
        type="info"
        showIcon
        message="Preview before deleting"
        description="Preview is read-only and is required before Run delete now becomes available."
        style={{ marginTop: 18 }}
      />
    );
  }

  return (
    <div style={{ marginTop: 18 }}>
      <div style={formRow(8, { justifyContent: 'space-between' })}>
        <Text strong>Latest preview/run result</Text>
        <Tag color={outcome.errors > 0 ? 'error' : 'processing'}>{outcome.status}</Tag>
      </div>
      <div style={{ marginTop: 8, display: 'flex', flexWrap: 'wrap', gap: 8 }}>
        <Metric label="Scanned" value={outcome.objects_scanned} />
        <Metric label="Would affect / affected" value={outcome.objects_affected} />
        <Metric label="Skipped" value={outcome.objects_skipped} />
        <Metric label="Bytes affected" value={formatBytes(outcome.bytes_affected)} />
        <Metric label="Errors" value={outcome.errors} tone={outcome.errors > 0 ? 'error' : undefined} />
      </div>
      <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 8 }}>
        Candidate list is capped by max failures/candidates (<Text code>{maxCandidates}</Text>).
      </Text>
      <div style={{ marginTop: 10, display: 'grid', gridTemplateColumns: '1fr', gap: 8, maxHeight: 300, overflow: 'auto' }}>
        {outcome.candidates.length === 0 ? (
          <Text type="secondary" style={{ fontSize: 12 }}>No lifecycle candidates returned.</Text>
        ) : outcome.candidates.map((obj) => (
          <div
            key={`${obj.bucket}/${obj.key}`}
            style={{
              border: '1px solid var(--border)',
              borderRadius: 10,
              padding: '8px 10px',
              background: 'var(--input-bg)',
              fontSize: 12,
              display: 'grid',
              gridTemplateColumns: 'minmax(0, 1fr) 160px 90px',
              gap: 10,
              alignItems: 'center',
            }}
          >
            <div>
              <Text code style={{ wordBreak: 'break-word' }}>{obj.bucket}/{obj.key}</Text>
              <div style={{ marginTop: 4 }}>
                <Tag color={obj.action === 'transition' ? 'processing' : 'warning'}>{obj.action}</Tag>
                {obj.destination_bucket && obj.destination_key && (
                  <Text type="secondary">
                    → <Text code>{obj.destination_bucket}/{obj.destination_key}</Text>
                    {obj.delete_source_after_success ? ' · delete source after copy' : ' · keep source'}
                  </Text>
                )}
              </div>
            </div>
            <Text type="secondary">{fmtDate(obj.created_at)}</Text>
            <Text>{formatBytes(obj.size)}</Text>
          </div>
        ))}
      </div>
      {outcome.failures.length > 0 && (
        <Alert
          type="error"
          showIcon
          message={`${outcome.failures.length} preview/run failure${outcome.failures.length === 1 ? '' : 's'}`}
          description={outcome.failures[0].error}
          style={{ marginTop: 10 }}
        />
      )}
    </div>
  );
}

/** Recent-runs + failures detail block for the selected lifecycle rule. */
export function RuntimeDetails({
  history,
  failures,
  runtimeError,
}: {
  history: LifecycleHistoryEntry[];
  failures: LifecycleFailureEntry[];
  runtimeError: string | null;
}) {
  const latestRunId = history[0]?.id ?? null;
  const latestRunFailures = latestRunId == null
    ? failures
    : failures.filter((failure) => failure.run_id === latestRunId);
  const olderFailures = latestRunId == null
    ? []
    : failures.filter((failure) => failure.run_id !== latestRunId);

  return (
    <div style={{ marginTop: 18, display: 'flex', flexDirection: 'column', gap: 14 }}>
      {runtimeError && (
        <Alert
          type="info"
          showIcon
          message="Lifecycle runtime history unavailable"
          description={`The config API returned no history/failure data for this rule: ${runtimeError}`}
        />
      )}
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
              {fmtUnix(run.started_at)} · affected {run.objects_affected}/{run.objects_scanned} · errors {run.errors}
            </div>
          ))}
        </div>
      </div>

      <FailureSection
        title={latestRunId == null ? 'Failures' : `Failures from latest run #${latestRunId}`}
        failures={latestRunFailures}
        emptyText={latestRunId == null ? 'No failures recorded.' : 'No failures recorded for the latest run.'}
        prominent
      />
      {olderFailures.length > 0 && (
        <FailureSection
          title="Older failures"
          failures={olderFailures}
          emptyText="No older failures."
        />
      )}
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
  failures: LifecycleFailureEntry[];
  emptyText: string;
  prominent?: boolean;
}) {
  const { ACCENT_RED } = useColors();
  return (
    <div>
      <div style={formRow(8, { justifyContent: 'space-between' })}>
        <Text strong>{title}</Text>
        {failures.length > 0 && <Tag color="error">{failures.length} shown</Tag>}
      </div>
      {prominent && failures.length > 0 && (
        <Alert
          type="error"
          showIcon
          style={{ marginTop: 8 }}
          message={failures[0].error_message}
          description={<Text code>{failures[0].bucket}/{failures[0].object_key}</Text>}
        />
      )}
      <div style={{ marginTop: 8, display: 'grid', gridTemplateColumns: '1fr', gap: 8, maxHeight: 300, overflow: 'auto' }}>
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
              gridTemplateColumns: '160px minmax(0, 1fr) minmax(220px, 0.8fr)',
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
              <Text code>{failure.bucket}/{failure.object_key || '(operation)'}</Text>
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
