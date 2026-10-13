/**
 * Job detail drawer: Definition (editable for rule kinds via the parent's
 * section editors; read-only parameters for one-offs), Runs, Failures.
 */
import { useEffect, useRef, useMemo, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { Alert, Button, Drawer, Empty, Modal, Tabs, Tag, Typography } from 'antd';
import { StopOutlined } from '@ant-design/icons';
import RowActionsMenu from '../RowActionsMenu';
import { draftRemoveAction } from './draftRemoveAction';
import { useColors } from '../../ThemeContext';
import './OutcomeMeter.css';
import type { LifecycleConfig, ReplicationConfig } from '../../adminApi';
import type { JobRow, LifecycleVerb } from '../../jobsView';
import {
  availableActions,
  cancelConfirmText,
  failuresCountText,
  groupFailureErrors,
  isActiveJobStatus,
  jobInFlight,
  jobWalkProgress,
  jobStrategyMix,
  jobStatusLabel,
  jobStatusTone,
  kindLabel,
  oneOffDefinition,
  overlayLiveRun,
  parseJobId,
  rowMeterInput,
  runDuration,
  runMeterInput,
  runsRefreshKey,
} from '../../jobsView';
import { useNow } from '../../hooks/useNow';
import StrategyBand from './StrategyBand';
import { formatBytes } from '../../utils';
import { qk } from '../../queries/keys';
import { useJob, useJobFailures, useJobRuns } from '../../queries/jobs';
import TimeAgo from '../TimeAgo';
import RecordList from './RecordList';
import OutcomeMeter from './OutcomeMeter';
import ReplicationRuleFields from '../ReplicationRuleFields';
import LifecycleRuleFields from '../LifecycleRuleFields';
import VerifyTab from './VerifyTab';
import LifecyclePreviewTab from './LifecyclePreviewTab';
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
  /** Drop a draft (a rule that exists only in the unsaved edits). */
  onRemoveDraft: (draft: Pick<JobRow, 'id' | 'kind' | 'name'>) => void;
  /** Cancel a running one-off (asked for confirmation here first). */
  onCancelJob?: (row: JobRow) => Promise<unknown> | void;
  /** What a lifecycle rule does to an object (meter words). */
  lifecycleVerbOf?: (row: JobRow) => LifecycleVerb | undefined;
  /** The page's unsaved-changes bar is showing: keep the last fields clear of it. */
  bottomInset?: boolean;
  /** Currently active tab (URL-deep-linked). */
  activeTab?: string;
  /** Called when the user switches tabs. */
  onTabChange?: (tab: string) => void;
  inputRadius: { borderRadius: number };
  onClose: () => void;
}

/**
 * Escape inside an open dropdown (an autocomplete, a select) closes the
 * dropdown only. The drawer closes on Escape too, and it listens on an
 * ancestor of the field, so the key must stop at the field.
 */
function stopEscapeFromOpenDropdown(e: React.KeyboardEvent) {
  if (e.key !== 'Escape') return;
  const t = e.target as HTMLElement | null;
  if (t?.getAttribute('aria-expanded') === 'true') e.stopPropagation();
}

export default function JobDrawer({
  jobId,
  rows,
  replication,
  lifecycle,
  onReplicationChange,
  onLifecycleChange,
  onJobIdChange,
  onRemoveDraft,
  onCancelJob,
  lifecycleVerbOf,
  bottomInset,
  activeTab,
  onTabChange,
  inputRadius,
  onClose,
}: Props) {
  const c = useColors();
  const parsed = jobId ? parseJobId(jobId) : null;
  const listedRow = rows.find((r) => r.id === jobId) ?? null;
  // The list carries the recent one-offs only: a link to an older one reads
  // it by id.
  const isOneOffId = parsed?.subsystem === 'maintenance';
  const fetched = useJob(isOneOffId && !listedRow ? jobId : null);
  const serverRow = listedRow ?? (isOneOffId ? (fetched.data ?? null) : null);
  const ruleKind = parsed?.subsystem === 'replication' ? 'replication' : parsed?.subsystem === 'lifecycle' ? 'lifecycle' : null;
  const verb = serverRow ? lifecycleVerbOf?.(serverRow) : undefined;
  const nowSec = useNow(serverRow && isActiveJobStatus(serverRow.status) ? 1000 : 60_000) / 1000;
  const [cancelling, setCancelling] = useState(false);
  // Live "currently copying" objects (name + size) — justifies a slow-moving
  // counter when the run is chewing through multi-GB files.
  const inFlight = jobInFlight(serverRow);
  // Live reconcile-walk position + dir counts (replication rows on an active
  // run). Shown as an activity indicator so a long HEAD-heavy scan reads as
  // "working, here" rather than "stuck at 0 copied".
  const walk = jobWalkProgress(serverRow);
  // Runs/failures only exist for jobs the SERVER knows (not drafts).
  // These are NOT polled — the jobs LIST already polls (2s while active) and
  // carries the live progress in `serverRow`. We overlay that onto the running
  // run row below, and refetch the history ONCE when the run finishes.
  const runsQuery = useJobRuns(serverRow ? jobId : null);
  const failuresQuery = useJobFailures(serverRow ? jobId : null);
  const bucketNames = useBucketNames();

  // Read run history + failures again whenever the row says a run started,
  // settled, or the rule was paused — including a run that the scheduler
  // started and finished between two polls of the list.
  const qc = useQueryClient();
  const refreshKey = runsRefreshKey(serverRow);
  const lastKey = useRef(refreshKey);
  useEffect(() => {
    if (lastKey.current !== refreshKey && serverRow) {
      qc.invalidateQueries({ queryKey: qk.jobs.runs(serverRow.id) });
      qc.invalidateQueries({ queryKey: qk.jobs.failures(serverRow.id) });
    }
    lastKey.current = refreshKey;
  }, [refreshKey, serverRow, qc]);

  // Overlay the live (list-polled) progress onto the running run row so its
  // numbers tick without re-fetching.
  const liveRuns = overlayLiveRun(runsQuery.data?.runs ?? [], serverRow);
  const runKind = serverRow?.kind ?? ruleKind ?? 'replication';

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
      // One-off job: read-only parameters, what it did, and Cancel.
      const entries = oneOffDefinition(serverRow, nowSec);
      const canCancel = availableActions(serverRow).includes('cancel') && !!onCancelJob;
      const askCancel = () => {
        const text = cancelConfirmText(serverRow);
        Modal.confirm({
          title: text.title,
          content: text.content,
          okText: 'Cancel job',
          cancelText: 'Keep running',
          okButtonProps: { danger: true },
          onOk: async () => {
            setCancelling(true);
            try {
              await onCancelJob?.(serverRow);
            } finally {
              setCancelling(false);
            }
          },
        });
      };
      return (
        <div>
          {serverRow.last_error && (
            <Alert type="error" showIcon title={serverRow.last_error} style={{ marginBottom: 12, borderRadius: 8 }} />
          )}
          <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginBottom: 8 }}>
            <div style={{ flex: 1, minWidth: 0 }}>
              <OutcomeMeter {...rowMeterInput(serverRow)} />
            </div>
            {canCancel && (
              <Button danger size="small" icon={<StopOutlined />} loading={cancelling} onClick={askCancel}>
                Cancel job…
              </Button>
            )}
          </div>
          {entries.map(([k, v]) => (
            <div key={k} style={{ display: 'flex', gap: 12, padding: '6px 0' }}>
              <Text type="secondary" style={{ width: 110, flexShrink: 0, fontSize: 12 }}>
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
    if (isOneOffId) {
      return fetched.isLoading ? null : <Empty description="This job does not exist on this proxy." />;
    }
    // A rule-kind jobId that resolves to NEITHER a live editor rule NOR a server
    // row is a stale draft reference (the draft was discarded or renamed while
    // the drawer pointed at it). Don't auto-close: show "this job no longer
    // exists" on a shared/deep-linked URL instead of a silent close.
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
            <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
              <OutcomeMeter
                {...runMeterInput(
                  r,
                  runKind,
                  verb,
                  isActiveJobStatus(r.status) ? serverRow?.phase : null,
                )}
              />
              {/* How each copied object moved: replication only. */}
              {runKind === 'replication' && <StrategyBand mix={jobStrategyMix(r)} />}
            </div>
          ),
        },
        {
          key: 'took',
          label: 'Took',
          track: 'max-content',
          render: (r) => (
            <Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
              {runDuration(r.started_at, r.finished_at, nowSec) ?? '—'}
            </Text>
          ),
        },
        {
          key: 'bytes',
          label: 'Data',
          track: 'max-content',
          render: (r) => (
            <Text
              type="secondary"
              style={{ fontSize: 12, whiteSpace: 'nowrap' }}
              title="Logical size of the objects the run wrote"
            >
              {formatBytes(r.bytes)}
            </Text>
          ),
        },
      ]}
    />
  );

  const failures = failuresQuery.data?.failures ?? [];
  const failuresCount = failuresCountText(failures.length, failuresQuery.data?.total);
  const errorGroups = groupFailureErrors(failures);
  const failuresTable = (
    <div>
      {failuresCount && (
        <Text type="secondary" style={{ display: 'block', fontSize: 12, marginBottom: 8 }}>
          {failuresCount}
        </Text>
      )}
      {errorGroups.length > 0 && (
        <div style={{ marginBottom: 12, fontSize: 12 }} aria-label="Repeated errors">
          {errorGroups.slice(0, 3).map((g) => (
            <div key={g.error} style={{ display: 'flex', gap: 8, padding: '2px 0' }}>
              <Text strong style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
                {g.count}×
              </Text>
              <span className="dg-fail-text" style={{ color: c.ACCENT_RED }}>
                {g.error}
              </span>
            </div>
          ))}
        </div>
      )}
      <RecordList
        rows={failures}
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
                  {f.dest_key && f.dest_key !== f.object_key && (
                    <span style={{ color: c.TEXT_MUTED }}> → {f.dest_key}</span>
                  )}
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
    </div>
  );

  // Clamp the URL-deep-linked tab to one that actually exists for this job —
  // e.g. ?tab=verify on a lifecycle job would otherwise render a blank body.
  const availableTabs = useMemo(() => {
    const tabs = ['definition'];
    if (serverRow) {
      tabs.push('runs', 'failures');
      if (parsed?.subsystem === 'replication') tabs.push('verify');
      if (parsed?.subsystem === 'lifecycle') tabs.push('preview');
    }
    return tabs;
  }, [serverRow, parsed]);

  const clampedTab = activeTab && availableTabs.includes(activeTab) ? activeTab : 'definition';

  return (
    <Drawer
      open={!!jobId}
      onClose={onClose}
      // 100%, not 100vw: the drawer sits left of a classic scrollbar, and
      // 100vw pushed its left edge off a phone screen.
      size="min(640px, 100%)"
      // Room under the last field for the page's floating unsaved-changes bar.
      styles={bottomInset ? { body: { paddingBottom: 96 } } : undefined}
      extra={
        !serverRow && parsed && ruleKind ? (
          <RowActionsMenu
            label={`More actions for ${parsed.key}`}
            actions={[
              draftRemoveAction(
                {
                  id: jobId ?? '',
                  kind: ruleKind,
                  name: parsed.key,
                },
                onRemoveDraft,
              ),
            ]}
          />
        ) : undefined
      }
      title={
        serverRow ? (
          <span>
            {kindLabel(serverRow.kind)} · <span style={{ fontFamily: 'var(--font-mono)' }}>{serverRow.name}</span>{' '}
            <Tag color={jobStatusTone(serverRow)} style={{ marginLeft: 8 }}>
              {jobStatusLabel(serverRow)}
            </Tag>
          </span>
        ) : parsed && ruleKind ? (
          <span>
            {kindLabel(ruleKind)} · <span style={{ fontFamily: 'var(--font-mono)' }}>{parsed.key}</span>{' '}
            <Tag color="warning" style={{ marginLeft: 8 }}>
              draft
            </Tag>
          </span>
        ) : (
          parsed && (
            <span>
              Job · <span style={{ fontFamily: 'var(--font-mono)' }}>{parsed.key}</span>
            </span>
          )
        )
      }
    >
      <div onKeyDown={stopEscapeFromOpenDropdown}>
      {walk && (
        <div
          style={{
            marginBottom: 12,
            padding: '8px 12px',
            borderRadius: 8,
            background: c.BG_ELEVATED,
            fontSize: 12.5,
          }}
        >
          <div style={{ display: 'flex', gap: 8, alignItems: 'baseline' }}>
            <Typography.Text type="secondary" style={{ whiteSpace: 'nowrap' }}>
              scanning
            </Typography.Text>
            <Typography.Text
              style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}
              ellipsis
            >
              {walk.scanning ?? '…'}
            </Typography.Text>
          </div>
          <div style={{ marginTop: 2 }}>
            <Typography.Text type="secondary">
              {walk.dirs_completed.toLocaleString()} dirs done ·{' '}
              {walk.dirs_pending.toLocaleString()} pending
            </Typography.Text>
          </div>
          {/* Slim indeterminate activity bar — no honest % (pending grows then
              drains as the tree is discovered), so this signals motion only. */}
          <div
            style={{
              position: 'relative',
              height: 3,
              marginTop: 6,
              borderRadius: 2,
              overflow: 'hidden',
              background: c.BORDER,
            }}
          >
            <div className="dg-meter-indeterminate" />
          </div>
        </div>
      )}
      {inFlight.length > 0 && (
        <div
          style={{
            marginBottom: 12,
            padding: '8px 12px',
            borderRadius: 8,
            background: c.BG_ELEVATED,
            fontSize: 12.5,
          }}
        >
          {inFlight.map((f) => (
            <div key={f.key} style={{ display: 'flex', gap: 8, alignItems: 'baseline' }}>
              <Typography.Text type="secondary">copying</Typography.Text>
              <Typography.Text style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }} ellipsis>
                {f.key}
              </Typography.Text>
              <Typography.Text type="secondary" style={{ whiteSpace: 'nowrap' }}>
                {formatBytes(f.size)} · <TimeAgo ts={f.started_unix} />
              </Typography.Text>
            </div>
          ))}
        </div>
      )}
      <Tabs
        activeKey={clampedTab}
        onChange={onTabChange}
        items={[
          { key: 'definition', label: 'Definition', children: definition },
          ...(serverRow
            ? [
                { key: 'runs', label: 'Runs', children: runsTable },
                { key: 'failures', label: 'Failures', children: failuresTable },
              ]
            : []),
          // Preview is lifecycle-only: what the SAVED rule would act on now.
          ...(serverRow && parsed?.subsystem === 'lifecycle'
            ? [{ key: 'preview', label: 'Preview', children: <LifecyclePreviewTab jobId={serverRow.id} /> }]
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
      </div>
    </Drawer>
  );
}
