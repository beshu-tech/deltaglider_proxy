/**
 * Jobs — ONE screen for every background operation: replication rules,
 * lifecycle rules, and one-off re-encrypt / migrate jobs.
 *
 * Reads come from the unified GET /api/admin/jobs (adaptive 2s polling
 * while anything runs). Rule DEFINITIONS are still YAML config: this
 * panel hosts TWO section editors (replication + lifecycle, both on the
 * `storage` section with disjoint `{replication}` / `{lifecycle}`
 * merge-patches — verified safe to be dirty simultaneously) and ONE
 * dirty bar driving a SEQUENTIAL apply queue: replication's ApplyDialog
 * first, then lifecycle's. Cancelling either step keeps the remaining
 * edits dirty — nothing is ever auto-discarded.
 *
 * One-off jobs are DB-born (created via the modals), not config: they
 * have no dirty state, just live progress + cancel.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Alert, Button, Space, Spin, Tag, Typography, message } from 'antd';
import Dropdown from '../KeyboardDropdown';
import {
  CaretRightOutlined,
  DeleteOutlined,
  EyeOutlined,
  PauseOutlined,
  PlayCircleOutlined,
  PlusOutlined,
  StopOutlined,
  ThunderboltOutlined,
} from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import type { LifecycleConfig, ReplicationConfig, StorageSectionBody } from '../../adminApi';
import { runJobAction } from '../../adminApi';
import type { JobAction, JobDisplayRow, JobRow } from '../../jobsView';
import {
  availableActions,
  editorAfterDiscard,
  isActiveJobStatus,
  jobStatusLabel,
  jobStatusTone,
  kindLabel,
  kindTone,
  mergeDraftRules,
  draftBlocksAction,
  planRuleDeleteSync,
  runNowMessage,
  triggerLabel,
} from '../../jobsView';
import { qk } from '../../queries/keys';
import { useJobs } from '../../queries/jobs';
import { useNavigation } from '../../NavigationContext';
import { buildViewUrl, parseAdminQuery } from '../../urlState';
import { useOverlayClose } from '../../hooks/useOverlayClose';
import TimeAgo from '../TimeAgo';
import RecordList, { type RecordColumn } from './RecordList';
import RowActionsMenu, { type RowActionConfirm } from '../RowActionsMenu';
import OutcomeMeter from './OutcomeMeter';
import { useSectionEditor } from '../../useSectionEditor';
import { useApplyHandler } from '../../useDirtySection';
import { useCardStyles, contentColumn, CONTENT_WIDE } from '../shared-styles';
import ApplyDialog from '../ApplyDialog';
import StickyDirtyBar from '../StickyDirtyBar';
import ReencryptProposalModal from '../ReencryptProposalModal';
import MigrateBucketModal from '../MigrateBucketModal';
import JobDrawer from './JobDrawer';
import LifecycleRunConfirm from './LifecycleRunConfirm';
import {
  buildReplicationPayload,
  DEFAULT_REPLICATION,
  emptyRule as emptyReplicationRule,
  normalizeReplication,
} from '../replicationPayload';
import {
  buildLifecyclePayload,
  DEFAULT_LIFECYCLE,
  emptyRule as emptyLifecycleRule,
  normalizeLifecycle,
} from '../lifecyclePayload';
import ReplicationApplySummary from '../ReplicationApplySummary';
import { LifecycleApplySummary } from '../LifecycleSummary';
import { normalizeUiError } from '../../errorHandling';

const { Text } = Typography;

interface Props {
  onSessionExpired?: () => void;
  /** Raw query string (with leading `?`) for deep-linking (?job=…&tab=…). */
  search?: string;
}

const ACTION_META: Record<
  JobAction,
  { label: string; icon: React.ReactNode; danger?: boolean; done?: string }
> = {
  pause: { label: 'Pause', icon: <PauseOutlined />, done: 'Paused' },
  // Resume (un-pause the schedule) keeps the bare play caret; Run now/once
  // (fire a single execution) uses a DISTINCT circled-play so the two icons
  // don't collide on a paused rule that offers both.
  resume: { label: 'Resume', icon: <CaretRightOutlined />, done: 'Resumed' },
  'run-now': { label: 'Run now', icon: <PlayCircleOutlined /> },
  preview: { label: 'Preview', icon: <EyeOutlined /> },
  cancel: {
    label: 'Cancel',
    icon: <StopOutlined />,
    danger: true,
    done: 'Cancellation requested — the job stops at the next safe point',
  },
  kill: {
    label: 'Kill run',
    icon: <ThunderboltOutlined />,
    danger: true,
    done: 'Kill requested — the running sweep aborts in-flight',
  },
  delete: {
    label: 'Delete',
    icon: <DeleteOutlined />,
    danger: true,
    done: 'Rule deleted',
  },
};

/** Destructive row actions: shown in the row's "⋯" menu, not as buttons. */
type MenuAction = 'kill' | 'delete';
const MENU_ACTIONS: readonly JobAction[] = ['kill', 'delete'] satisfies MenuAction[];
/** The confirmation each menu action asks before it runs. */
const MENU_CONFIRM: Record<MenuAction, (name: string) => RowActionConfirm> = {
  delete: (name) => ({
    title: `Delete rule "${name}"?`,
    content: 'This removes the rule from the configuration and clears its run history.',
    okText: 'Delete rule',
  }),
  // Kill aborts in-flight work mid-object.
  kill: (name) => ({
    title: `Kill the running "${name}" run?`,
    content: 'Transfers in progress stop immediately.',
    okText: 'Kill run',
  }),
};

export default function JobsPanel({ onSessionExpired, search }: Props) {
  const { cardStyle, inputRadius } = useCardStyles();
  const qc = useQueryClient();
  const [messageApi, msgCtx] = message.useMessage();
  const { navigate } = useNavigation();
  const { markPushed, closeOverlay } = useOverlayClose();

  const jobsQuery = useJobs();
  const serverRows: JobRow[] = useMemo(
    () => jobsQuery.data?.jobs ?? [],
    [jobsQuery.data]
  );

  // ── The two rule-definition editors (disjoint storage merge-patches). ──
  const repl = useSectionEditor<StorageSectionBody, ReplicationConfig>({
    section: 'storage',
    dirtyKey: 'jobs/replication',
    initial: DEFAULT_REPLICATION,
    onSessionExpired,
    noun: 'replication',
    pick: (body) => normalizeReplication(body.replication),
    toPayload: (v) => {
      const res = buildReplicationPayload(v);
      return res.ok ? res.body : {};
    },
  });
  const lc = useSectionEditor<StorageSectionBody, LifecycleConfig>({
    section: 'storage',
    dirtyKey: 'jobs/lifecycle',
    initial: DEFAULT_LIFECYCLE,
    onSessionExpired,
    noun: 'lifecycle',
    pick: (body) => normalizeLifecycle(body.lifecycle),
    toPayload: (v) => {
      const res = buildLifecyclePayload(v);
      return res.ok ? res.body : {};
    },
  });

  // True after a rule was deleted on the server while that editor was dirty:
  // the rule was filtered out of the value, but the baseline still holds it,
  // so Discard must re-fetch instead of reverting to the baseline.
  const replBaselineStale = useRef(false);
  const lcBaselineStale = useRef(false);

  const syncEditorAfterDelete = (row: JobRow) => {
    if (row.kind === 'replication') {
      const plan = planRuleDeleteSync(repl.isDirty, repl.value.rules, row.name);
      if (plan.action === 'refresh') {
        void repl.refresh();
      } else {
        repl.setValue((cur) => ({ ...cur, rules: cur.rules.filter((r) => r.name !== row.name) }));
        replBaselineStale.current = true;
      }
    } else if (row.kind === 'lifecycle') {
      const plan = planRuleDeleteSync(lc.isDirty, lc.value.rules, row.name);
      if (plan.action === 'refresh') {
        void lc.refresh();
      } else {
        lc.setValue((cur) => ({ ...cur, rules: cur.rules.filter((r) => r.name !== row.name) }));
        lcBaselineStale.current = true;
      }
    }
  };

  const discardEditors = () => {
    for (const [ed, stale] of [
      [repl, replBaselineStale],
      [lc, lcBaselineStale],
    ] as const) {
      if (editorAfterDiscard(stale.current) === 'refresh') {
        stale.current = false;
        void ed.refresh();
      } else {
        ed.discard();
      }
    }
  };

  // ── Sequential apply queue: replication dialog → lifecycle dialog. ──
  // 'lifecycle-pending' means: after replication confirms, open lifecycle.
  const [queueLifecycleNext, setQueueLifecycleNext] = useState(false);
  const startApplyQueue = useCallback(async () => {
    // Client-side validation for BOTH before any dialog opens.
    if (repl.isDirty) {
      const r = buildReplicationPayload(repl.value);
      if (!r.ok) {
        messageApi.error(r.error);
        return;
      }
    }
    if (lc.isDirty) {
      const l = buildLifecyclePayload(lc.value);
      if (!l.ok) {
        messageApi.error(l.error);
        return;
      }
    }
    if (repl.isDirty) {
      setQueueLifecycleNext(lc.isDirty);
      await repl.runApply();
    } else if (lc.isDirty) {
      await lc.runApply();
    }
  }, [repl, lc, messageApi]);

  const anyDirty = repl.isDirty || lc.isDirty;
  useApplyHandler('jobs', startApplyQueue, anyDirty);

  const confirmReplApply = useCallback(async () => {
    const ok = await repl.confirmApply();
    qc.invalidateQueries({ queryKey: qk.jobs.list() });
    if (!ok) {
      // Replication PUT failed: abort the queue. The lifecycle edits stay
      // dirty (nothing is discarded) and its dialog must NOT open stacked
      // on top of the failure the operator is looking at.
      setQueueLifecycleNext(false);
      return;
    }
    // A successful apply re-fetches the section: the baseline is fresh.
    replBaselineStale.current = false;
    if (queueLifecycleNext) {
      setQueueLifecycleNext(false);
      // Open the lifecycle dialog as the next step of the queue.
      await lc.runApply();
    }
  }, [repl, lc, qc, queueLifecycleNext]);

  const cancelReplApply = useCallback(() => {
    // Aborting step 1 aborts the queue; BOTH edit sets stay dirty.
    setQueueLifecycleNext(false);
    repl.cancelApply();
  }, [repl]);

  const confirmLcApply = useCallback(async () => {
    if (await lc.confirmApply()) lcBaselineStale.current = false;
    qc.invalidateQueries({ queryKey: qk.jobs.list() });
  }, [lc, qc]);

  // ── Display rows: server rows + drafts/pending-deletes overlay. ──
  const displayRows: JobDisplayRow[] = useMemo(
    () => mergeDraftRules(serverRows, repl.value.rules, lc.value.rules),
    [serverRows, repl.value.rules, lc.value.rules]
  );

  // ── Drawer + creation modals ──
  // The drawer's open job and active tab are URL-deep-linked via ?job=…&tab=…
  // so shared links, browser Back/Forward, and reloads all land on the right
  // view. On first mount we read from the query string; thereafter the URL is
  // the single source of truth.
  const queryParams = useMemo(() => parseAdminQuery(search ?? ''), [search]);
  const [drawerJobId, setDrawerJobId] = useState<string | null>(queryParams.job ?? null);
  const [drawerTab, setDrawerTab] = useState<string>(queryParams.tab ?? 'definition');
  const [newJobMenuOpen, setNewJobMenuOpen] = useState(false);
  const [reencryptOpen, setReencryptOpen] = useState(false);
  const [migrateOpen, setMigrateOpen] = useState(false);
  const [actionBusy, setActionBusy] = useState<string | null>(null);
  const [confirmRun, setConfirmRun] = useState<JobRow | null>(null);

  // Sync URL → state: when the query string changes (Back/Forward / shared link),
  // update the local drawer state.
  useEffect(() => {
    setDrawerJobId(queryParams.job ?? null);
    setDrawerTab(queryParams.tab ?? 'definition');
  }, [queryParams.job, queryParams.tab]);

  // Sync state → URL: when the drawer opens/closes or tab changes, update the
  // URL so it's shareable and Back-button navigable. We replace (not push) for
  // tab changes to avoid history spam; push for open/close transitions.
  const updateDrawerUrl = useCallback(
    (job: string | null, tab: string) => {
      const query: Record<string, string> = {};
      if (job) query.job = job;
      if (job && tab && tab !== 'definition') query.tab = tab;
      const url = buildViewUrl('admin', 'jobs', Object.keys(query).length > 0 ? query : undefined);
      navigate(url, { replace: true });
    },
    [navigate],
  );

  const openDrawer = useCallback(
    (jobId: string, tab = 'definition') => {
      setDrawerJobId(jobId);
      setDrawerTab(tab);
      const url = buildViewUrl('admin', 'jobs', tab === 'definition' ? { job: jobId } : { job: jobId, tab });
      navigate(url);
      markPushed();
    },
    [navigate, markPushed],
  );

  // Rename retarget: same as openDrawer but REPLACE (not push) so typing
  // 5 characters doesn't create 5 history entries.  The drawer is already
  // open; we're just updating which job it points at.
  const retargetDrawer = useCallback(
    (jobId: string) => {
      setDrawerJobId(jobId);
      const url = buildViewUrl('admin', 'jobs', { job: jobId, ...(drawerTab !== 'definition' ? { tab: drawerTab } : {}) });
      navigate(url, { replace: true });
    },
    [navigate, drawerTab],
  );

  const handleDrawerClose = useCallback(() => {
    setDrawerJobId(null);
    setDrawerTab('definition');
    closeOverlay(buildViewUrl('admin', 'jobs'), navigate);
  }, [closeOverlay, navigate]);

  const handleTabChange = useCallback(
    (tab: string) => {
      setDrawerTab(tab);
      if (drawerJobId) updateDrawerUrl(drawerJobId, tab);
    },
    [drawerJobId, updateDrawerUrl],
  );

  const runAction = async (row: JobRow, action: JobAction, confirmed = false) => {
    // A lifecycle run deletes or moves objects: confirm with the preview list.
    if (action === 'run-now' && row.kind === 'lifecycle' && !confirmed) {
      setConfirmRun(row);
      return;
    }
    // Preview is a view, not a toast: the drawer's Preview tab lists the keys.
    if (action === 'preview') {
      void qc.invalidateQueries({ queryKey: qk.jobs.preview(row.id) });
      openDrawer(row.id, 'preview');
      return;
    }
    setActionBusy(`${row.id}:${action}`);
    try {
      const result = await runJobAction(row.id, action);
      if (action === 'run-now') {
        messageApi.success(runNowMessage(row.kind, result));
      } else {
        messageApi.success(ACTION_META[action].done ?? `${ACTION_META[action].label} OK`);
      }
      // A deleted rule no longer exists — close its drawer.
      if (action === 'delete' && drawerJobId === row.id) handleDrawerClose();
      // …and drop it from its editor, or it comes back as a draft that the
      // next Apply re-creates.
      if (action === 'delete') syncEditorAfterDelete(row);
      // Refresh the list AND this job's runs/failures tables — a resume/run-now
      // starts a new run that the open drawer's Runs/Failures tabs must show.
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
      qc.invalidateQueries({ queryKey: qk.jobs.runs(row.id) });
      qc.invalidateQueries({ queryKey: qk.jobs.failures(row.id) });
    } catch (e) {
      messageApi.error(normalizeUiError(e, `${action} failed`));
    } finally {
      setActionBusy(null);
    }
  };

  const newJobMenu = {
    items: [
      { key: 'replication', label: 'Replication rule — continuous copy' },
      { key: 'lifecycle', label: 'Lifecycle rule — scheduled expiry / archive' },
      { type: 'divider' as const },
      { key: 'reencrypt', label: 'Re-encrypt buckets… — one-off rewrite' },
      { key: 'migrate', label: 'Migrate bucket… — one-off move' },
    ],
    onClick: ({ key }: { key: string }) => {
      // Close the menu explicitly: opening a drawer / modal synchronously in
      // these handlers otherwise leaves the dropdown lingering over the drawer
      // until the next click.
      setNewJobMenuOpen(false);
      if (key === 'replication') {
        const rule = emptyReplicationRule(repl.value.rules);
        repl.setValue((cur) => ({ ...cur, rules: [...cur.rules, rule] }));
        openDrawer(`replication:${rule.name}`);
      } else if (key === 'lifecycle') {
        const rule = emptyLifecycleRule(lc.value.rules);
        lc.setValue((cur) => ({ ...cur, rules: [...cur.rules, rule] }));
        openDrawer(`lifecycle:${rule.name}`);
      } else if (key === 'reencrypt') {
        setReencryptOpen(true);
      } else {
        setMigrateOpen(true);
      }
    },
  };

  const columns: RecordColumn<JobDisplayRow>[] = [
    {
      key: 'job',
      label: 'Job',
      // The name is what operators scan for; Status is usually one short tag.
      track: 'minmax(200px,2fr)',
      render: (d) => (
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
          <Tag color={kindTone(d.row.kind)} style={{ margin: 0, flexShrink: 0 }}>
            {kindLabel(d.row.kind)}
          </Tag>
          <Text
            strong
            // AntD tooltips are hidden globally (theme.css): native title.
            ellipsis
            title={d.row.name}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 13, minWidth: 0 }}
          >
            {d.row.name}
          </Text>
          {d.draft && <Tag color="warning" style={{ margin: 0, flexShrink: 0 }}>draft</Tag>}
          {d.pendingDelete && <Tag color="error" style={{ margin: 0, flexShrink: 0 }}>removing</Tag>}
        </div>
      ),
    },
    {
      key: 'scope',
      label: 'Scope',
      track: 'minmax(140px,1.2fr)',
      render: (d) => {
        const scope = `${d.row.scope.bucket}${d.row.scope.prefix ? `/${d.row.scope.prefix}` : ''}${
          d.row.scope.target ? ` → ${d.row.scope.target}` : ''
        }`;
        return (
          <Text
            type="secondary"
            className="dg-cell-truncate"
            title={scope}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}
          >
            {scope || '—'}
          </Text>
        );
      },
    },
    {
      key: 'trigger',
      label: 'Trigger',
      track: 'max-content',
      render: (d) => (
        <Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
          {triggerLabel(d.row.trigger)}
        </Text>
      ),
    },
    {
      key: 'status',
      label: 'Status',
      track: 'minmax(0,1fr)',
      render: (d) => {
        const live = d.row.trigger === 'oneoff' && isActiveJobStatus(d.row.status);
        return (
          <div style={{ minWidth: 0 }}>
            {live ? (
              <OutcomeMeter
                scanned={d.row.progress.processed + d.row.progress.skipped + d.row.progress.failed}
                copied={d.row.progress.processed}
                errors={d.row.progress.failed}
                skipped={d.row.progress.skipped}
                status={d.row.status}
                percent={d.row.percent ?? null}
              />
            ) : (
              <Tag color={jobStatusTone(d.row)} style={{ margin: 0 }}>
                {jobStatusLabel(d.row)}
              </Tag>
            )}
            {!live && d.row.last_error && (
              <Text type="danger" style={{ display: 'block', fontSize: 11, marginTop: 2 }} ellipsis title={d.row.last_error}>
                {d.row.last_error}
              </Text>
            )}
          </div>
        );
      },
    },
    {
      key: 'last',
      label: 'Last run',
      track: 'max-content',
      render: (d) => {
        const ts = d.row.last_run_at ?? d.row.finished_at ?? d.row.started_at;
        return (
          <Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
            <TimeAgo ts={ts} />
          </Text>
        );
      },
    },
    {
      key: 'actions',
      label: 'Actions',
      track: 'max-content',
      align: 'end',
      hideLabelOnNarrow: true,
      render: (d) =>
        d.draft ? (
          <span />
        ) : (
          <Space size={2} onClick={(e) => e.stopPropagation()}>
            {availableActions(d.row)
              .filter((a) => !MENU_ACTIONS.includes(a))
              .map((a) => {
                // A one-off on a disabled/paused rule reads as "Run once" (it runs
                // the rule a single time without re-enabling/resuming it).
                const oneOff =
                  a === 'run-now' && (d.row.enabled === false || d.row.paused === true);
                const label = oneOff ? 'Run once' : ACTION_META[a].label;
                const blocked = draftBlocksAction(a, d.row.kind, {
                  replication: repl.isDirty,
                  lifecycle: lc.isDirty,
                });
                const title = oneOff
                  ? 'Run this rule once now — does not enable or resume it'
                  : label;
                return (
                  <Button
                    key={a}
                    size="small"
                    type="text"
                    danger={ACTION_META[a].danger}
                    icon={ACTION_META[a].icon}
                    loading={actionBusy === `${d.row.id}:${a}`}
                    disabled={blocked !== null}
                    title={blocked ?? title}
                    // Unique per row: every row has a "Run now".
                    aria-label={`${title}: ${d.row.name}`}
                    onClick={() => void runAction(d.row, a)}
                  >
                    {/* Icon-only on the wide table (label in the tooltip); the
                        caption returns on the narrow stacked card. */}
                    <span className="dg-action-label">{label}</span>
                  </Button>
                );
              })}
            {/* Destructive actions (delete, kill) live behind "⋯", not as a
                red icon on every row; the menu confirms each one. */}
            <RowActionsMenu
              label={`More actions for ${d.row.name}`}
              loading={MENU_ACTIONS.some((a) => actionBusy === `${d.row.id}:${a}`)}
              actions={availableActions(d.row)
                .filter((a) => MENU_ACTIONS.includes(a))
                .map((a) => ({
                  key: a,
                  danger: true as const,
                  icon: ACTION_META[a].icon,
                  label: `${ACTION_META[a].label}…`,
                  confirm: MENU_CONFIRM[a as MenuAction](d.row.name),
                  onSelect: () => runAction(d.row, a),
                }))}
            />
          </Space>
        ),
    },
  ];

  if (jobsQuery.isLoading) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}>
        <Spin />
      </div>
    );
  }

  return (
    <div style={contentColumn(CONTENT_WIDE)}>
      {msgCtx}
      <ReencryptProposalModal
        open={reencryptOpen}
        transition="encrypt"
        backendName=""
        buckets={[]}
        pickBuckets
        onClose={() => setReencryptOpen(false)}
      />
      <MigrateBucketModal
        open={migrateOpen}
        bucket={null}
        onClose={() => setMigrateOpen(false)}
      />
      <LifecycleRunConfirm
        row={confirmRun}
        onCancel={() => setConfirmRun(null)}
        onRun={(r) => {
          setConfirmRun(null);
          void runAction(r, 'run-now', true);
        }}
      />
      <JobDrawer
        jobId={drawerJobId}
        rows={serverRows}
        replication={repl.value}
        lifecycle={lc.value}
        onReplicationChange={repl.setValue}
        onLifecycleChange={lc.setValue}
        onJobIdChange={retargetDrawer}
        activeTab={drawerTab}
        onTabChange={handleTabChange}
        inputRadius={inputRadius}
        onClose={handleDrawerClose}
      />

      <div style={cardStyle}>
        {/* Lean toolbar — the page TabHeader already carries the "Jobs" title +
            description, so this row is just the count + the action (no duplicate
            heading). Keeps one header per screen and saves vertical space. */}
        <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 12 }}>
          <Text type="secondary" style={{ fontSize: 13 }}>
            {displayRows.length} job{displayRows.length === 1 ? '' : 's'} — tap one for definition,
            runs, and failures.
          </Text>
          <Dropdown
            menu={newJobMenu}
            trigger={['click']}
            open={newJobMenuOpen}
            onOpenChange={setNewJobMenuOpen}
          >
            <Button type="primary" icon={<PlusOutlined />}>
              New job
            </Button>
          </Dropdown>
        </div>

        {jobsQuery.error ? (
          <Alert
            type="error"
            showIcon
            style={{ marginTop: 16, borderRadius: 8 }}
            title={normalizeUiError(jobsQuery.error, 'Failed to load jobs')}
          />
        ) : (
          <div style={{ marginTop: 16 }}>
            <RecordList
              rows={displayRows}
              columns={columns}
              rowKey={(d) => d.row.id}
              onRowClick={(d) => openDrawer(d.row.id)}
              empty='No jobs yet. Use "New job" to add a replication or lifecycle rule, or start a one-off re-encrypt or migrate job.'
            />
          </div>
        )}
      </div>

      <StickyDirtyBar
        visible={anyDirty}
        applying={repl.applying || lc.applying}
        onDiscard={discardEditors}
        onApply={() => void startApplyQueue()}
        floating
      />
      <ApplyDialog
        open={repl.applyOpen}
        section="storage"
        response={repl.applyResponse}
        onApply={() => void confirmReplApply()}
        onCancel={cancelReplApply}
        loading={repl.applying}
        summary={<ReplicationApplySummary replication={repl.pendingBody?.replication ?? repl.value} />}
      />
      <ApplyDialog
        open={lc.applyOpen}
        section="storage"
        response={lc.applyResponse}
        onApply={() => void confirmLcApply()}
        onCancel={lc.cancelApply}
        loading={lc.applying}
        summary={<LifecycleApplySummary lifecycle={lc.pendingBody?.lifecycle ?? lc.value} />}
      />
    </div>
  );
}
