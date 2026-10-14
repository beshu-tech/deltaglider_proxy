/**
 * Pure view logic for the unified Jobs screen (React-free — transpiled
 * directly by the unit test).
 *
 * The backend's GET /api/admin/jobs returns ONE row shape for every
 * background operation (replication rules, lifecycle rules, one-off
 * re-encrypt/migrate jobs). These helpers turn rows into UI decisions:
 * status tones, kind labels, the per-kind action matrix, progress labels,
 * bucket-busy lookups, and the draft-row merge for staged-but-unapplied
 * rule definitions.
 */

import type { ConflictPolicy, FixAction, JobRunEntry, RerunVerdict } from './adminApi';
import { formatBytes, formatDuration } from './utils';
import { ruleUid } from './ruleUid';

export type JobKind = 'replication' | 'lifecycle' | 'reencrypt' | 'migrate' | string;
export type JobAction = 'pause' | 'resume' | 'run-now' | 'preview' | 'cancel' | 'delete' | 'kill';

export interface JobRow {
  id: string; // "replication:<rule>" | "lifecycle:<rule>" | "maintenance:<n>"
  kind: JobKind;
  name: string;
  scope: { bucket: string; prefix?: string; target?: string };
  trigger: 'continuous' | 'scheduled' | 'oneoff' | string;
  enabled?: boolean;
  paused?: boolean;
  /** normalized: idle|queued|running|cancelling|paused|deferred|succeeded|completed_with_errors|failed|cancelled */
  status: string;
  status_raw: string;
  phase?: string;
  percent?: number | null;
  progress: {
    processed: number;
    total?: number | null;
    bytes: number;
    failed: number;
    skipped: number;
    /** Objects the run deleted (mirror delete, lifecycle expiry). */
    deleted?: number | null;
  };
  /** Total objects when known (also inside `progress.total` on older servers). */
  total?: number | null;
  lifetime?: { objects: number; bytes: number };
  last_run_at?: number | null;
  next_due_at?: number | null;
  created_at?: number | null;
  started_at?: number | null;
  finished_at?: number | null;
  last_error?: string | null;
  /** Failures recorded for the latest run. */
  failed_total?: number | null;
  detail: Record<string, unknown>;
}

/** Split a job id into its subsystem + key. */
export function parseJobId(id: string): { subsystem: string; key: string } | null {
  const idx = id.indexOf(':');
  if (idx <= 0 || idx === id.length - 1) return null;
  return { subsystem: id.slice(0, idx), key: id.slice(idx + 1) };
}

/** Statuses for which a one-off job is live (busy chips, fast polling). */
export function isActiveJobStatus(status: string): boolean {
  return status === 'queued' || status === 'running' || status === 'cancelling';
}

/** Poll cadence while any job/run is live (progress bars move). */
export const ACTIVE_POLL_MS = 2000;
/**
 * Idle floor for job polls: with nothing running, the poll still catches a
 * job that STARTS elsewhere (a scheduler tick, another operator, another
 * tab) without a reload. 10 s: the list is one read of the jobs DB, and an
 * operator who looks at the page sees a new run within 10 s. At 60 s a run
 * shorter than a minute was often never seen as running at all. There is no
 * cheap "something changed" counter for all job kinds, so a short poll it is.
 */
export const IDLE_POLL_MS = 10_000;
/**
 * How long the list polls fast after an action that makes the SERVER start a
 * run a moment later (Apply of a rule, Resume, Run now). The scheduler picks
 * the rule up within a second; the window only bounds the case where no run
 * starts at all (a lifecycle rule with nothing due).
 */
export const WATCH_WINDOW_MS = 30_000;

/**
 * A job the list watches after an action: it polls fast until the run is
 * seen and settles, or until the window ends.
 */
export interface JobWatch {
  id: string;
  /** `last_run_at` when the watch started: a later value = a run finished. */
  baselineLastRunAt: number | null;
  sawActive: boolean;
  /** Epoch ms after which the watch ends even without a run. */
  until: number;
  /** Tell the operator how the run ended (Run now: they asked for it). */
  announce?: boolean;
}

export function startJobWatch(
  id: string,
  row: Pick<JobRow, 'last_run_at'> | null | undefined,
  nowMs: number,
  announce = false,
): JobWatch {
  return {
    id,
    baselineLastRunAt: row?.last_run_at ?? null,
    sawActive: false,
    until: nowMs + WATCH_WINDOW_MS,
    ...(announce ? { announce } : {}),
  };
}

/** The watched run was seen (or its finish was) and is over. */
export function watchSettled(w: JobWatch, row: Pick<JobRow, 'status' | 'last_run_at'> | null | undefined): boolean {
  if (!row || isActiveJobStatus(row.status)) return false;
  return w.sawActive || (row.last_run_at ?? 0) > (w.baselineLastRunAt ?? 0);
}

/** Fold one list sample into a watch; null = the watch is done. */
export function stepJobWatch(
  w: JobWatch,
  row: Pick<JobRow, 'status' | 'last_run_at'> | null | undefined,
  nowMs: number,
): JobWatch | null {
  if (row && isActiveJobStatus(row.status)) return w.sawActive ? w : { ...w, sawActive: true };
  if (watchSettled(w, row)) return null;
  return nowMs > w.until ? null : w;
}

/** The toast when a run that the operator started has ended. */
export function runEndedMessage(row: JobRow, verb?: LifecycleVerb): { type: 'success' | 'warning' | 'error'; text: string } {
  const input = rowMeterInput(row, verb);
  // A rule row may carry no counts once its run ended: then point at the
  // Runs tab instead of saying "no objects".
  const label =
    input.scanned > 0 || row.trigger === 'oneoff' ? deriveMeter(input).label : 'the Runs tab has its counts';
  const what = `${kindLabel(row.kind)} "${row.name}"`;
  if (row.status === 'failed') return { type: 'error', text: `${what}: the run failed${row.last_error ? ` — ${row.last_error}` : ''}` };
  if (row.status === 'completed_with_errors' || row.progress.failed > 0)
    return { type: 'warning', text: `${what}: the run finished with errors — ${label}` };
  if (row.status === 'cancelled' || row.status === 'paused') return { type: 'warning', text: `${what}: the run stopped — ${label}` };
  if (row.status === 'deferred')
    return { type: 'warning', text: `${what}: the proxy stopped the run; it goes on from there in about a minute — ${label}` };
  return { type: 'success', text: `${what}: the run finished — ${label}` };
}

/** refetchInterval for the unified jobs list. */
export function jobsPollInterval(jobs: Pick<JobRow, 'status'>[], watches: readonly JobWatch[] = []): number {
  return watches.length > 0 || jobs.some((j) => isActiveJobStatus(j.status)) ? ACTIVE_POLL_MS : IDLE_POLL_MS;
}

/**
 * The open drawer's Runs and Failures tabs read again whenever this value
 * changes: a run starts, settles, or the rule is paused. Waiting only for
 * the active→terminal edge missed runs that the scheduler started and
 * finished between two polls.
 */
export function runsRefreshKey(
  row: Pick<JobRow, 'status' | 'last_run_at' | 'started_at' | 'finished_at' | 'paused'> | null,
): string {
  if (!row) return '';
  return [row.status, row.last_run_at ?? '', row.started_at ?? '', row.finished_at ?? '', row.paused ? 'p' : ''].join('|');
}

export type InFlightCopy = { key: string; size: number; started_unix: number };

/**
 * Live "currently copying" entries from a job row's detail (replication
 * rows on an active run). Defensive: the field is absent on old servers,
 * drafts, and non-replication kinds.
 */
export function jobInFlight(row: Pick<JobRow, 'status' | 'detail'> | null): InFlightCopy[] {
  if (!row || !isActiveJobStatus(row.status)) return [];
  const raw = row.detail?.in_flight;
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (e): e is InFlightCopy =>
      !!e &&
      typeof e === 'object' &&
      typeof (e as InFlightCopy).key === 'string' &&
      typeof (e as InFlightCopy).size === 'number',
  );
}

export type WalkProgress = {
  scanning: string | null;
  dirs_completed: number;
  dirs_pending: number;
};

/**
 * Live reconcile-walk progress from a replication row's detail (active run
 * only). `scanning` is the directory the walk is currently in; the dir counts
 * are an activity indicator, NOT a percent basis (pending grows-then-drains as
 * the tree is discovered). Absent on old servers / non-replication kinds.
 */
export function jobWalkProgress(
  row: Pick<JobRow, 'status' | 'detail'> | null,
): WalkProgress | null {
  if (!row || !isActiveJobStatus(row.status)) return null;
  const raw = row.detail?.walk as Record<string, unknown> | undefined;
  if (!raw || typeof raw !== 'object') return null;
  const dirs_completed = typeof raw.dirs_completed === 'number' ? raw.dirs_completed : 0;
  const dirs_pending = typeof raw.dirs_pending === 'number' ? raw.dirs_pending : 0;
  const scanning = typeof raw.scanning === 'string' ? raw.scanning : null;
  return { scanning, dirs_completed, dirs_pending };
}

type StrategySegment = {
  key: 'verbatim' | 'reconstructed' | 'straight';
  count: number;
  /** Layman label shown inline. */
  label: string;
  /** Technical explanation, surfaced on hover. */
  hint: string;
  /** Leading glyph. */
  glyph: string;
};

export type StrategyMix = {
  segments: StrategySegment[];
  bytesEgressSaved: number;
};

/**
 * How each copied object was moved — the "what algorithm did the engine
 * apply" story. `delta_passthrough` (verbatim) and `reconstructed` come
 * straight from the run; `straight` is the remainder (copied − the two).
 * Returns null when nothing was copied (no story to tell) or the run predates
 * the counters. Only non-zero segments are included, so the caller renders
 * exactly what happened.
 */
export function jobStrategyMix(
  run: {
    objects_processed?: number;
    delta_passthrough?: number;
    reconstructed?: number;
    bytes_egress_saved?: number;
  } | null,
): StrategyMix | null {
  if (!run) return null;
  const copied = run.objects_processed ?? 0;
  if (copied <= 0) return null;
  const verbatim = Math.max(0, run.delta_passthrough ?? 0);
  const reconstructed = Math.max(0, run.reconstructed ?? 0);
  const straight = Math.max(0, copied - verbatim - reconstructed);
  const all: StrategySegment[] = [
    {
      key: 'verbatim',
      count: verbatim,
      label: 'shipped as-is',
      hint: 'Delta bytes copied verbatim — no decompress, no recompress. The cheapest path.',
      glyph: '⚡',
    },
    {
      key: 'reconstructed',
      count: reconstructed,
      label: 'rebuilt',
      hint: 'Decompressed from the delta, then re-stored (recompressed or re-encrypted) at the destination.',
      glyph: '↻',
    },
    {
      key: 'straight',
      count: straight,
      label: 'straight copy',
      hint: 'Whole object copied byte-for-byte (already-compressed files: images, video, archives).',
      glyph: '→',
    },
  ];
  return {
    segments: all.filter((s) => s.count > 0),
    bytesEgressSaved: Math.max(0, run.bytes_egress_saved ?? 0),
  };
}

/**
 * AntD tag color for a job row. A run in progress wins over pause and
 * disable (the run continues); otherwise pause/disable win over the last
 * status.
 */
export function jobStatusTone(row: Pick<JobRow, 'status' | 'paused' | 'enabled'>): string {
  if (row.status === 'running' || row.status === 'cancelling') return 'processing';
  if (row.enabled === false) return 'default';
  if (row.paused || row.status === 'paused' || row.status === 'deferred') return 'warning';
  switch (row.status) {
    case 'queued':
      return 'default';
    case 'succeeded':
      return 'success';
    case 'completed_with_errors':
      // Amber, NOT red: the sweep finished and copied everything it could —
      // a transient per-object error doesn't make the run a failure.
      return 'warning';
    case 'failed':
      return 'error';
    case 'cancelled':
      return 'default';
    default:
      return 'default'; // idle
  }
}

/**
 * The status word the row should display. A run in progress wins: pausing a
 * rule does not stop the run that is going, it stops the NEXT one, so a
 * paused rule mid-run says so. Otherwise pause/disable beat the last status.
 */
export function jobStatusLabel(row: Pick<JobRow, 'status' | 'paused' | 'enabled'>): string {
  if (row.status === 'running') return row.paused ? 'running · pauses after this run' : 'running';
  if (row.status === 'cancelling') return 'cancelling';
  if (row.enabled === false) return 'disabled';
  if (row.paused) return 'paused';
  if (row.status === 'queued') return 'waiting to start';
  // A run that a pause stopped; it kept its position, so it is not
  // "cancelled": the next run goes on from there.
  if (row.status === 'paused') return 'stopped · resumes where it left off';
  // A run that the proxy stopped (a maintenance job or an unhealthy backend
  // gates its bucket, or a page budget): nobody paused it, and the next run,
  // about a minute later, goes on from where it stopped.
  if (row.status === 'deferred') return 'deferred · continues shortly';
  // Shorten the verbose backend status for the chip; the run's error count is
  // shown separately in the Runs table.
  if (row.status === 'completed_with_errors') return 'completed · errors';
  return row.status;
}

export function kindLabel(kind: JobKind): string {
  switch (kind) {
    case 'replication':
      return 'Replication';
    case 'lifecycle':
      return 'Lifecycle';
    case 'reencrypt':
      return 'Re-encrypt';
    case 'migrate':
      return 'Migrate';
    case 'backfill-metadata':
      return 'Backfill metadata';
    default:
      return kind;
  }
}

/** AntD tag color for a job kind chip: rules blue/purple, one-offs gold. */
export function kindTone(kind: JobKind): string {
  switch (kind) {
    case 'replication':
      return 'blue';
    case 'lifecycle':
      return 'purple';
    default:
      return 'gold';
  }
}

export function triggerLabel(trigger: string): string {
  switch (trigger) {
    case 'continuous':
      return 'continuous';
    case 'scheduled':
      return 'scheduled';
    case 'oneoff':
      return 'one-off';
    default:
      return trigger;
  }
}

/**
 * The uniform action matrix, contextualised by row state:
 * - rule kinds: pause XOR resume (by current flag) + run-now (only when
 *   enabled, not paused, not mid-run); lifecycle adds preview.
 * - one-off kinds: cancel while active.
 */
export function availableActions(row: JobRow): JobAction[] {
  const out: JobAction[] = [];
  if (row.kind === 'replication' || row.kind === 'lifecycle') {
    // Pause is meaningless on a disabled rule (the scheduler never runs it);
    // resume stays so a paused-and-disabled rule can still clear its flag.
    if (row.paused) out.push('resume');
    else if (row.enabled !== false) out.push('pause');
    if (row.kind === 'lifecycle') out.push('preview');
    // run-now whenever not running. Replication runs even a disabled/paused
    // rule once (without flipping the flag). The backend 409s a disabled or
    // paused lifecycle rule: the row shows that button disabled, with the
    // reason from `lifecycleRunNowBlock`.
    if (!isActiveJobStatus(row.status)) out.push('run-now');
    // Kill the in-flight run at will: it stops at the next object and, unlike
    // pause, drops the cursor (lifecycle) or interrupts the copy (replication).
    if (row.status === 'running') out.push('kill');
    out.push('delete');
    return out;
  }
  if (isActiveJobStatus(row.status) && row.status !== 'cancelling') out.push('cancel');
  return out;
}

/**
 * Why lifecycle run-now would be refused (409), or null. Mirrors the server's
 * checks: the global scheduler switch, the rule's Enabled switch, the pause.
 * A lifecycle run deletes or moves objects, so there is no one-off override.
 */
export function lifecycleRunNowBlock(row: JobRow, schedulerOn: boolean): string | null {
  if (row.kind !== 'lifecycle') return null;
  if (!schedulerOn)
    return 'Turn on "Run lifecycle rules on schedule" first. While it is off, the proxy refuses every lifecycle run.';
  if (row.enabled === false)
    return 'Enable the rule first. A lifecycle run deletes or moves objects, so the proxy refuses to run a disabled rule.';
  if (row.paused) return 'Resume the rule first. The proxy refuses to run a paused lifecycle rule.';
  return null;
}

/**
 * Preview and run-now act on the SAVED rule. While that kind's editor has
 * unsaved edits they would silently use the old definition — a lifecycle
 * run could delete by the rule the operator just changed — so they wait
 * until the edits are applied or discarded. Returns the reason, or null.
 */
export function draftBlocksAction(
  action: JobAction,
  row: Pick<JobRow, 'id'>,
  edited: ReadonlySet<string>,
): string | null {
  if (action !== 'preview' && action !== 'run-now') return null;
  return edited.has(row.id)
    ? 'Apply or discard your unsaved edits to this rule first — this would use the saved rule.'
    : null;
}

/**
 * Ids of the rules whose editor value differs from the saved one: the
 * server id (`<kind>:<name>`) of an applied rule with edits, and the draft
 * id (see `editorRuleJobId`) of a rule that exists only in the editor. Only
 * those rules block Run now and Preview, and only those show the "edited"
 * marker. A rule is matched to its saved twin by its uid, never by name.
 */
export function editedRuleIds(
  kind: 'replication' | 'lifecycle',
  rules: readonly NamedRule[],
  baseline: readonly NamedRule[],
): Set<string> {
  const saved = new Map(baseline.map((r) => [ruleUid(r), r]));
  const applied = appliedUidByName(baseline);
  const out = new Set<string>();
  for (const r of rules) {
    if (!isAppliedRule(r, applied)) out.add(editorRuleJobId(kind, r, baseline));
    else if (stableJson(saved.get(ruleUid(r))) !== stableJson(r)) out.add(`${kind}:${r.name}`);
  }
  return out;
}

/**
 * The server ids (`<kind>:<name>`) that an apply of this editor creates or
 * changes: the jobs to watch after the apply, because the scheduler starts
 * them a moment later.
 */
export function ruleIdsChangedByApply(
  kind: 'replication' | 'lifecycle',
  rules: readonly NamedRule[],
  baseline: readonly NamedRule[],
): string[] {
  const edited = editedRuleIds(kind, rules, baseline);
  return rules
    .filter((r) => r.name && edited.has(editorRuleJobId(kind, r, baseline)))
    .map((r) => `${kind}:${r.name}`);
}

/**
 * Names of saved rules that an apply of this editor deletes or changes. Data
 * cached for such a name (the Verify tab's parity result) describes a rule
 * that will not exist any more.
 */
export function savedRulesChangedByApply(rules: readonly NamedRule[], baseline: readonly NamedRule[]): string[] {
  const now = new Map(rules.map((r) => [ruleUid(r), r]));
  return baseline
    .filter((b) => {
      const r = now.get(ruleUid(b));
      return !r || r.name !== b.name || stableJson(r) !== stableJson(b);
    })
    .map((b) => b.name);
}

/** Marks the key of a draft job id: rule names never contain it. */
export const DRAFT_KEY_MARK = '#';

/** What the table and the drawer call a draft whose name is empty. */
export const UNNAMED_RULE = '(unnamed)';

/** Saved rule name → uid. */
function appliedUidByName(baseline: readonly NamedRule[]): Map<string, string> {
  return new Map(baseline.map((r) => [r.name, ruleUid(r)]));
}

/** The editor rule still carries the name it has on the server. */
function isAppliedRule(rule: NamedRule, applied: Map<string, string>): boolean {
  return !!rule.name && applied.get(rule.name) === ruleUid(rule);
}

/**
 * The job id of an editor rule. A rule that carries its saved name has the
 * server id `<kind>:<name>`. Every other rule (new, renamed, or with an
 * empty name) has a draft id `<kind>:#<uid>`, which stays the same while
 * the operator types its name.
 */
export function editorRuleJobId(
  kind: 'replication' | 'lifecycle',
  rule: NamedRule,
  baseline: readonly NamedRule[],
): string {
  return isAppliedRule(rule, appliedUidByName(baseline))
    ? `${kind}:${rule.name}`
    : `${kind}:${DRAFT_KEY_MARK}${ruleUid(rule)}`;
}

/**
 * The index in `rules` of the editor rule that a job-id key names, or -1. A
 * draft key names the rule by uid. A server key names the saved rule, and
 * the editor rule is the one with that rule's uid — also when the operator
 * renamed it, and never a draft that only has the same name.
 */
export function findEditorRule(key: string, rules: readonly NamedRule[], baseline: readonly NamedRule[]): number {
  if (key.startsWith(DRAFT_KEY_MARK)) {
    const uid = key.slice(DRAFT_KEY_MARK.length);
    return rules.findIndex((r) => ruleUid(r) === uid);
  }
  const saved = baseline.find((r) => r.name === key);
  if (!saved) return -1;
  const uid = ruleUid(saved);
  return rules.findIndex((r) => ruleUid(r) === uid);
}

/** JSON with sorted object keys: equal values give equal strings. */
function stableJson(v: unknown): string {
  return JSON.stringify(v, (_k, x) => {
    if (x === null || typeof x !== 'object' || Array.isArray(x)) return x;
    const o: Record<string, unknown> = {};
    for (const k of Object.keys(x as Record<string, unknown>).sort()) o[k] = (x as Record<string, unknown>)[k];
    return o;
  });
}

/**
 * Toast after a successful run-now. Both rule kinds answer 202 and run in the
 * background (the body carries no final counts), so the toast points at the
 * row and the Runs tab, which poll until the run settles.
 */
export function runNowMessage(_kind: JobKind, _result: unknown): string {
  return 'Run started — progress shows in the row and the Runs tab';
}

/**
 * A run row of the Runs tab with the live numbers of the list row laid over
 * it. The list polls; the runs table does not, so the running run's meter
 * moves with the list. A rule row carries no `started_at` on older servers:
 * the newest in-flight run is the one that is going.
 */
export function overlayLiveRun<R extends Pick<JobRunEntry, 'started_at' | 'status'>>(
  runs: R[],
  row: JobRow | null,
): Array<R & { __percent?: number | null }> {
  if (!row || !isActiveJobStatus(row.status)) return runs;
  let done = false;
  return runs.map((r) => {
    const match =
      !done &&
      isActiveJobStatus(r.status) &&
      (row.started_at == null || r.started_at === row.started_at);
    if (!match) return r;
    done = true;
    return {
      ...r,
      status: row.status,
      objects_processed: row.progress.processed,
      objects_skipped: row.progress.skipped,
      objects_deleted: row.progress.deleted ?? (r as { objects_deleted?: number | null }).objects_deleted,
      errors: row.progress.failed,
      bytes: row.progress.bytes,
      objects_scanned: row.progress.processed + row.progress.skipped + row.progress.failed,
      __percent: row.percent ?? null,
    };
  });
}

/** Runs tab poll: fast while any run in the table is still in flight, else off. */
export function runsPollInterval(runs: Pick<JobRunEntry, 'status'>[]): number | false {
  return runs.some((r) => isActiveJobStatus(r.status)) ? ACTIVE_POLL_MS : false;
}

/** Compact progress label for the table row. */
export function progressLabel(row: JobRow): string {
  if (row.trigger === 'oneoff') {
    if (row.status === 'queued') return 'waiting to start…';
    const done = row.progress.processed + row.progress.skipped;
    const total = row.progress.total;
    if (row.phase === 'counting') return 'counting objects…';
    if (total != null) return `${done} / ${total} objects`;
    // A re-encrypt or backfill walks the bucket once and learns its total
    // at the end, unless the bucket's usage counter knew it.
    if (row.phase === 'objects') return `${done} objects (total known at the end)`;
    return `${done} objects`;
  }
  const lifetime = row.lifetime?.objects ?? 0;
  return lifetime > 0 ? `${lifetime} objects lifetime` : '—';
}

/** Active one-off job touching `bucket` (busy chips on the Buckets page). */
export function busyJobForBucket(rows: JobRow[], bucket: string): JobRow | null {
  const key = bucket.toLowerCase();
  return (
    rows.find(
      (r) =>
        r.trigger === 'oneoff' &&
        r.scope.bucket.toLowerCase() === key &&
        isActiveJobStatus(r.status)
    ) ?? null
  );
}

/**
 * Tag color + label for a parity finding kind (the Verify tab findings table).
 * Pure so the mapping is unit-tested without rendering. AntD Tag colors:
 * missing=amber/gold, orphan(extra)=blue, mismatch=red.
 */
export function parityKindMeta(kind: string): { label: string; color: string } {
  switch (kind) {
    case 'missing_on_dest':
      return { label: 'Missing on dest', color: 'gold' };
    case 'orphan_on_dest':
      return { label: 'Extra on dest', color: 'blue' };
    case 'checksum_mismatch':
      return { label: 'Checksum mismatch', color: 'red' };
    default:
      return { label: kind, color: 'default' };
  }
}

/** Human label for a conflict policy (the rule-context line). */
export function conflictPolicyLabel(p: ConflictPolicy): string {
  switch (p) {
    case 'newer-wins':
      return 'newer wins';
    case 'content-diff':
      return 'content diff';
    case 'skip-if-dest-exists':
      return 'skip if destination exists';
    default:
      return p;
  }
}

/**
 * The verdict chip for "will re-running the rule fix this finding?" — pure so
 * the AntD-Tag color + tone are unit-tested without rendering. Discriminated on
 * `verdict`: `yes` → green; `conditional` → blue; `no` → gold for the soft
 * "needs a delete / not ours" cases, red for the hard policy lies (re-run
 * provably skips / keeps the dest / keeps failing).
 */
export function rerunVerdictMeta(rerun: RerunVerdict): {
  /** SHORT chip label — the verdict only. The specific cause is `cause`. */
  label: string;
  /** One-line cause, shown next to the chip (NOT duplicated by reason_detail). */
  cause?: string;
  color: string;
  tone: 'good' | 'bad' | 'maybe';
} {
  switch (rerun.verdict) {
    case 'yes':
      return { label: 'Re-run fixes this', color: 'green', tone: 'good' };
    case 'conditional':
      return rerun.why === 'transient_copy_error_may_clear'
        ? { label: 'Re-run may help', cause: 'transient error — retry', color: 'blue', tone: 'maybe' }
        : { label: 'Depends on timestamps', color: 'blue', tone: 'maybe' };
    case 'no': {
      // Gold = soft (an out-of-band delete is the real fix, nothing lied);
      // red = the hard policy lie (re-run runs but provably won't help).
      const soft = rerun.why === 'orphan_needs_delete';
      const cause =
        rerun.why === 'policy_skips_existing_dest'
          ? 'policy skips existing destination'
          : rerun.why === 'dest_newer_than_source'
            ? 'destination is newer'
            : rerun.why === 'tied_timestamps_no_winner'
              ? 'timestamps tied, no winner'
              : rerun.why === 'orphan_needs_delete'
                ? 'needs a delete'
                : 'copy keeps failing';
      return { label: "Re-run won't help", cause, color: soft ? 'gold' : 'red', tone: 'bad' };
    }
    default:
      return { label: "Re-run won't help", color: 'red', tone: 'bad' };
  }
}

/**
 * The guided-action affordance for a fix. `runnable` is true ONLY for the one
 * executable action (`run_now`); everything else is instructional text. `how`
 * is the one-line operator guidance (rendered as muted helper text + native
 * `title`). Discriminated on `action`. `failureDetail` (the finding's
 * `fix_detail`) feeds the copy-failure how-to when present.
 */
export function fixActionMeta(
  fix: FixAction,
  failureDetail?: string
): { label: string; runnable: boolean; how?: string } {
  switch (fix.action) {
    case 'run_now':
      return { label: 'Run now', runnable: true };
    case 'change_conflict_policy':
      return {
        label: `Change policy to ${fix.to}`,
        runnable: false,
        how: "Edit the rule's conflict policy in Definition, then re-verify.",
      };
    case 'enable_replicate_deletes':
      return {
        label: 'Enable mirror-delete',
        runnable: false,
        how: 'Set replicate_deletes on the rule, then run it.',
      };
    case 'copy_overwrite':
      return {
        label: 'Overwrite manually',
        runnable: false,
        how: 'Copy the object to the destination (Browser → bulk copy), then re-verify.',
      };
    case 'delete_from_dest':
      return {
        label: fix.foreign ? 'Delete foreign object' : 'Delete from destination',
        runnable: false,
        how: 'Remove it via Browser → bulk delete.',
      };
    case 'resolve_copy_failure':
      return {
        label: 'Fix the copy error',
        runnable: false,
        how: failureDetail || 'Resolve the underlying copy error, then re-run.',
      };
    case 'manual_review':
      return { label: 'Review manually', runnable: false };
    default:
      return { label: 'Review manually', runnable: false };
  }
}

/**
 * A rule definition as the section editors carry it. The name is enough to
 * merge; the scope fields (replication `source`/`destination`, lifecycle
 * `bucket`/`prefix`) fill the Scope column of a draft row.
 */
export interface NamedRule {
  name: string;
  bucket?: string;
  prefix?: string;
  source?: { bucket: string; prefix?: string };
  destination?: { bucket: string; prefix?: string };
}

export interface JobDisplayRow {
  row: JobRow;
  /** Staged in the editor but not yet applied to the server. */
  draft: boolean;
  /** Present on the server but deleted in the editor (pending removal). */
  pendingDelete: boolean;
  /** Present on the server with unapplied edits in the editor. */
  edited?: boolean;
}

/** The Scope column of a rule as the editor holds it. */
function ruleScope(rule: NamedRule): JobRow['scope'] {
  if (rule.source) {
    return {
      bucket: rule.source.bucket ?? '',
      prefix: rule.source.prefix || undefined,
      target: rule.destination?.bucket || undefined,
    };
  }
  return { bucket: rule.bucket ?? '', prefix: rule.prefix || undefined };
}

/** One line for the Scope column: `bucket/prefix → target`, or "". */
export function scopeText(scope: JobRow['scope']): string {
  if (!scope.bucket && !scope.target) return '';
  return `${scope.bucket}${scope.prefix ? `/${scope.prefix}` : ''}${scope.target ? ` → ${scope.target}` : ''}`;
}

/** One rule editor's staged rules and its last server snapshot. */
export interface RuleEditorRules {
  rules: readonly NamedRule[];
  baseline: readonly NamedRule[];
}

/**
 * Merge server job rows with the two editors' staged rule lists. Editor
 * rules are matched to server rows by uid (`isAppliedRule`), not by name:
 * - a server rule that no editor rule carries under its saved name gets
 *   `pendingDelete` (Apply deletes it; a rename is a delete and a create,
 *   because the server knows a rule by its name only);
 * - every other editor rule (new, renamed, nameless) is a DRAFT row with a
 *   draft id (`editorRuleJobId`); a nameless one shows as "(unnamed)";
 * - server rules in `edited` get the edited marker.
 * One-off jobs pass through untouched.
 */
export function mergeDraftRules(
  serverRows: JobRow[],
  replication: RuleEditorRules,
  lifecycle: RuleEditorRules,
  edited: ReadonlySet<string> = new Set(),
): JobDisplayRow[] {
  const out: JobDisplayRow[] = [];
  const editors = { replication, lifecycle };
  const applied = {
    replication: appliedUidByName(replication.baseline),
    lifecycle: appliedUidByName(lifecycle.baseline),
  };
  const carried = (kind: 'replication' | 'lifecycle') =>
    new Set(editors[kind].rules.filter((r) => isAppliedRule(r, applied[kind])).map((r) => r.name));
  const editorNames = { replication: carried('replication'), lifecycle: carried('lifecycle') };
  const serverNames = { replication: new Set<string>(), lifecycle: new Set<string>() };

  for (const row of serverRows) {
    if (row.kind === 'replication' || row.kind === 'lifecycle') {
      serverNames[row.kind].add(row.name);
      const pendingDelete = !editorNames[row.kind].has(row.name);
      out.push({ row, draft: false, pendingDelete, edited: !pendingDelete && edited.has(row.id) });
    } else {
      out.push({ row, draft: false, pendingDelete: false });
    }
  }

  for (const kind of ['replication', 'lifecycle'] as const) {
    const { rules, baseline } = editors[kind];
    for (const rule of rules) {
      const isApplied = isAppliedRule(rule, applied[kind]);
      // An applied rule shows as its server row. Until the jobs list has it
      // (the moment after an apply), it shows as a draft.
      if (isApplied && serverNames[kind].has(rule.name)) continue;
      out.push({
        draft: true,
        pendingDelete: false,
        row: {
          id: editorRuleJobId(kind, rule, baseline),
          kind,
          name: rule.name || UNNAMED_RULE,
          scope: ruleScope(rule),
          trigger: kind === 'replication' ? 'continuous' : 'scheduled',
          enabled: undefined,
          paused: undefined,
          status: 'idle',
          status_raw: 'draft',
          progress: { processed: 0, bytes: 0, failed: 0, skipped: 0 },
          detail: {},
        },
      });
    }
  }
  return out;
}

/**
 * How to bring a rule editor in line after the server deleted one of its
 * rules. Without this the editor still carries the rule, mergeDraftRules
 * shows it as a DRAFT, and the next Apply re-creates it.
 * - clean editor → re-fetch (value AND baseline lose the rule);
 * - dirty editor → drop the rule from the value only, keeping the other
 *   edits. The baseline still holds it, so a later discard must refresh
 *   (see `editorAfterDiscard`).
 */
export function planRuleDeleteSync<T extends NamedRule>(
  dirty: boolean,
  rules: T[],
  baseline: readonly T[],
  name: string,
): { action: 'refresh' } | { action: 'filter'; rules: T[] } {
  if (!dirty) return { action: 'refresh' };
  // Drop the rule that IS the deleted one (by uid, still under that name):
  // a draft that only has the same name stays, and so does a rename of the
  // deleted rule (the operator's new rule).
  const idx = findEditorRule(name, rules, baseline);
  if (idx < 0 || rules[idx].name !== name) return { action: 'filter', rules };
  return { action: 'filter', rules: rules.filter((_, i) => i !== idx) };
}

/**
 * Discard for a rule editor. `baselineStale` = a rule was deleted on the
 * server while the editor was dirty: its baseline still holds that rule, so a
 * plain discard would resurrect it — re-fetch server truth instead.
 */
export function editorAfterDiscard(baselineStale: boolean): 'discard' | 'refresh' {
  return baselineStale ? 'refresh' : 'discard';
}

/**
 * "Run now → re-verify" waits for the run to settle: the verify endpoint 409s
 * while the replication run holds its lease, and run-now returns 202 at once.
 * Fed each jobs-list sample of `replication:<rule>`; `start` is true on the
 * active→terminal edge, or when `last_run_at` moved past the value captured at
 * click time (a fast run that finished between two polls, never seen active).
 */
export interface PendingReverify {
  baselineLastRunAt: number | null;
  sawActive: boolean;
}

export function stepPendingReverify(
  p: PendingReverify,
  row: Pick<JobRow, 'status' | 'last_run_at'> | null,
): { next: PendingReverify | null; start: boolean } {
  if (!row) return { next: p, start: false };
  if (isActiveJobStatus(row.status)) {
    return { next: p.sawActive ? p : { ...p, sawActive: true }, start: false };
  }
  const ranSince = (row.last_run_at ?? 0) > (p.baselineLastRunAt ?? 0);
  if (p.sawActive || ranSince) return { next: null, start: true };
  return { next: p, start: false };
}

// ── Words per job kind ────────────────────────────────────────────────────

/** How a lifecycle rule acts on an object (from the rule definition). */
export type LifecycleVerb = 'delete' | 'move' | 'copy';

/** The lifecycle verb of a rule's `action` (editor shape or row detail). */
export function lifecycleVerb(action: unknown, deleteSourceAfterSuccess?: boolean): LifecycleVerb | undefined {
  if (action === 'delete' || action === 'retain-newest') return 'delete';
  if (action && typeof action === 'object') {
    const a = action as { type?: string; delete_source_after_success?: boolean };
    if (a.type === 'retain-newest') return 'delete';
    if (a.type === 'transition' || a.type === 'archive') return a.delete_source_after_success ? 'move' : 'copy';
  }
  if (action === 'transition') return deleteSourceAfterSuccess === false ? 'copy' : deleteSourceAfterSuccess ? 'move' : undefined;
  return undefined;
}

interface KindWords {
  /** Past participle for an object the run acted on: "copied", "stamped". */
  done: string;
  /** For an object the run left alone because it needs nothing. */
  skipped: string;
  /** The run found objects and none needed work. */
  nothing: string;
}

/**
 * The words a meter uses for each job kind. Replication copies; lifecycle
 * deletes, moves or copies; backfill stamps metadata; re-encrypt rewrites;
 * migrate copies to the target backend. "In sync" is only true for
 * replication.
 */
function kindWords(kind: JobKind, verb?: LifecycleVerb): KindWords {
  switch (kind) {
    case 'replication':
      return { done: 'copied', skipped: 'already in sync', nothing: 'in sync' };
    case 'lifecycle':
      return {
        done: verb === 'delete' ? 'deleted' : verb === 'move' ? 'moved' : verb === 'copy' ? 'copied' : 'acted on',
        skipped: 'not due',
        nothing: 'nothing due',
      };
    case 'backfill-metadata':
      return { done: 'stamped', skipped: 'already stamped', nothing: 'all already stamped' };
    case 'reencrypt':
      return { done: 're-encrypted', skipped: 'already current', nothing: 'all already current' };
    case 'migrate':
      return { done: 'copied', skipped: 'already on the target', nothing: 'nothing to copy' };
    default:
      return { done: 'processed', skipped: 'skipped', nothing: 'nothing to do' };
  }
}

/**
 * The word for a run's phase, or null when the phase has no word worth
 * showing (the plain object walk). Migrate: stage → copy → verify → flip →
 * cleanup.
 */
export function phaseWord(kind: JobKind, phase: string | null | undefined): string | null {
  switch (phase) {
    case 'counting':
      return 'counting objects';
    case 'references':
      return 'finishing shared reference files';
    case 'stage':
      return 'preparing';
    case 'copy':
      return kind === 'migrate' ? 'copying' : null;
    case 'verify':
      return 'verifying the copies';
    case 'flip':
      return 'switching to the new backend';
    case 'cleanup':
      return 'cleaning up';
    case 'scanning':
      return 'scanning';
    case 'copying':
      return 'copying';
    case 'deleting':
      return 'deleting';
    default:
      return null;
  }
}

// ── Outcome meter (the calm-by-default run-result visual) ─────────────────
type MeterState = 'in-sync' | 'copied' | 'errors' | 'mixed' | 'running' | 'waiting' | 'cancelling' | 'idle';

export interface OutcomeMeterInput {
  scanned: number;
  copied: number;
  errors: number;
  skipped: number;
  status: string;
  /** Live percent for a running run; null/undefined = unknown (indeterminate). */
  percent?: number | null;
  /** Selects the words (default: replication's). */
  kind?: JobKind;
  /** Objects the run deleted (replication mirror delete). */
  deleted?: number | null;
  /** The run's phase (migrate: stage|copy|verify|flip|cleanup). */
  phase?: string | null;
  /** Lifecycle: what the rule does to an object. */
  verb?: LifecycleVerb;
}

export interface MeterView {
  state: MeterState;
  greenPct: number;
  redPct: number;
  dot: 'green' | 'red' | 'amber' | 'muted';
  label: string;
  aria: string;
}

const n = (x: number) => x.toLocaleString();

/** PURE: the whole meter is a function of a run's numbers + status. Color is an
 *  attention budget — the dominant all-skipped "in sync" case stays calm, and
 *  saturated green/red is spent only on work that happened or failed. */
export function deriveMeter(p: OutcomeMeterInput): MeterView {
  const { scanned, copied, errors, skipped, status, percent } = p;
  const kind = p.kind ?? 'replication';
  const w = kindWords(kind, p.verb);
  // Lifecycle `processed` already counts the deletes of a delete rule.
  const deleted = kind === 'lifecycle' ? 0 : Math.max(0, p.deleted ?? 0);
  const acted = copied + deleted + errors;

  const parts = (withZeroDone: boolean) => {
    const out: string[] = [];
    if (copied > 0 || withZeroDone) out.push(`${n(copied)} ${w.done}`);
    if (deleted > 0) out.push(`${n(deleted)} deleted`);
    if (skipped > 0) out.push(`${n(skipped)} ${w.skipped}`);
    if (errors > 0) out.push(`${n(errors)} failed`);
    return out;
  };

  const aria =
    `${n(scanned)} scanned, ${n(copied)} ${w.done}, ` +
    (deleted > 0 ? `${n(deleted)} deleted, ` : '') +
    `${n(skipped)} ${w.skipped}, ` +
    `${n(errors)} error${errors === 1 ? '' : 's'}` +
    (percent != null && isActiveJobStatus(status) ? `, ${percent}% done` : '') +
    `; status ${jobStatusLabel({ status })}`;

  if (status === 'queued') {
    return { state: 'waiting', greenPct: 0, redPct: 0, dot: 'muted', label: 'waiting to start', aria };
  }
  if (status === 'cancelling') {
    const so = parts(false);
    return {
      state: 'cancelling',
      greenPct: 0,
      redPct: 0,
      dot: 'amber',
      label: so.length > 0 ? `cancelling · ${so.join(' · ')}` : 'cancelling',
      aria,
    };
  }
  if (status === 'running') {
    const known = percent != null;
    const head = phaseWord(kind, p.phase) ?? 'running';
    return {
      state: 'running',
      greenPct: known ? Math.max(0, Math.min(100, percent)) : 0,
      redPct: 0,
      dot: 'amber',
      label: [head, ...parts(true)].join(' · ') + (known ? '' : '…'),
      aria,
    };
  }
  if (status === 'cancelled' || status === 'paused' || status === 'deferred') {
    const so = parts(false);
    // `paused`: a pause stopped the run and it kept its position.
    // `deferred`: the proxy stopped it; it kept its position too.
    const word = status === 'paused' ? 'stopped' : status === 'deferred' ? 'deferred' : 'cancelled';
    return {
      state: 'idle',
      greenPct: 0,
      redPct: 0,
      dot: 'muted',
      label: so.length > 0 ? `${word} · ${so.join(' · ')}` : word,
      aria,
    };
  }
  if (status === 'failed' && acted === 0) {
    return { state: 'errors', greenPct: 0, redPct: 100, dot: 'red', label: 'failed', aria };
  }
  if (acted === 0) {
    return {
      state: 'in-sync',
      greenPct: 0,
      redPct: 0,
      dot: 'muted',
      label: scanned === 0 ? 'no objects' : `${w.nothing} · ${n(scanned)} checked`,
      aria,
    };
  }
  const greenPct = ((copied + deleted) / acted) * 100;
  const redPct = (errors / acted) * 100;
  const label = parts(false).join(' · ');
  if (errors > 0 && copied + deleted > 0) {
    return { state: 'mixed', greenPct, redPct, dot: 'red', label, aria };
  }
  if (errors > 0) {
    return { state: 'errors', greenPct, redPct, dot: 'red', label, aria };
  }
  return { state: 'copied', greenPct, redPct, dot: 'green', label, aria };
}

/**
 * The meter input for a list row: live progress of the run in progress, or
 * of the last one-off run. `scanned` counts failures too (a failed object
 * was scanned).
 */
export function rowMeterInput(row: JobRow, verb?: LifecycleVerb): OutcomeMeterInput {
  const pr = row.progress;
  return {
    scanned: pr.processed + pr.skipped + pr.failed,
    copied: pr.processed,
    errors: pr.failed,
    skipped: pr.skipped,
    deleted: pr.deleted ?? 0,
    status: row.status,
    percent: row.percent ?? null,
    kind: row.kind,
    phase: row.phase ?? null,
    verb: verb ?? lifecycleVerb(row.detail?.action),
  };
}

/**
 * The meter input for a Runs-tab row. `objects_scanned` of an older server
 * left out the failed objects ("6 scanned, 6 copied, 1 error"), so the
 * scanned count is never below the sum of its parts.
 */
export function runMeterInput(
  run: Pick<JobRunEntry, 'objects_scanned' | 'objects_processed' | 'objects_skipped' | 'errors' | 'status'> & {
    objects_deleted?: number | null;
    __percent?: number | null;
  },
  kind: JobKind,
  verb?: LifecycleVerb,
  phase?: string | null,
): OutcomeMeterInput {
  return {
    scanned: Math.max(run.objects_scanned, run.objects_processed + run.objects_skipped + run.errors),
    copied: run.objects_processed,
    errors: run.errors,
    skipped: run.objects_skipped,
    deleted: run.objects_deleted ?? 0,
    status: run.status,
    percent: run.__percent ?? null,
    kind,
    phase: phase ?? null,
    verb,
  };
}

/** How long a run took (or is taking): "1m", "12s"; null when unknown. */
export function runDuration(startedAt: number | null | undefined, finishedAt: number | null | undefined, nowSec: number): string | null {
  if (!startedAt) return null;
  const end = finishedAt ?? nowSec;
  return formatDuration(Math.max(0, end - startedAt));
}

/**
 * The "Last run" cell. A run in progress shows how long it has run (the
 * previous run's time beside it would read as this run's); otherwise the
 * time the last run FINISHED.
 */
export function lastRunCell(
  row: Pick<JobRow, 'status' | 'trigger' | 'last_run_at' | 'started_at' | 'finished_at' | 'created_at'>,
  nowSec: number,
): { running: string | null; finishedAt: number | null } {
  const finishedAt = row.trigger === 'oneoff' ? (row.finished_at ?? null) : (row.last_run_at ?? null);
  if (row.status === 'running' || row.status === 'cancelling') {
    const d = runDuration(row.started_at, null, nowSec);
    return { running: d ? `running for ${d}` : 'running now', finishedAt: row.trigger === 'oneoff' ? null : finishedAt };
  }
  if (row.status === 'queued') return { running: 'not started yet', finishedAt: null };
  return { running: null, finishedAt };
}

/** "next run in ~40 min" for a scheduled rule, or null. */
export function nextRunText(
  row: Pick<JobRow, 'trigger' | 'enabled' | 'paused' | 'next_due_at' | 'status'>,
  nowMs: number,
  schedulerOn = true,
): string | null {
  if (row.trigger === 'oneoff' || !row.next_due_at || !schedulerOn) return null;
  if (row.enabled === false || row.paused || isActiveJobStatus(row.status)) return null;
  const until = approxTimeUntil(row.next_due_at * 1000, nowMs);
  return until === 'now' ? 'next run: due now' : `next run in ${until}`;
}

/** "deferred: bucket 'x' has an active maintenance job; …" — why the
 *  scheduler did not run a due lifecycle rule (server `detail.deferred`). */
export function deferredText(row: Pick<JobRow, 'detail'>): string | null {
  const why = row.detail?.deferred;
  return typeof why === 'string' && why ? `deferred: ${why}` : null;
}

/** "20 most recent of 1,000" over the Failures tab; null when all shown. */
export function failuresCountText(shown: number, total: number | null | undefined): string | null {
  if (total == null || total <= shown) return null;
  return `The ${n(shown)} most recent of ${n(total)} failures.`;
}

/**
 * Identical error messages folded together, most frequent first. Only
 * messages seen more than once are returned: the table still lists every
 * object, this is the summary above it.
 */
export function groupFailureErrors(failures: readonly { error: string }[]): { error: string; count: number }[] {
  const counts = new Map<string, number>();
  for (const f of failures) counts.set(f.error, (counts.get(f.error) ?? 0) + 1);
  return [...counts.entries()]
    .filter(([, c]) => c > 1)
    .map(([error, count]) => ({ error, count }))
    .sort((a, b) => b.count - a.count);
}

/**
 * What the Status cell of a row shows. A run in progress, and a one-off
 * (its row IS its run), show the meter. A settled rule shows its status
 * word, and under it the meter of its last run when that run has counts,
 * so "14 copied" stays after the run.
 */
export function statusCellView(
  row: Pick<JobRow, 'status' | 'trigger' | 'paused' | 'enabled' | 'progress'>,
  draft = false,
): { meter: boolean; tag: boolean } {
  if (draft) return { meter: false, tag: true };
  if (isActiveJobStatus(row.status) || row.trigger === 'oneoff') return { meter: true, tag: false };
  const pr = row.progress;
  const counts = pr.processed + pr.skipped + pr.failed + (pr.deleted ?? 0) > 0;
  return { meter: counts, tag: true };
}

/** What event-driven replication did for a rule (`detail.events`). */
interface EventActivity {
  last_applied_at?: number | null;
  copied?: number;
  deleted?: number;
  failed?: number;
  last_failed_at?: number | null;
}

/**
 * The live (event-driven) copies of a replication rule since the proxy
 * started: when the last one landed, and the counts. Null when there were
 * none. Rendered as "last live copy <time> · 14 copied · 1 deleted".
 */
export function liveCopySummary(row: Pick<JobRow, 'kind' | 'detail'>): { at: number | null; counts: string } | null {
  if (row.kind !== 'replication') return null;
  const ev = (row.detail?.events ?? null) as EventActivity | null;
  const at = (typeof row.detail?.last_event_applied_at === 'number' ? row.detail.last_event_applied_at : null) ?? ev?.last_applied_at ?? null;
  const parts: string[] = [];
  if (ev?.copied) parts.push(`${n(ev.copied)} copied`);
  if (ev?.deleted) parts.push(`${n(ev.deleted)} deleted`);
  if (ev?.failed) parts.push(`${n(ev.failed)} failed`);
  if (at == null && parts.length === 0) return null;
  return { at, counts: parts.join(' · ') };
}

/**
 * The Definition tab of a one-off job: its parameters, what it did, and
 * when. Read-only rows of [label, value].
 */
export function oneOffDefinition(row: JobRow, nowSec: number): Array<[string, string]> {
  const d = row.detail ?? {};
  const w = kindWords(row.kind);
  const out: Array<[string, string]> = [['Bucket', row.scope.bucket]];
  if (row.kind === 'migrate') {
    if (typeof d.from_backend === 'string') out.push(['From backend', d.from_backend]);
    const to = typeof d.target_backend === 'string' ? d.target_backend : row.scope.target;
    if (to) out.push(['To backend', to]);
    if (typeof d.target === 'string')
      out.push([
        'Destination',
        d.target === 'mirror' ? 'made an exact mirror of the source' : 'must be empty before the copy',
      ]);
    if (typeof d.delete_source === 'boolean')
      out.push(['Source copy', d.delete_source ? 'deleted after the switch' : 'kept after the switch']);
  } else if (row.scope.target) {
    out.push(['Target', row.scope.target]);
  }
  if (row.kind === 'backfill-metadata' && typeof d.refresh_last_modified === 'boolean')
    out.push([
      'Last-Modified',
      d.refresh_last_modified ? 'set to the time of the backfill' : 'kept as it was',
    ]);
  const phase = phaseWord(row.kind, row.phase);
  if (phase && isActiveJobStatus(row.status)) out.push(['Phase', phase]);
  const pr = row.progress;
  const total = row.total ?? pr.total;
  out.push([
    'Objects',
    [
      `${n(pr.processed)} ${w.done}`,
      `${n(pr.skipped)} ${w.skipped}`,
      ...(pr.deleted ? [`${n(pr.deleted)} deleted`] : []),
      `${n(pr.failed)} failed`,
      ...(total != null ? [`of ${n(total)}`] : []),
    ].join(' · '),
  ]);
  out.push(['Data', formatBytes(pr.bytes)]);
  out.push(['Created', fmtTs(row.created_at)]);
  out.push(['Started', fmtTs(row.started_at)]);
  out.push(['Finished', fmtTs(row.finished_at)]);
  const dur = runDuration(row.started_at, row.finished_at, nowSec);
  if (dur) out.push([row.finished_at ? 'Took' : 'Running for', dur]);
  return out;
}

function fmtTs(ts?: number | null): string {
  return ts ? new Date(ts * 1000).toLocaleString() : '—';
}

/**
 * What Cancel of a one-off does, for its confirmation. A migration
 * cancelled before the switch deletes the copies it made so far.
 */
export function cancelConfirmText(row: Pick<JobRow, 'kind' | 'phase' | 'scope'>): { title: string; content: string } {
  const bucket = row.scope.bucket;
  if (row.kind === 'migrate') {
    const preFlip = row.phase == null || ['stage', 'copy', 'verify'].includes(row.phase);
    return {
      title: `Cancel the migration of "${bucket}"?`,
      content: preFlip
        ? 'The proxy deletes the copies it made on the target backend so far, and the bucket stays on its current backend. A new migration starts the copy again from the beginning.'
        : 'The bucket already uses the new backend. Cancel stops the clean-up of the old copies; the bucket stays on the new backend.',
    };
  }
  return {
    title: `Cancel the ${kindLabel(row.kind).toLowerCase()} job of "${bucket}"?`,
    content:
      'The job stops at the next safe point. The objects it already processed stay processed; a new job does the rest.',
  };
}

/**
 * Fold one poll sample into a smoothed objects/sec rate. Pure so it's testable.
 * Returns the new EMA, or the previous rate unchanged when there's no forward
 * progress (the DB flushes the count in chunks, so most 2s polls show Δ=0 — a
 * raw Δ/Δt would flicker to 0 and read as "dead"). `null` prev / non-positive
 * dt / backwards count → treat as no update.
 */
export function computeRate(
  prevRate: number | null,
  prevScanned: number,
  prevTs: number,
  scanned: number,
  ts: number,
  alpha = 0.4,
): number {
  const dScanned = scanned - prevScanned;
  const dt = (ts - prevTs) / 1000;
  if (dScanned <= 0 || dt <= 0) return Math.max(0, prevRate ?? 0);
  const inst = dScanned / dt;
  const next = prevRate == null ? inst : alpha * inst + (1 - alpha) * prevRate;
  return Math.max(0, next);
}

/**
 * How far away a moment is, rounded for a reader: "~40 min", "~13 h",
 * "~3 days". A moment that is already here or past is "now".
 */
export function approxTimeUntil(atMs: number, nowMs: number): string {
  const minutes = Math.round((atMs - nowMs) / 60_000);
  if (minutes <= 0) return 'now';
  if (minutes < 60) return `~${minutes} min`;
  const hours = Math.round(minutes / 60);
  if (hours < 48) return `~${hours} h`;
  return `~${Math.round(hours / 24)} days`;
}

/**
 * The lifecycle run-now confirm button names what the run does and to how
 * many objects: "Run: delete 2 objects". The verb comes from the preview's
 * candidate sample (one kind of action → that verb, a mix → "act on").
 */
export function lifecycleRunLabel(affected: number, actions: string[]): string {
  const kinds = new Set(actions);
  const verb = kinds.size === 1 ? (kinds.has('delete') ? 'delete' : kinds.has('transition') ? 'transition' : 'act on') : 'act on';
  return `Run: ${affected === 0 ? 'nothing to' : verb} ${affected === 0 ? 'do' : `${affected.toLocaleString()} object${affected === 1 ? '' : 's'}`}`;
}
