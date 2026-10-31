/**
 * A rules page: the replication rules or the lifecycle rules — definitions
 * that run many times. Their runs, and the one-off jobs, are on the Jobs
 * page (RunsPanel); a rule's "runs →" link opens it filtered to the rule.
 *
 * Rule definitions are YAML config: the page hosts ONE section editor (the
 * `{replication}` or `{lifecycle}` merge-patch of the `storage` section) and
 * ONE dirty bar. The rows come from GET /jobs, with the editor's drafts and
 * pending deletes laid over them.
 */
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Alert, Button, Input, Space, Switch, Tag, Typography, message } from 'antd';
import {
  CaretRightOutlined,
  DeleteOutlined,
  EyeOutlined,
  PauseOutlined,
  PlayCircleOutlined,
  PlusOutlined,
  ScheduleOutlined,
  StopOutlined,
  ThunderboltOutlined,
  UndoOutlined,
} from '@ant-design/icons';
import { useQueryClient } from '@tanstack/react-query';
import type {
  LifecycleConfig,
  LifecycleRuleConfig,
  ReplicationConfig,
  ReplicationRuleConfig,
  StorageSectionBody,
} from '../../adminApi';
import { getJobRuns, runJobAction } from '../../adminApi';
import type { JobAction, JobDisplayRow, JobRow, JobWatch } from '../../jobsView';
import {
  availableActions,
  editedRuleIds,
  editorRuleJobId,
  findEditorRule,
  ruleIdsChangedByApply,
  savedRulesChangedByApply,
  DRAFT_KEY_MARK,
  isActiveJobStatus,
  jobStatusLabel,
  jobStatusTone,
  lastRunCell,
  lifecycleVerb,
  mergeDraftRules,
  draftBlocksAction,
  lifecycleRunNowBlock,
  nextRunText,
  deferredText,
  liveCopySummary,
  rowMeterInput,
  runEndedMessage,
  statusCellView,
  runNowMessage,
  scopeText,
  startJobWatch,
  stepJobWatch,
  triggerLabel,
  watchSettled,
  parseJobId,
} from '../../jobsView';
import { carryRuleUids, ruleUid } from '../../ruleUid';
import { tickPeriodMs, useNow } from '../../hooks/useNow';
import { qk } from '../../queries/keys';
import { useJobs } from '../../queries/jobs';
import { useNavigation } from '../../NavigationContext';
import { buildViewUrl, parseAdminQuery } from '../../urlState';
import { useOverlayClose } from '../../hooks/useOverlayClose';
import TimeAgo from '../TimeAgo';
import RecordList, { type RecordColumn } from './RecordList';
import RowActionsMenu, { type RowActionConfirm } from '../RowActionsMenu';
import { cloneRule } from '../ruleClone';
import { draftRemoveAction } from './draftRemoveAction';
import { cloneRuleAction } from './cloneRuleAction';
import OutcomeMeter from './OutcomeMeter';
import { useSectionEditor } from '../../useSectionEditor';
import { useApplyHandler } from '../../useDirtySection';
import { useCardStyles, contentColumn, CONTENT_WIDE } from '../shared-styles';
import ApplyDialog from '../ApplyDialog';
import StickyDirtyBar from '../StickyDirtyBar';
import JobDrawer, { type RuleEditor } from './JobDrawer';
import LifecycleRunConfirm from './LifecycleRunConfirm';
import { lifecycleRunResultText, newestSettledRun } from './lifecycleText';
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
import { plainError } from '../../plainError';
import FormField from '../FormField';
import PageActions from '../PageActions';
import { EmptyState, ErrorState, LoadingState } from '../StatePlaceholders';
import { runsUrl } from './runsLink';
import { durationError } from './humanDuration';
import { lifecycleRuleErrors, replicationRuleErrors, type RuleFieldErrors } from './ruleFieldErrors';
import { useErrorGate } from './useErrorGate';
import TruncatedName from '../TruncatedName';

const { Text } = Typography;

type RuleKind = 'replication' | 'lifecycle';

/** The rules config of one kind: a master switch, a tick, and the rules. */
interface RulesConfig<R> {
  enabled: boolean;
  tick_interval: string;
  rules: R[];
}

type PayloadResult = { ok: true; body: StorageSectionBody } | { ok: false; error: string };

/** What differs between the replication page and the lifecycle page. */
interface RuleKindSpec<C extends RulesConfig<R>, R extends { name: string; enabled: boolean }> {
  kind: RuleKind;
  /** The admin path of the page. */
  path: string;
  initial: C;
  normalize: (body: StorageSectionBody) => C;
  build: (value: C) => PayloadResult;
  emptyRule: (existing: R[]) => R;
  /** The errors of one rule, by field (shown under the fields in the drawer). */
  fieldErrors: (rule: R, rules: readonly R[]) => RuleFieldErrors;
  /** The editor value inside a pending PUT body. */
  pending: (body: StorageSectionBody | null) => C | undefined;
  applySummary: (value: C) => ReactNode;
  /** The drawer's editor prop for this kind. */
  drawer: (editor: RuleEditor<C>) => {
    replication?: RuleEditor<ReplicationConfig>;
    lifecycle?: RuleEditor<LifecycleConfig>;
  };
  newLabel: string;
  /** The empty state of the list: what a rule of this kind does. */
  empty: { title: string; hint: string };
  /** The master switch and the tick, as page settings. */
  settings: {
    switchLabel: string;
    switchPath: string;
    switchHelp: string;
    tickLabel: string;
    tickPath: string;
    tickHelp: string;
    tickPlaceholder: string;
    /** The server's minimum of the tick. */
    tickMin: { seconds: number; text: string };
  };
}

const REPLICATION_SPEC: RuleKindSpec<ReplicationConfig, ReplicationRuleConfig> = {
  kind: 'replication',
  path: 'storage/replication',
  initial: DEFAULT_REPLICATION,
  normalize: (body) => normalizeReplication(body.replication),
  build: buildReplicationPayload,
  emptyRule: emptyReplicationRule,
  fieldErrors: replicationRuleErrors,
  pending: (body) => body?.replication,
  applySummary: (value) => <ReplicationApplySummary replication={value} />,
  drawer: (editor) => ({ replication: editor }),
  newLabel: 'New replication rule',
  empty: {
    title: 'No replication rules yet',
    hint: 'A replication rule copies a bucket, or a folder of it, to another bucket. Click New replication rule at the top right.',
  },
  settings: {
    switchLabel: 'Run replication rules',
    switchPath: 'storage.replication.enabled',
    switchHelp:
      'The switch for every replication rule. While it is off, no rule copies on schedule or on events. Each rule also has its own Enabled switch.',
    tickLabel: 'Check for due rules every',
    tickPath: 'storage.replication.tick_interval',
    tickHelp: 'How often the scheduler looks for a rule whose interval has passed. A duration such as 30s or 1m. The minimum is 5s.',
    tickPlaceholder: '30s',
    tickMin: { seconds: 5, text: '5s' },
  },
};

const LIFECYCLE_SPEC: RuleKindSpec<LifecycleConfig, LifecycleRuleConfig> = {
  kind: 'lifecycle',
  path: 'storage/lifecycle',
  initial: DEFAULT_LIFECYCLE,
  normalize: (body) => normalizeLifecycle(body.lifecycle),
  build: buildLifecyclePayload,
  // A new rule is on: a user who writes a rule expects it to run. Whether
  // the scheduler runs it is the page switch, and the page says so.
  emptyRule: (existing) => ({ ...emptyLifecycleRule(existing), enabled: true }),
  fieldErrors: lifecycleRuleErrors,
  pending: (body) => body?.lifecycle,
  applySummary: (value) => <LifecycleApplySummary lifecycle={value} />,
  drawer: (editor) => ({ lifecycle: editor }),
  newLabel: 'New lifecycle rule',
  empty: {
    title: 'No lifecycle rules yet',
    hint: 'A lifecycle rule deletes or archives objects by age or by count. Click New lifecycle rule at the top right.',
  },
  settings: {
    switchLabel: 'Run lifecycle rules on schedule',
    switchPath: 'storage.lifecycle.enabled',
    switchHelp:
      'Off by default. While it is off, no lifecycle rule runs, and Run now is refused. Each rule also has its own Enabled switch.',
    tickLabel: 'Run each lifecycle rule every',
    tickPath: 'storage.lifecycle.tick_interval',
    tickHelp:
      'A rule runs this long after its last run ended. A new rule first runs this long after you apply it, so you can preview it first. A duration such as 30m, 1h or 1d. The minimum is 1m.',
    tickPlaceholder: '1h',
    tickMin: { seconds: 60, text: '1m' },
  },
};

interface Props {
  onSessionExpired?: () => void;
  /** Raw query string (with leading `?`) for deep-linking (?job=…&tab=…). */
  search?: string;
}

export function ReplicationRulesPanel(props: Props) {
  return <RulesPanel spec={REPLICATION_SPEC} {...props} />;
}

export function LifecycleRulesPanel(props: Props) {
  return <RulesPanel spec={LIFECYCLE_SPEC} {...props} />;
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
  cancel: { label: 'Cancel', icon: <StopOutlined />, danger: true },
  kill: {
    label: 'Kill run',
    icon: <ThunderboltOutlined />,
    danger: true,
    done: 'Kill requested — the running sweep aborts in-flight',
  },
  delete: { label: 'Remove rule', icon: <DeleteOutlined />, danger: true },
};

/**
 * Destructive row actions: shown in the row's "⋯" menu, not as buttons.
 * Delete is a STAGED edit like every other rule change (UX harvest C22):
 * the row shows "removing", the dirty bar appears, and Review & apply
 * removes the rule (the apply also drops its run history). Kill acts at
 * once: it stops a run, not a definition.
 */
type MenuAction = 'kill' | 'delete';
const MENU_ACTIONS: readonly JobAction[] = ['kill', 'delete'] satisfies MenuAction[];
/** The confirmation each menu action asks before it runs. */
const MENU_CONFIRM: Record<MenuAction, (row: JobRow) => RowActionConfirm> = {
  delete: (row) => ({
    title: `Remove rule "${row.name}"?`,
    content:
      'The rule is removed, with its run history, when you review and apply. Until then it keeps running, and Discard keeps it.',
    okText: 'Remove rule',
  }),
  // Kill aborts in-flight work mid-object.
  kill: (row) => ({
    title: `Kill the running "${row.name}" run?`,
    content: 'Transfers in progress stop immediately.',
    okText: 'Kill run',
  }),
};

/**
 * The Last run cell: how long a run in progress has run, else when the last
 * run finished; under it, when the rule runs next, and a link to its runs.
 * Its own component so that only this cell re-renders each second.
 */
function LastRunCell({ row, schedulerOn }: { row: JobRow; schedulerOn: boolean }) {
  const { navigate } = useNavigation();
  const live = isActiveJobStatus(row.status);
  const now = useNow(live ? 1000 : tickPeriodMs(3600));
  const cell = lastRunCell(row, now / 1000);
  const next = nextRunText(row, now, row.kind === 'lifecycle' ? schedulerOn : true);
  const deferred = deferredText(row);
  return (
    <div style={{ fontSize: 12, whiteSpace: 'nowrap', lineHeight: 1.4 }}>
      {cell.running && <Text style={{ fontSize: 12, display: 'block' }}>{cell.running}</Text>}
      {(cell.finishedAt || !cell.running) && (
        <Text type="secondary" style={{ fontSize: 12, display: 'block' }}>
          {cell.running && cell.finishedAt ? 'last: ' : ''}
          <TimeAgo ts={cell.finishedAt} />
        </Text>
      )}
      {next && (
        <Text type="secondary" style={{ fontSize: 11, display: 'block' }}>
          {next}
        </Text>
      )}
      {deferred && (
        <Text type="warning" style={{ fontSize: 11, display: 'block', whiteSpace: 'normal' }}>
          {deferred}
        </Text>
      )}
      <Button
        type="link"
        size="small"
        style={{ padding: 0, height: 'auto', fontSize: 11 }}
        aria-label={`All runs of ${row.name}`}
        onClick={(e) => {
          // The row click opens the drawer; this opens the Jobs page.
          e.stopPropagation();
          navigate(runsUrl({ rule: row.id }));
        }}
      >
        All runs →
      </Button>
    </div>
  );
}

function RulesPanel<C extends RulesConfig<R>, R extends { name: string; enabled: boolean }>({
  spec,
  onSessionExpired,
  search,
}: Props & { spec: RuleKindSpec<C, R> }) {
  const { kind } = spec;
  const { cardStyle, inputRadius } = useCardStyles();
  const qc = useQueryClient();
  const [messageApi, msgCtx] = message.useMessage();
  const { navigate } = useNavigation();
  const { markPushed, closeOverlay } = useOverlayClose();

  // Jobs watched after an action that makes the server start a run a moment
  // later: the list polls fast until that run is seen and settles.
  const [watches, setWatches] = useState<JobWatch[]>([]);
  const jobsQuery = useJobs({ watches });
  const serverRows: JobRow[] = useMemo(
    () => (jobsQuery.data?.jobs ?? []).filter((r) => r.kind === kind),
    [jobsQuery.data, kind],
  );
  const schedulerOn = jobsQuery.data?.workers?.lifecycle?.enabled ?? true;
  useEffect(() => {
    if (watches.length === 0) return;
    const now = Date.now();
    const next = watches
      .map((w) => {
        const row = serverRows.find((r) => r.id === w.id);
        // A run the operator started: say how it ended. The row of a rule
        // carries the counts of the run in progress only, so the words come
        // from its status.
        if (w.announce && row && watchSettled(w, row)) void announceRunEnd(row);
        return stepJobWatch(w, row, now);
      })
      .filter((w): w is JobWatch => w !== null);
    if (next.length !== watches.length || next.some((w, i) => w !== watches[i])) setWatches(next);
    // Each new list sample steps the watches; `watches` itself is not a trigger.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [jobsQuery.dataUpdatedAt]);

  // ── The rule-definition editor. ──
  // Each re-read keeps the rule uids (by name) of the rules the editor holds,
  // then of its last snapshot: after an apply, the rules the editor holds must
  // still match their saved twins. The value comes first because the re-read
  // after the PUT can resolve before React renders the new snapshot.
  const known = useRef<R[]>([]);
  const ed = useSectionEditor<StorageSectionBody, C>({
    section: 'storage',
    dirtyKey: spec.path,
    initial: spec.initial,
    onSessionExpired,
    noun: kind,
    pick: (body) => {
      const next = spec.normalize(body);
      return { ...next, rules: carryRuleUids(next.rules, known.current) };
    },
    toPayload: (v) => {
      const res = spec.build(v);
      return res.ok ? res.body : {};
    },
  });
  known.current = [...ed.value.rules, ...ed.baseline.rules];

  // What a lifecycle rule does to an object, from its saved definition.
  const ruleVerb = (row: JobRow) => {
    if (row.kind !== 'lifecycle') return undefined;
    const rule = (ed.baseline.rules as unknown as LifecycleRuleConfig[]).find((r) => r.name === row.name);
    return lifecycleVerb(rule?.action ?? row.detail?.action);
  };

  // The ONE toast for a run that the operator started. A lifecycle run reads
  // its run record, which names what it did ("deleted 80"); the row may no
  // longer carry the counts once the run ended.
  const announceRunEnd = async (row: JobRow) => {
    if (row.kind === 'lifecycle') {
      try {
        const data = await qc.fetchQuery({
          queryKey: qk.jobs.runs(row.id),
          queryFn: () => getJobRuns(row.id),
          staleTime: 0,
        });
        const run = newestSettledRun(data.runs);
        if (run) {
          const m = lifecycleRunResultText(row.name, run);
          void messageApi.open({ type: m.type, content: m.text, duration: 8 });
          return;
        }
      } catch {
        // Fall back to the row's words below.
      }
    }
    const m = runEndedMessage(row, ruleVerb(row));
    messageApi[m.type](m.text);
  };
  const watchJobs = useCallback(
    (ids: Iterable<string>, announce = false) => {
      const now = Date.now();
      const add = [...ids].map((id) => startJobWatch(id, serverRows.find((r) => r.id === id), now, announce));
      if (add.length === 0) return;
      setWatches((cur) => [...cur.filter((w) => !add.some((a) => a.id === w.id)), ...add]);
    },
    [serverRows],
  );

  // Stage the removal of a saved rule: it leaves the editor value, so the
  // list shows it as "removing" and Apply deletes it. The other unsaved
  // edits stay.
  // Its toast is keyed: Keep and Discard take it away (UX harvest D39).
  const stagedToasts = useRef(new Set<string>());
  const stagedToastKey = (name: string) => `rules-staged-remove:${name}`;
  const dropStagedToast = (name: string) => {
    messageApi.destroy(stagedToastKey(name));
    stagedToasts.current.delete(name);
  };
  const stageRemove = (row: Pick<JobRow, 'id' | 'name'>) => {
    ed.setValue((cur) => ({ ...cur, rules: cur.rules.filter((r) => r.name !== row.name) }));
    if (drawerJobId === row.id) handleDrawerClose();
    stagedToasts.current.add(row.name);
    void messageApi.open({
      key: stagedToastKey(row.name),
      type: 'info',
      content: `Rule ${row.name} is staged for removal. Review and apply to remove it.`,
    });
  };
  // A saved rule that the edits remove (Remove rule, or a rename). Not the
  // same as the row's `pendingDelete`: before the section loads, every
  // server row lacks an editor twin.
  const stagedForRemoval = (name: string) =>
    !ed.loading && ed.baseline.rules.some((r) => r.name === name) && !ed.value.rules.some((r) => r.name === name);
  // Undo a staged removal: the saved rule comes back at its saved place.
  const keepRule = (row: Pick<JobRow, 'name'>) => {
    const saved = ed.baseline.rules.find((r) => r.name === row.name);
    if (!saved) return;
    // A rename shows the old name as "removing" too: its edits live on
    // under the new name, and a second copy of the rule would clash.
    if (ed.value.rules.some((r) => ruleUid(r) === ruleUid(saved))) {
      messageApi.info(`Rule ${row.name} is renamed in your edits. Rename it back, or discard the edits.`);
      return;
    }
    ed.setValue((cur) => {
      if (cur.rules.some((r) => r.name === row.name)) return cur;
      const at = Math.min(ed.baseline.rules.indexOf(saved), cur.rules.length);
      return { ...cur, rules: [...cur.rules.slice(0, at), saved, ...cur.rules.slice(at)] };
    });
    dropStagedToast(row.name);
    messageApi.info(`Rule ${row.name} is kept.`);
  };

  // Clone a rule into a new draft and open it, so the operator points it at
  // other buckets before applying. The copy comes from the editor value, so
  // unsaved edits of the rule come along.
  const cloneRuleRow = (row: Pick<JobRow, 'id' | 'kind' | 'name'>) => {
    const key = parseJobId(row.id)?.key ?? '';
    const idx = findEditorRule(key, ed.value.rules, ed.baseline.rules);
    if (idx < 0) return;
    const copy = cloneRule(ed.value.rules[idx], ed.value.rules);
    ed.setValue((cur) => ({ ...cur, rules: [...cur.rules, copy] }));
    openDrawer(editorRuleJobId(kind, copy, ed.baseline.rules));
    messageApi.info(`Copy of ${row.name || 'the rule'} staged: change its buckets, then apply.`);
  };

  // A draft (staged, never applied) leaves the editor alone: the other
  // unsaved edits stay, and the dirty bar goes away if it was the only one.
  // The draft is found by its id (a uid), never by its name: another rule
  // can have the same name, and a draft's name can be empty.
  const removeDraft = (row: Pick<JobRow, 'id' | 'kind'>) => {
    const key = parseJobId(row.id)?.key ?? '';
    const idx = findEditorRule(key, ed.value.rules, ed.baseline.rules);
    if (idx >= 0) {
      const uid = ruleUid(ed.value.rules[idx]);
      ed.setValue((cur) => ({ ...cur, rules: cur.rules.filter((r) => ruleUid(r) !== uid) }));
    }
    if (drawerJobId === row.id) handleDrawerClose();
  };

  // Discard takes back every staged edit: the errors it showed, and the
  // "staged for removal" toasts of the rules it keeps.
  const discardEditor = () => {
    ed.discard();
    gate.reset();
    for (const name of [...stagedToasts.current]) dropStagedToast(name);
  };

  // Rules with unapplied edits (and drafts): only THESE block Run now /
  // Preview and show the "edited" marker.
  const edited = useMemo(
    () => editedRuleIds(kind, ed.value.rules, ed.baseline.rules),
    [kind, ed.value.rules, ed.baseline.rules],
  );

  // Every field is checked here (`ruleFieldErrors`): an error shows under
  // its field once the field lost focus, or after Review & apply, and the
  // dirty bar counts the errors on screen. ONE gate for the page and every
  // rule: keys `tick` and `<rule uid>:<field>` (UX harvest D9, D10).
  const gate = useErrorGate();
  const tickError = durationError(ed.value.tick_interval, { min: spec.settings.tickMin });
  const ruleErrors = ed.value.rules.map((r) => spec.fieldErrors(r, ed.value.rules));
  const errorCount =
    (tickError && gate.show('tick') ? 1 : 0) +
    ruleErrors.reduce(
      (sum, errs, i) =>
        sum + Object.entries(errs).filter(([field, e]) => e && gate.show(`${ruleUid(ed.value.rules[i])}:${field}`)).length,
      0,
    );
  const firstBadRule = ruleErrors.findIndex((errs) => Object.values(errs).some(Boolean));
  const startApply = useCallback(async () => {
    if (tickError || firstBadRule >= 0) {
      // Show every error under its field, and the first rule with one.
      gate.submit();
      if (firstBadRule >= 0) {
        const rule = ed.value.rules[firstBadRule];
        const id = editorRuleJobId(kind, rule, ed.baseline.rules);
        if (drawerJobId !== id) openDrawer(id);
      }
      return;
    }
    const r = spec.build(ed.value);
    if (!r.ok) {
      messageApi.error(r.error);
      return;
    }
    await ed.runApply();
    // `drawerJobId` and `openDrawer` are declared below; they are read when
    // the handler runs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [spec, ed, messageApi, tickError, firstBadRule, gate, kind]);
  useApplyHandler(spec.path, startApply, ed.isDirty);

  // The row of a rule that the user just applied: it flashes once.
  const [highlightId, setHighlightId] = useState<string | null>(null);
  useEffect(() => {
    if (!highlightId) return;
    const t = window.setTimeout(() => setHighlightId(null), 3000);
    return () => window.clearTimeout(t);
  }, [highlightId]);

  const confirmApply = useCallback(async () => {
    // Before the apply moves the snapshot: the saved rules it deletes or
    // changes, and the rules whose runs the scheduler starts within a second.
    const staleVerify = kind === 'replication' ? savedRulesChangedByApply(ed.value.rules, ed.baseline.rules) : [];
    const changed = ruleIdsChangedByApply(kind, ed.value.rules, ed.baseline.rules);
    // The rule in the drawer: after the apply it is a saved rule, known by its name.
    const shown = drawerRule.current ? ed.value.rules.find((r) => ruleUid(r) === drawerRule.current?.uid) : undefined;
    if (await ed.confirmApply()) {
      // ONE toast (the editor's "Applied"): a scheduler that is off is a
      // fact of the page, and its banner says so (UX harvest D20).
      watchJobs(changed);
      gate.reset();
      for (const name of [...stagedToasts.current]) dropStagedToast(name);
      // A deleted, renamed or changed rule's parity result is not the new rule's.
      for (const name of staleVerify) qc.removeQueries({ queryKey: qk.jobs.verify(name) });
      // Done with the rule: close its drawer and point at its row (D53).
      if (drawerJobId) handleDrawerClose();
      if (shown) setHighlightId(`${kind}:${shown.name}`);
    }
    qc.invalidateQueries({ queryKey: qk.jobs.list() });
    // `drawerRule`, `drawerJobId` and `handleDrawerClose` are declared below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, ed, qc, watchJobs, gate]);

  // ── Display rows: server rows + drafts/pending-deletes overlay. ──
  const displayRows: JobDisplayRow[] = useMemo(() => {
    const mine = { rules: ed.value.rules, baseline: ed.baseline.rules };
    const none = { rules: [], baseline: [] };
    return mergeDraftRules(
      serverRows,
      kind === 'replication' ? mine : none,
      kind === 'lifecycle' ? mine : none,
      edited,
    );
  }, [serverRows, kind, ed.value.rules, ed.baseline.rules, edited]);

  // ── Drawer ──
  // The drawer's open rule and active tab are URL-deep-linked via
  // ?job=…&tab=… so shared links, browser Back/Forward, and reloads all land
  // on the right view.
  const queryParams = useMemo(() => parseAdminQuery(search ?? ''), [search]);
  const [drawerJobId, setDrawerJobId] = useState<string | null>(queryParams.job ?? null);
  const [drawerTab, setDrawerTab] = useState<string>(queryParams.tab ?? 'definition');
  const [actionBusy, setActionBusy] = useState<string | null>(null);
  const [confirmRun, setConfirmRun] = useState<JobRow | null>(null);

  // Sync URL → state (Back/Forward / shared link).
  useEffect(() => {
    setDrawerJobId(queryParams.job ?? null);
    setDrawerTab(queryParams.tab ?? 'definition');
  }, [queryParams.job, queryParams.tab]);

  const pageUrl = useCallback(
    (query?: Record<string, string>) => buildViewUrl('admin', spec.path, query),
    [spec.path],
  );

  // Tab changes REPLACE the URL (no history spam); open/close PUSH.
  const updateDrawerUrl = useCallback(
    (job: string | null, tab: string) => {
      const query: Record<string, string> = {};
      if (job) query.job = job;
      if (job && tab && tab !== 'definition') query.tab = tab;
      navigate(pageUrl(Object.keys(query).length > 0 ? query : undefined), { replace: true });
    },
    [navigate, pageUrl],
  );

  const openDrawer = useCallback(
    (jobId: string, tab = 'definition') => {
      setDrawerJobId(jobId);
      setDrawerTab(tab);
      navigate(pageUrl(tab === 'definition' ? { job: jobId } : { job: jobId, tab }));
      markPushed();
    },
    [navigate, markPushed, pageUrl],
  );

  // Retarget: same as openDrawer but REPLACE (not push). The drawer is
  // already open on the same rule; only its job id changes.
  const retargetDrawer = useCallback(
    (jobId: string) => {
      setDrawerJobId(jobId);
      navigate(pageUrl({ job: jobId, ...(drawerTab !== 'definition' ? { tab: drawerTab } : {}) }), { replace: true });
    },
    [navigate, drawerTab, pageUrl],
  );

  // The drawer holds ONE rule while its id changes under it. A draft id
  // stays the same while the operator types the name (also through the
  // name of another rule). After an apply, the drawer moves to the rule's
  // server id: a draft becomes a server rule, and a renamed rule is now
  // known by its new name.
  const drawerRule = useRef<{ jobId: string; uid: string } | null>(null);
  useEffect(() => {
    const parsed = drawerJobId ? parseJobId(drawerJobId) : null;
    if (!drawerJobId || !parsed || parsed.subsystem !== kind) {
      drawerRule.current = null;
      return;
    }
    const idx = findEditorRule(parsed.key, ed.value.rules, ed.baseline.rules);
    let rule = idx >= 0 ? ed.value.rules[idx] : undefined;
    if (!rule && drawerRule.current?.jobId === drawerJobId) {
      const uid = drawerRule.current.uid;
      rule = ed.value.rules.find((r) => ruleUid(r) === uid);
    }
    if (!rule) {
      drawerRule.current = null;
      return;
    }
    const canonical = editorRuleJobId(kind, rule, ed.baseline.rules);
    // A server id stays while it resolves (a rename in progress keeps its
    // Runs tab); a draft id moves once the rule is applied.
    const move = canonical !== drawerJobId && (idx < 0 || parsed.key.startsWith(DRAFT_KEY_MARK));
    drawerRule.current = { jobId: move ? canonical : drawerJobId, uid: ruleUid(rule) };
    if (move) retargetDrawer(canonical);
  }, [drawerJobId, kind, ed, retargetDrawer]);

  const handleDrawerClose = useCallback(() => {
    setDrawerJobId(null);
    setDrawerTab('definition');
    closeOverlay(pageUrl(), navigate);
  }, [closeOverlay, navigate, pageUrl]);

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
    // It shows the cached preview while it is fresh (each one walks the rule's
    // scope); the tab's "Refresh preview" computes a new one.
    if (action === 'preview') {
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
      // Resume and Run now make the server start a run a moment later.
      if (action === 'run-now' || action === 'resume') watchJobs([row.id], action === 'run-now');
      // Refresh the list AND this rule's runs/failures tables — a resume or
      // run-now starts a new run that the open drawer's tabs must show.
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
      qc.invalidateQueries({ queryKey: qk.jobs.runs(row.id) });
      qc.invalidateQueries({ queryKey: qk.jobs.failures(row.id) });
      qc.invalidateQueries({ queryKey: qk.jobs.runList() });
      // A run changes the objects that the cached preview lists.
      if (action === 'run-now') qc.invalidateQueries({ queryKey: qk.jobs.preview(row.id) });
    } catch (e) {
      messageApi.error(plainError(e, `${ACTION_META[action].label} did not work. Try again.`).text);
    } finally {
      setActionBusy(null);
    }
  };

  const newRule = () => {
    const rule = spec.emptyRule(ed.value.rules);
    ed.setValue((cur) => ({ ...cur, rules: [...cur.rules, rule] }));
    openDrawer(editorRuleJobId(kind, rule, ed.baseline.rules));
  };

  const columns: RecordColumn<JobDisplayRow>[] = [
    {
      key: 'rule',
      label: 'Rule',
      // The name is what operators scan for. Self-describing: no caption on
      // a phone card.
      hideLabelOnNarrow: true,
      track: 'minmax(180px,2fr)',
      render: (d) => (
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
          {/* Cut in the middle: rule names often differ only at the end. */}
          <TruncatedName
            text={d.row.name}
            className="dg-rule-name"
            style={{ fontFamily: 'var(--font-mono)', fontSize: 13, fontWeight: 600 }}
          />
          {d.draft && <Tag color="warning" style={{ margin: 0, flexShrink: 0 }}>draft</Tag>}
          {d.edited && (
            <Tag
              color="warning"
              style={{ margin: 0, flexShrink: 0 }}
              title="This rule has unsaved edits. Run now and Preview use the saved rule, so they wait until you apply or discard."
            >
              edited
            </Tag>
          )}
          {/* Not while the section loads: then no server row has an editor twin yet. */}
          {d.pendingDelete && !ed.loading && (
            <Tag
              color="error"
              style={{ margin: 0, flexShrink: 0 }}
              title="Staged for removal: Review & apply removes this rule. Discard, or Keep this rule in the ⋯ menu, keeps it."
            >
              removing
            </Tag>
          )}
        </div>
      ),
    },
    {
      key: 'scope',
      label: 'Scope',
      track: 'minmax(140px,1.2fr)',
      render: (d) => {
        const scope = scopeText(d.row.scope);
        return (
          // Cut in the middle: bucket/prefix scopes differ at the end (D5).
          <Text type="secondary" style={{ display: 'flex', minWidth: 0, fontFamily: 'var(--font-mono)', fontSize: 12 }}>
            <TruncatedName text={scope || '—'} />
          </Text>
        );
      },
    },
    {
      key: 'trigger',
      label: 'Trigger',
      narrowHalf: true,
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
      // Self-describing: no caption on a phone card.
      hideLabelOnNarrow: true,
      track: 'minmax(0,1fr)',
      render: (d) => {
        const live = isActiveJobStatus(d.row.status);
        // A settled rule keeps the counts of its last run ("14 copied"),
        // not only a status word.
        const view = statusCellView(d.row, d.draft);
        const failed = d.row.failed_total ?? d.row.progress.failed;
        const liveCopies = liveCopySummary(d.row);
        return (
          // OutcomeMeter.css grows this wrapper to the column width.
          <div>
            {view.tag && (
              <Tag color={jobStatusTone(d.row)} style={{ margin: view.meter ? '0 0 4px' : 0 }}>
                {jobStatusLabel(d.row)}
              </Tag>
            )}
            {view.meter && <OutcomeMeter {...rowMeterInput(d.row, ruleVerb(d.row))} />}
            {liveCopies && (
              // Event-driven copies write no run row: this is where they show.
              <Text type="secondary" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
                last live copy {liveCopies.at ? <TimeAgo ts={liveCopies.at} /> : '—'}
                {liveCopies.counts ? ` · ${liveCopies.counts}` : ''}
              </Text>
            )}
            {live && d.row.paused && d.row.status === 'running' && (
              <Text type="warning" style={{ display: 'block', fontSize: 11, marginTop: 2 }}>
                Paused: this run goes on; the next one waits for Resume.
              </Text>
            )}
            {failed > 0 && (
              <Button
                type="link"
                size="small"
                danger
                style={{ padding: '0 4px', height: 'auto', fontSize: 12 }}
                aria-label={`${failed} failed — show failures: ${d.row.name}`}
                title="Show the failed objects"
                onClick={(e) => {
                  // The row click opens the Definition tab; this opens Failures.
                  e.stopPropagation();
                  openDrawer(d.row.id, 'failures');
                }}
              >
                {failed.toLocaleString()} failed
              </Button>
            )}
            {d.row.last_error && (
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
      narrowHalf: true,
      track: 'max-content',
      render: (d) => (d.draft ? <Text type="secondary">—</Text> : <LastRunCell row={d.row} schedulerOn={schedulerOn} />),
    },
    {
      key: 'actions',
      label: 'Actions',
      track: 'max-content',
      align: 'end',
      hideLabelOnNarrow: true,
      render: (d) =>
        // Staged for removal: no Pause / Preview / Run now on a rule that
        // is about to go; the one action is to keep it (UX harvest D38).
        d.pendingDelete && !d.draft && stagedForRemoval(d.row.name) ? (
          <Button
            size="small"
            icon={<UndoOutlined />}
            aria-label={`Keep this rule: ${d.row.name}`}
            onClick={(e) => {
              e.stopPropagation();
              keepRule(d.row);
            }}
          >
            Keep this rule
          </Button>
        ) : d.draft ? (
          <Space size={2} onClick={(e) => e.stopPropagation()}>
            <RowActionsMenu
              label={`More actions for ${d.row.name}`}
              actions={[cloneRuleAction(d.row, cloneRuleRow), draftRemoveAction(d.row, removeDraft)]}
            />
          </Space>
        ) : (
          <Space size={2} onClick={(e) => e.stopPropagation()}>
            {availableActions(d.row)
              .filter((a) => !MENU_ACTIONS.includes(a))
              .map((a) => {
                // A replication one-off on a disabled/paused rule reads as "Run
                // once" (it runs the rule a single time without re-enabling it).
                const oneOff =
                  a === 'run-now' &&
                  d.row.kind === 'replication' &&
                  (d.row.enabled === false || d.row.paused === true);
                const label = oneOff ? 'Run once' : ACTION_META[a].label;
                const blocked =
                  (stagedForRemoval(d.row.name) && (a === 'run-now' || a === 'preview')
                    ? 'This rule is staged for removal. Keep the rule or discard the edits first.'
                    : null) ??
                  draftBlocksAction(a, d.row, edited) ??
                  // The SAVED master switch decides (unknown while it loads:
                  // do not block on the default).
                  (a === 'run-now' ? lifecycleRunNowBlock(d.row, ed.loading || ed.baseline.enabled) : null);
                const title = oneOff ? 'Run this rule once now — does not enable or resume it' : label;
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
            {/* Clone, and the destructive actions (delete, kill), live behind
                "⋯", not as a red icon on every row; the menu confirms each
                destructive one. */}
            <RowActionsMenu
              label={`More actions for ${d.row.name}`}
              loading={MENU_ACTIONS.some((a) => actionBusy === `${d.row.id}:${a}`)}
              actions={[
                ...(stagedForRemoval(d.row.name)
                  ? [{ key: 'keep', icon: <UndoOutlined />, label: 'Keep this rule', onSelect: () => keepRule(d.row) }]
                  : [cloneRuleAction(d.row, cloneRuleRow)]),
                ...availableActions(d.row)
                  .filter((a) => MENU_ACTIONS.includes(a) && !(a === 'delete' && stagedForRemoval(d.row.name)))
                  .map((a) => ({
                    key: a,
                    danger: true as const,
                    icon: ACTION_META[a].icon,
                    label: `${ACTION_META[a].label}…`,
                    confirm: MENU_CONFIRM[a as MenuAction](d.row),
                    // Remove is staged in the editor; Kill goes to the server.
                    onSelect: () => (a === 'delete' ? stageRemove(d.row) : runAction(d.row, a)),
                  })),
              ]}
            />
          </Space>
        ),
    },
  ];

  const pageActions = (
    <PageActions>
      <Button type="primary" icon={<PlusOutlined />} onClick={newRule}>
        {spec.newLabel}
      </Button>
    </PageActions>
  );
  const what = kind === 'replication' ? 'the replication rules' : 'the lifecycle rules';
  // One state for the page: no settings and no rows before the rules load,
  // and an error with Retry instead of a list (UX harvest D16, D17).
  if (jobsQuery.isPending || (ed.loading && !ed.loadError)) {
    return (
      // No "New rule" before the rules load: a draft made now would be lost
      // when the section arrives.
      <div style={contentColumn(CONTENT_WIDE)}>
        <div style={cardStyle}>
          <LoadingState variant="list" rows={4} label={`Loading ${what}`} />
        </div>
      </div>
    );
  }
  const loadError = (!jobsQuery.data && jobsQuery.error) || ed.loadError;
  if (loadError) {
    return (
      <div style={contentColumn(CONTENT_WIDE)}>
        <div style={cardStyle}>
          <ErrorState
            error={loadError}
            what={what}
            onRetry={() => {
              void jobsQuery.refetch();
              void ed.refresh();
            }}
          />
        </div>
      </div>
    );
  }

  const st = spec.settings;
  // A lifecycle rule runs only while the page switch is on (off by
  // default): say so where the switch is, while enabled rules wait on it.
  const schedulerOffNote =
    kind === 'lifecycle' && !ed.loading && !ed.value.enabled && ed.value.rules.some((r) => r.enabled)
      ? `No lifecycle rule runs while “${st.switchLabel}” is off. Turn it on, then review and apply.`
      : null;
  return (
    <div style={contentColumn(CONTENT_WIDE)}>
      {msgCtx}
      {pageActions}
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
        {...spec.drawer({ value: ed.value, saved: ed.baseline, onChange: ed.setValue, gate })}
        onRemoveDraft={removeDraft}
        onCloneRule={cloneRuleRow}
        lifecycleVerbOf={ruleVerb}
        footer={
          ed.isDirty ? (
            <StickyDirtyBar
              visible
              applying={ed.applying}
              errorCount={errorCount}
              onDiscard={discardEditor}
              onApply={() => void startApply()}
              inline
            />
          ) : undefined
        }
        activeTab={drawerTab}
        onTabChange={handleTabChange}
        inputRadius={inputRadius}
        onClose={handleDrawerClose}
      />

      <div style={cardStyle}>
        {displayRows.length > 0 && (
          <Text type="secondary" style={{ display: 'block', fontSize: 13, marginBottom: 16 }}>
            {`${displayRows.length} rule${displayRows.length === 1 ? '' : 's'}. Select one for its definition, runs and failures.`}
          </Text>
        )}

        <FormField
          label={st.switchLabel}
          yamlPath={st.switchPath}
          helpText={st.switchHelp}
          style={{ marginBottom: 0 }}
        >
          <Switch
            checked={ed.value.enabled}
            disabled={ed.loading}
            onChange={(enabled) => ed.setValue((cur) => ({ ...cur, enabled }))}
          />
        </FormField>
        {schedulerOffNote && (
          <Alert
            type="warning"
            showIcon
            data-testid="rules-scheduler-off"
            style={{ marginTop: 8, borderRadius: 8 }}
            title={schedulerOffNote}
          />
        )}
        <FormField
          label={st.tickLabel}
          yamlPath={st.tickPath}
          helpText={st.tickHelp}
          error={gate.show('tick') && !ed.loading ? (tickError ?? undefined) : undefined}
          style={{ marginTop: 12, marginBottom: 0, maxWidth: 360 }}
        >
          <Input
            {...gate.props('tick')}
            value={ed.value.tick_interval}
            disabled={ed.loading}
            placeholder={st.tickPlaceholder}
            onChange={(e) => ed.setValue((cur) => ({ ...cur, tick_interval: e.target.value }))}
            style={{ ...inputRadius, fontFamily: 'var(--font-mono)' }}
          />
        </FormField>

        <div style={{ marginTop: 16 }}>
          <RecordList
            rows={displayRows}
            columns={columns}
            rowKey={(d) => d.row.id}
            onRowClick={(d) => openDrawer(d.row.id)}
            highlightKey={highlightId}
            isDimmed={(d) => !!d.pendingDelete && !d.draft && stagedForRemoval(d.row.name)}
            // The page header carries "New … rule": the empty state names it
            // instead of repeating the button (PageActions).
            empty={<EmptyState icon={<ScheduleOutlined />} title={spec.empty.title} hint={spec.empty.hint} />}
          />
        </div>
      </div>

      {/* With the drawer open the bar sits in its footer (above). */}
      <StickyDirtyBar
        visible={ed.isDirty && !drawerJobId}
        applying={ed.applying}
        errorCount={errorCount}
        onDiscard={discardEditor}
        onApply={() => void startApply()}
        floating
      />
      <ApplyDialog
        open={ed.applyOpen}
        section="storage"
        response={ed.applyResponse}
        onApply={() => void confirmApply()}
        onCancel={ed.cancelApply}
        loading={ed.applying}
        summary={spec.applySummary(spec.pending(ed.pendingBody) ?? ed.value)}
      />
    </div>
  );
}
