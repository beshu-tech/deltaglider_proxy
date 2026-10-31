/**
 * Job detail drawer: Definition (editable for rule kinds via the parent's
 * section editors; read-only parameters for one-offs), Runs, Failures.
 */
import { useEffect, useRef, useMemo, useState, type CSSProperties } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { Alert, Button, Drawer, Modal, Tabs, Tag, Typography } from 'antd';
import { HistoryOutlined, QuestionCircleOutlined, StopOutlined } from '@ant-design/icons';
import { EmptyState, LoadingState } from '../StatePlaceholders';
import { lifecycleRuleErrors, replicationRuleErrors, type RuleFieldErrors } from './ruleFieldErrors';
import type { ErrorGate } from './useErrorGate';
import { ruleUid } from '../../ruleUid';
import RowActionsMenu from '../RowActionsMenu';
import { draftRemoveAction } from './draftRemoveAction';
import { cloneRuleAction } from './cloneRuleAction';
import { useColors } from '../../ThemeContext';
import './OutcomeMeter.css';
import type { LifecycleConfig, ReplicationConfig } from '../../adminApi';
import type { JobRow, LifecycleVerb } from '../../jobsView';
import {
  availableActions,
  cancelConfirmText,
  DRAFT_KEY_MARK,
  findEditorRule,
  UNNAMED_RULE,
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
import FailuresList from './FailuresList';
import OutcomeMeter from './OutcomeMeter';
import ReplicationRuleFields from '../ReplicationRuleFields';
import LifecycleRuleFields from '../LifecycleRuleFields';
import { sameScopeRule } from '../ruleClone';
import VerifyTab from './VerifyTab';
import LifecyclePreviewTab from './LifecyclePreviewTab';
import { useBucketNames } from '../../queries/backends';
import TruncatedName from '../TruncatedName';

const { Text } = Typography;

/** A rule editor as the drawer sees it. `saved` is the editor's last server
 *  snapshot: a server job id names the saved rule, and the editor rule is
 *  the one with that rule's uid. */
export interface RuleEditor<C> {
  value: C;
  saved: C;
  onChange: (fn: (cur: C) => C) => void;
  /** When a field error shows (keys `<rule uid>:<field>`); the page owns it
   *  so its dirty bar counts the errors on screen. */
  gate?: ErrorGate;
}

/** The errors of one rule that its gate shows now. */
function shownErrors(all: RuleFieldErrors, uid: string, gate: ErrorGate | undefined): RuleFieldErrors {
  const out: RuleFieldErrors = {};
  for (const [field, error] of Object.entries(all)) if (error && (gate?.show(`${uid}:${field}`) ?? true)) out[field] = error;
  return out;
}

interface Props {
  jobId: string | null;
  rows: JobRow[];
  /** The rule editor of the page (a rule page has one; the Jobs page none). */
  replication?: RuleEditor<ReplicationConfig>;
  lifecycle?: RuleEditor<LifecycleConfig>;
  /** Drop a draft (a rule that exists only in the unsaved edits). */
  onRemoveDraft?: (draft: Pick<JobRow, 'id' | 'kind' | 'name'>) => void;
  /** Copy the shown rule into a new draft. */
  onCloneRule?: (rule: Pick<JobRow, 'id' | 'kind' | 'name'>) => void;
  /** Cancel a running one-off (asked for confirmation here first). */
  onCancelJob?: (row: JobRow) => Promise<unknown> | void;
  /** What a lifecycle rule does to an object (meter words). */
  lifecycleVerbOf?: (row: JobRow) => LifecycleVerb | undefined;
  /** Pinned under the body: the rule page's unsaved-changes bar, so it sits
   *  inside the drawer instead of half under its edge. */
  footer?: React.ReactNode;
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

/** The drawer title: one line; a long rule name is cut in the middle (D5). */
const TITLE_ROW: CSSProperties = { display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 };

export default function JobDrawer({
  jobId,
  rows,
  replication: replEditor,
  lifecycle: lcEditor,
  onRemoveDraft,
  onCloneRule,
  onCancelJob,
  lifecycleVerbOf,
  footer,
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

  // The editor rule this drawer shows: by uid, never by name (a name
  // changes while the operator types it, and can match another rule).
  const replication = replEditor?.value;
  const lifecycle = lcEditor?.value;
  const replIndex =
    parsed?.subsystem === 'replication' && replEditor
      ? findEditorRule(parsed.key, replEditor.value.rules, replEditor.saved.rules)
      : -1;
  const lcIndex =
    parsed?.subsystem === 'lifecycle' && lcEditor
      ? findEditorRule(parsed.key, lcEditor.value.rules, lcEditor.saved.rules)
      : -1;
  const editorRuleName =
    replication && replIndex >= 0
      ? replication.rules[replIndex].name
      : lifecycle && lcIndex >= 0
        ? lifecycle.rules[lcIndex].name
        : null;
  // What the title calls a rule that the server does not know yet.
  const draftName = editorRuleName ?? (parsed && !parsed.key.startsWith(DRAFT_KEY_MARK) ? parsed.key : '');
  const draftLabel = draftName || UNNAMED_RULE;

  const definition = (() => {
    if (replEditor && replication && replIndex >= 0) {
      const rule = replication.rules[replIndex];
      const twin = sameScopeRule('replication', replication.rules, replIndex);
      const uid = ruleUid(rule);
      return (
        <>
          {twin && <SameScopeAlert twin={twin} what="source, destination and filters" />}
          <ReplicationRuleFields
            rule={rule}
            buckets={bucketNames}
            inputRadius={inputRadius}
            errors={shownErrors(replicationRuleErrors(rule, replication.rules), uid, replEditor.gate)}
            onFieldBlur={(field) => replEditor.gate?.touch(`${uid}:${field}`)}
            onChange={(patch) =>
              replEditor.onChange((cur) => ({
                ...cur,
                rules: cur.rules.map((r, i) => (i === replIndex ? { ...r, ...patch } : r)),
              }))
            }
            onRename={(next) =>
              replEditor.onChange((cur) => ({
                ...cur,
                rules: cur.rules.map((r, i) => (i === replIndex ? { ...r, name: next } : r)),
              }))
            }
          />
        </>
      );
    }
    if (lcEditor && lifecycle && lcIndex >= 0) {
      const rule = lifecycle.rules[lcIndex];
      const twin = sameScopeRule('lifecycle', lifecycle.rules, lcIndex);
      const uid = ruleUid(rule);
      return (
        <>
          {twin && <SameScopeAlert twin={twin} what="bucket, prefix, action and filters" />}
          <LifecycleRuleFields
            rule={rule}
            buckets={bucketNames}
            inputRadius={inputRadius}
            errors={shownErrors(lifecycleRuleErrors(rule, lifecycle.rules), uid, lcEditor.gate)}
            onFieldBlur={(field) => lcEditor.gate?.touch(`${uid}:${field}`)}
            onChange={(patch) =>
              lcEditor.onChange((cur) => ({
                ...cur,
                rules: cur.rules.map((r, i) => (i === lcIndex ? { ...r, ...patch } : r)),
              }))
            }
            onRename={(next) =>
              lcEditor.onChange((cur) => ({
                ...cur,
                rules: cur.rules.map((r, i) => (i === lcIndex ? { ...r, name: next } : r)),
              }))
            }
          />
        </>
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
            <div key={k} style={{ display: 'flex', gap: 12, padding: '8px 0' }}>
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
      return fetched.isLoading ? (
        <LoadingState variant="form" label="Loading the job" />
      ) : (
        <EmptyState
          icon={<QuestionCircleOutlined />}
          title="This job does not exist on this proxy"
          hint="The proxy keeps a limited history of one-off jobs. Close this panel to see the jobs it knows."
        />
      );
    }
    // A rule-kind jobId that resolves to NEITHER a live editor rule NOR a server
    // row is a stale draft reference (the draft was discarded or renamed while
    // the drawer pointed at it). Don't auto-close: show "this job no longer
    // exists" on a shared/deep-linked URL instead of a silent close.
    return (
      <EmptyState
        icon={<QuestionCircleOutlined />}
        title="This rule no longer exists"
        hint="It was removed, or its unsaved draft was discarded. Close this panel."
      />
    );
  })();

  const runsTable = (
    <RecordList
      rows={liveRuns}
      rowKey={(r) => String(r.id)}
      empty={<EmptyState compact icon={<HistoryOutlined />} title="No runs yet" hint="A run shows here when the rule runs on its schedule or with Run now." />}
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
          key: 'finished',
          label: 'Finished',
          track: 'max-content',
          render: (r) => (
            <Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
              {r.finished_at ? <TimeAgo ts={r.finished_at} /> : isActiveJobStatus(r.status) ? 'running' : '—'}
            </Text>
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

  const failuresTable = <FailuresList data={failuresQuery.data} />;

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
      footer={footer}
      extra={
        parsed && ruleKind && onCloneRule && onRemoveDraft ? (
          <RowActionsMenu
            label={`More actions for ${draftLabel}`}
            actions={[
              cloneRuleAction({ id: jobId ?? '', kind: ruleKind, name: draftLabel }, onCloneRule),
              ...(serverRow
                ? []
                : [draftRemoveAction({ id: jobId ?? '', kind: ruleKind, name: draftLabel }, onRemoveDraft)]),
            ]}
          />
        ) : undefined
      }
      title={
        serverRow ? (
          <span style={TITLE_ROW}>
            <span style={{ flexShrink: 0 }}>{kindLabel(serverRow.kind)} ·</span>
            <TruncatedName text={serverRow.name} style={{ fontFamily: 'var(--font-mono)' }} />
            <Tag color={jobStatusTone(serverRow)} style={{ margin: 0, flexShrink: 0 }}>
              {jobStatusLabel(serverRow)}
            </Tag>
          </span>
        ) : parsed && ruleKind ? (
          <span style={TITLE_ROW}>
            <span style={{ flexShrink: 0 }}>{kindLabel(ruleKind)} ·</span>
            <TruncatedName text={draftLabel} style={{ fontFamily: 'var(--font-mono)' }} />
            <Tag color="warning" style={{ margin: 0, flexShrink: 0 }}>
              draft
            </Tag>
          </span>
        ) : (
          parsed && (
            <span style={TITLE_ROW}>
              <span style={{ flexShrink: 0 }}>Job ·</span>
              <TruncatedName text={parsed.key} style={{ fontFamily: 'var(--font-mono)' }} />
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
            <TruncatedName text={walk.scanning ?? '…'} style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }} />
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
              marginTop: 8,
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
              <TruncatedName text={f.key} style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }} />
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

/** Two rules that act on the same objects: usually a clone not yet pointed elsewhere. */
function SameScopeAlert({ twin, what }: { twin: string; what: string }) {
  return (
    <Alert
      type="warning"
      showIcon
      style={{ marginBottom: 16 }}
      title={`Rule "${twin}" has the same ${what}`}
      description="Both rules would act on the same objects. Change a bucket, a prefix or a filter before you apply."
    />
  );
}
