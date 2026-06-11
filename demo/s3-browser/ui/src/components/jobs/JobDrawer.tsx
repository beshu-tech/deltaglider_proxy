/**
 * Job detail drawer: Definition (editable for rule kinds via the parent's
 * section editors; read-only parameters for one-offs), Runs, Failures.
 */
import { Alert, Drawer, Empty, Table, Tabs, Tag, Typography } from 'antd';
import type { LifecycleConfig, ReplicationConfig } from '../../adminApi';
import type { JobRow } from '../../jobsView';
import { jobStatusLabel, jobStatusTone, kindLabel, parseJobId } from '../../jobsView';
import { useJobFailures, useJobRuns } from '../../queries/jobs';
import ReplicationRuleFields from '../ReplicationRuleFields';
import LifecycleRuleFields from '../LifecycleRuleFields';
import { useBucketNames } from '../../queries/backends';

const { Text } = Typography;

interface Props {
  jobId: string | null;
  rows: JobRow[];
  replication: ReplicationConfig;
  lifecycle: LifecycleConfig;
  onReplicationChange: (fn: (cur: ReplicationConfig) => ReplicationConfig) => void;
  onLifecycleChange: (fn: (cur: LifecycleConfig) => LifecycleConfig) => void;
  /** Rename support: the drawer is keyed by `<kind>:<name>`, so a rename
   *  must retarget the key or the lookup loses the rule mid-keystroke. */
  onJobIdChange: (next: string) => void;
  inputRadius: { borderRadius: number };
  onClose: () => void;
}

function fmt(ts?: number | null): string {
  return ts ? new Date(ts * 1000).toLocaleString() : '—';
}

export default function JobDrawer({
  jobId,
  rows,
  replication,
  lifecycle,
  onReplicationChange,
  onLifecycleChange,
  onJobIdChange,
  inputRadius,
  onClose,
}: Props) {
  const parsed = jobId ? parseJobId(jobId) : null;
  const serverRow = rows.find((r) => r.id === jobId) ?? null;
  // Runs/failures only exist for jobs the SERVER knows (not drafts).
  const runsQuery = useJobRuns(serverRow ? jobId : null);
  const failuresQuery = useJobFailures(serverRow ? jobId : null);
  const bucketNames = useBucketNames();

  const replIndex =
    parsed?.subsystem === 'replication'
      ? replication.rules.findIndex((r) => r.name === parsed.key)
      : -1;
  const lcIndex =
    parsed?.subsystem === 'lifecycle'
      ? lifecycle.rules.findIndex((r) => r.name === parsed.key)
      : -1;

  const definition = (() => {
    if (replIndex >= 0) {
      const rule = replication.rules[replIndex];
      return (
        <ReplicationRuleFields
          rule={rule}
          buckets={bucketNames}
          inputRadius={inputRadius}
          onChange={(patch) =>
            onReplicationChange((cur) => ({
              ...cur,
              rules: cur.rules.map((r, i) => (i === replIndex ? { ...r, ...patch } : r)),
            }))
          }
          onRename={(next) => {
            onReplicationChange((cur) => ({
              ...cur,
              rules: cur.rules.map((r, i) => (i === replIndex ? { ...r, name: next } : r)),
            }));
            onJobIdChange(`replication:${next}`);
          }}
        />
      );
    }
    if (lcIndex >= 0) {
      const rule = lifecycle.rules[lcIndex];
      return (
        <LifecycleRuleFields
          rule={rule}
          buckets={bucketNames}
          inputRadius={inputRadius}
          onChange={(patch) =>
            onLifecycleChange((cur) => ({
              ...cur,
              rules: cur.rules.map((r, i) => (i === lcIndex ? { ...r, ...patch } : r)),
            }))
          }
          onRename={(next) => {
            onLifecycleChange((cur) => ({
              ...cur,
              rules: cur.rules.map((r, i) => (i === lcIndex ? { ...r, name: next } : r)),
            }));
            onJobIdChange(`lifecycle:${next}`);
          }}
        />
      );
    }
    if (serverRow) {
      // One-off job: read-only parameters.
      const entries: Array<[string, string]> = [
        ['Bucket', serverRow.scope.bucket],
        ...(serverRow.scope.target ? ([['Target', serverRow.scope.target]] as Array<[string, string]>) : []),
        ['Phase', serverRow.phase ?? '—'],
        ['Created', fmt(serverRow.created_at)],
        ['Started', fmt(serverRow.started_at)],
        ['Finished', fmt(serverRow.finished_at)],
      ];
      return (
        <div>
          {serverRow.last_error && (
            <Alert type="error" showIcon message={serverRow.last_error} style={{ marginBottom: 12, borderRadius: 8 }} />
          )}
          {entries.map(([k, v]) => (
            <div key={k} style={{ display: 'flex', gap: 12, padding: '6px 0' }}>
              <Text type="secondary" style={{ width: 90, fontSize: 12 }}>
                {k}
              </Text>
              <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>{v}</Text>
            </div>
          ))}
        </div>
      );
    }
    return <Empty description="Rule not found in the editor" />;
  })();

  const runsTable = (
    <Table
      dataSource={runsQuery.data?.runs ?? []}
      rowKey="id"
      size="small"
      pagination={false}
      locale={{ emptyText: 'No runs yet' }}
      columns={[
        { title: 'Started', render: (_: unknown, r) => fmt(r.started_at) },
        { title: 'By', dataIndex: 'triggered_by', width: 90 },
        {
          title: 'Status',
          width: 110,
          render: (_: unknown, r) => (
            <Tag color={jobStatusTone({ status: r.status })}>{r.status}</Tag>
          ),
        },
        { title: 'Scanned', dataIndex: 'objects_scanned', width: 80 },
        { title: 'Processed', dataIndex: 'objects_processed', width: 90 },
        { title: 'Errors', dataIndex: 'errors', width: 70 },
      ]}
    />
  );

  const failuresTable = (
    <Table
      dataSource={failuresQuery.data?.failures ?? []}
      rowKey="id"
      size="small"
      pagination={false}
      locale={{ emptyText: 'No recorded failures' }}
      columns={[
        { title: 'When', width: 160, render: (_: unknown, f) => fmt(f.occurred_at) },
        {
          title: 'Object',
          render: (_: unknown, f) => (
            <Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>
              {f.bucket ? `${f.bucket}/` : ''}
              {f.object_key || '(rule-level)'}
            </Text>
          ),
        },
        {
          title: 'Error',
          render: (_: unknown, f) => (
            <Text type="danger" style={{ fontSize: 12 }}>
              {f.error}
            </Text>
          ),
        },
      ]}
    />
  );

  return (
    <Drawer
      open={!!jobId}
      onClose={onClose}
      width={640}
      title={
        serverRow ? (
          <span>
            {kindLabel(serverRow.kind)} · <span style={{ fontFamily: 'var(--font-mono)' }}>{serverRow.name}</span>{' '}
            <Tag color={jobStatusTone(serverRow)} style={{ marginLeft: 8 }}>
              {jobStatusLabel(serverRow)}
            </Tag>
          </span>
        ) : (
          parsed && (
            <span>
              {kindLabel(parsed.subsystem === 'replication' ? 'replication' : 'lifecycle')} ·{' '}
              <span style={{ fontFamily: 'var(--font-mono)' }}>{parsed.key}</span>{' '}
              <Tag color="warning" style={{ marginLeft: 8 }}>
                draft
              </Tag>
            </span>
          )
        )
      }
    >
      <Tabs
        items={[
          { key: 'definition', label: 'Definition', children: definition },
          ...(serverRow
            ? [
                { key: 'runs', label: 'Runs', children: runsTable },
                { key: 'failures', label: 'Failures', children: failuresTable },
              ]
            : []),
        ]}
      />
    </Drawer>
  );
}
