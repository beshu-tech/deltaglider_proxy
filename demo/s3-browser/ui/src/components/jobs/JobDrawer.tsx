/**
 * Job detail drawer: Definition (editable for rule kinds via the parent's
 * section editors; read-only parameters for one-offs), Runs, Failures.
 */
import { useEffect, useRef } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { Alert, Drawer, Empty, Tabs, Tag, Typography } from 'antd';
import { useColors } from '../../ThemeContext';
import type { LifecycleConfig, ReplicationConfig } from '../../adminApi';
import type { JobRow } from '../../jobsView';
import { isActiveJobStatus, jobStatusLabel, jobStatusTone, kindLabel, parseJobId } from '../../jobsView';
import { qk } from '../../queries/keys';
import { useJobFailures, useJobRuns } from '../../queries/jobs';
import TimeAgo from '../TimeAgo';
import RecordList from './RecordList';
import OutcomeMeter from './OutcomeMeter';
import ReplicationRuleFields from '../ReplicationRuleFields';
import LifecycleRuleFields from '../LifecycleRuleFields';
import VerifyTab from './VerifyTab';
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
  const c = useColors();
  const parsed = jobId ? parseJobId(jobId) : null;
  const serverRow = rows.find((r) => r.id === jobId) ?? null;
  // Runs/failures only exist for jobs the SERVER knows (not drafts).
  // These are NOT polled — the jobs LIST already polls (2s while active) and
  // carries the live progress in `serverRow`. We overlay that onto the running
  // run row below, and refetch the history ONCE when the run finishes.
  const runsQuery = useJobRuns(serverRow ? jobId : null);
  const failuresQuery = useJobFailures(serverRow ? jobId : null);
  const bucketNames = useBucketNames();

  // Refetch run history + failures exactly when the active run transitions to a
  // terminal state — event-driven, so no second poller. The list poll is the
  // single live source; this just captures the final numbers + the next run.
  const qc = useQueryClient();
  const wasActive = useRef(false);
  const liveActive = serverRow ? isActiveJobStatus(serverRow.status) : false;
  useEffect(() => {
    if (wasActive.current && !liveActive && serverRow) {
      qc.invalidateQueries({ queryKey: qk.jobs.runs(serverRow.id) });
      qc.invalidateQueries({ queryKey: qk.jobs.failures(serverRow.id) });
    }
    wasActive.current = liveActive;
  }, [liveActive, serverRow, qc]);

  // Overlay the live (list-polled) progress onto the running run row so its
  // scanned/processed/status tick without re-fetching. Matched by started_at.
  const liveRuns = (() => {
    const runs = runsQuery.data?.runs ?? [];
    if (!serverRow || !liveActive) return runs;
    return runs.map((r) =>
      r.started_at === serverRow.started_at && isActiveJobStatus(r.status)
        ? {
            ...r,
            status: serverRow.status,
            objects_processed: serverRow.progress.processed,
            objects_skipped: serverRow.progress.skipped,
            errors: serverRow.progress.failed,
            // Carry live percent so the running run's meter fills (null = unknown).
            __percent: serverRow.percent ?? null,
          }
        : r,
    );
  })();

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
              <Text type="secondary" style={{ width: 90, flexShrink: 0, fontSize: 12 }}>
                {k}
              </Text>
              <Text
                style={{
                  fontFamily: 'var(--font-mono)',
                  fontSize: 13,
                  wordBreak: 'break-word',
                  minWidth: 0,
                }}
              >
                {v}
              </Text>
            </div>
          ))}
        </div>
      );
    }
    return <Empty description="This job no longer exists — it may have been removed or already applied. Close this panel." />;
  })();

  const runsTable = (
    <RecordList
      rows={liveRuns}
      rowKey={(r) => String(r.id)}
      empty="No runs yet"
      columns={[
        {
          key: 'started',
          label: 'Started',
          track: 'max-content',
          render: (r) => (
            <span style={{ whiteSpace: 'nowrap' }}>
              {/* "By" folded in as a leading glyph: ⏱ scheduler / ▸ manual. */}
              <span
                title={r.triggered_by === 'scheduler' ? 'scheduler' : r.triggered_by}
                aria-label={r.triggered_by === 'scheduler' ? 'scheduled' : 'manual run'}
                style={{ color: c.TEXT_MUTED, marginRight: 4 }}
              >
                {r.triggered_by === 'scheduler' ? '⏱' : '▸'}
              </span>
              <TimeAgo ts={r.started_at} />
            </span>
          ),
        },
        {
          key: 'meter',
          label: 'Outcome',
          track: 'minmax(0,1fr)',
          hideLabelOnNarrow: true,
          render: (r) => (
            <OutcomeMeter
              scanned={r.objects_scanned}
              copied={r.objects_processed}
              errors={r.errors}
              skipped={r.objects_skipped}
              status={r.status}
              percent={'__percent' in r ? (r as { __percent: number | null }).__percent : null}
            />
          ),
        },
      ]}
    />
  );

  const failuresTable = (
    <RecordList
      rows={failuresQuery.data?.failures ?? []}
      rowKey={(f) => String(f.id)}
      empty="No recorded failures"
      columns={[
        {
          key: 'when',
          label: 'When',
          track: 'max-content',
          render: (f) => (
            <span style={{ whiteSpace: 'nowrap' }}>
              <TimeAgo ts={f.occurred_at} />
            </span>
          ),
        },
        {
          key: 'object',
          label: 'Object',
          track: 'minmax(0,1.2fr)',
          render: (f) => {
            const obj = `${f.bucket ? `${f.bucket}/` : ''}${f.object_key || '(job-level)'}`;
            return (
              <span className="dg-fail-text" title={obj} style={{ fontFamily: 'var(--font-mono)' }}>
                {obj}
              </span>
            );
          },
        },
        {
          key: 'error',
          label: 'Error',
          track: 'minmax(0,1.5fr)',
          render: (f) => (
            <span className="dg-fail-text" title={f.error} style={{ color: c.ACCENT_RED }}>
              {f.error}
            </span>
          ),
        },
      ]}
    />
  );

  return (
    <Drawer
      open={!!jobId}
      onClose={onClose}
      width="min(640px, 100vw)"
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
          // Verify is replication-only and needs a server-known rule to audit.
          ...(serverRow && parsed?.subsystem === 'replication'
            ? [
                {
                  key: 'verify',
                  label: 'Verify',
                  children: <VerifyTab ruleName={parsed.key} />,
                },
              ]
            : []),
        ]}
      />
    </Drawer>
  );
}
