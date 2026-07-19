/**
 * Pure view logic for the unified Jobs screen (React-free — transpiled
 * directly by the Node regression script).
 *
 * The backend's GET /api/admin/jobs returns ONE row shape for every
 * background operation (replication rules, lifecycle rules, one-off
 * re-encrypt/migrate jobs). These helpers turn rows into UI decisions:
 * status tones, kind labels, the per-kind action matrix, progress labels,
 * bucket-busy lookups, and the draft-row merge for staged-but-unapplied
 * rule definitions.
 */

import type { ConflictPolicy, FixAction, RerunVerdict } from './adminApi';

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
  status: string; // normalized: idle|queued|running|cancelling|succeeded|failed|cancelled
  status_raw: string;
  phase?: string;
  percent?: number | null;
  progress: { processed: number; total?: number | null; bytes: number; failed: number; skipped: number };
  lifetime?: { objects: number; bytes: number };
  last_run_at?: number | null;
  next_due_at?: number | null;
  created_at?: number | null;
  started_at?: number | null;
  finished_at?: number | null;
  last_error?: string | null;
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

export type StrategySegment = {
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

/** AntD tag color for a job row. Pause/disable win over the last status. */
export function jobStatusTone(row: Pick<JobRow, 'status' | 'paused' | 'enabled'>): string {
  if (row.enabled === false) return 'default';
  if (row.paused) return 'warning';
  switch (row.status) {
    case 'running':
    case 'cancelling':
      return 'processing';
    case 'queued':
      return 'warning';
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

/** The status word the row should display (pause/disable beat status). */
export function jobStatusLabel(row: Pick<JobRow, 'status' | 'paused' | 'enabled'>): string {
  if (row.enabled === false) return 'disabled';
  if (row.paused) return 'paused';
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
    default:
      return kind;
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
    out.push(row.paused ? 'resume' : 'pause');
    if (row.kind === 'lifecycle') out.push('preview');
    // run-now availability differs by kind (matches the backend contract):
    //  - replication: a deliberate ONE-OFF that runs even a disabled/paused
    //    rule once (without flipping the flag) — offer whenever not running.
    //  - lifecycle: the backend 409s a disabled OR paused rule, so only offer
    //    run-now for an enabled, non-paused, non-running lifecycle rule.
    const runnable =
      row.status !== 'running' &&
      (row.kind === 'replication' || (row.enabled !== false && !row.paused));
    if (runnable) out.push('run-now');
    // Kill the in-flight run at will (interrupts mid-object, unlike pause).
    // Replication only — the backend has no lifecycle-kill arm (would 400).
    if (row.kind === 'replication' && row.status === 'running') out.push('kill');
    out.push('delete');
    return out;
  }
  if (isActiveJobStatus(row.status) && row.status !== 'cancelling') out.push('cancel');
  return out;
}

/** Compact progress label for the table row. */
export function progressLabel(row: JobRow): string {
  if (row.trigger === 'oneoff') {
    if (row.status === 'queued') return 'waiting to start…';
    const done = row.progress.processed + row.progress.skipped;
    const total = row.progress.total;
    if (row.phase === 'counting') return 'counting objects…';
    return total != null ? `${done} / ${total} objects` : `${done} objects`;
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
      const soft = rerun.why === 'orphan_needs_delete' || rerun.why === 'foreign_not_ours';
      const cause =
        rerun.why === 'policy_skips_existing_dest'
          ? 'policy skips existing destination'
          : rerun.why === 'dest_newer_than_source'
            ? 'destination is newer'
            : rerun.why === 'tied_timestamps_no_winner'
              ? 'timestamps tied, no winner'
              : rerun.why === 'orphan_needs_delete'
                ? 'needs a delete'
                : rerun.why === 'foreign_not_ours'
                  ? 'not written by this rule'
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

/** A rule definition as the section editors carry it (name is enough here). */
export interface NamedRule {
  name: string;
}

export interface JobDisplayRow {
  row: JobRow;
  /** Staged in the editor but not yet applied to the server. */
  draft: boolean;
  /** Present on the server but deleted in the editor (pending removal). */
  pendingDelete: boolean;
}

/**
 * Merge server job rows with the two editors' staged rule lists:
 * editor-only rules surface as DRAFT rows (synthetic JobRow, idle); server
 * rules absent from the editor get `pendingDelete` (they vanish on Apply).
 * One-off jobs pass through untouched.
 */
export function mergeDraftRules(
  serverRows: JobRow[],
  replicationRules: NamedRule[],
  lifecycleRules: NamedRule[]
): JobDisplayRow[] {
  const out: JobDisplayRow[] = [];
  const editorNames = {
    replication: new Set(replicationRules.map((r) => r.name)),
    lifecycle: new Set(lifecycleRules.map((r) => r.name)),
  };
  const serverNames = { replication: new Set<string>(), lifecycle: new Set<string>() };

  for (const row of serverRows) {
    if (row.kind === 'replication' || row.kind === 'lifecycle') {
      serverNames[row.kind].add(row.name);
      out.push({
        row,
        draft: false,
        pendingDelete: !editorNames[row.kind].has(row.name),
      });
    } else {
      out.push({ row, draft: false, pendingDelete: false });
    }
  }

  for (const kind of ['replication', 'lifecycle'] as const) {
    const rules = kind === 'replication' ? replicationRules : lifecycleRules;
    for (const rule of rules) {
      if (!rule.name || serverNames[kind].has(rule.name)) continue;
      out.push({
        draft: true,
        pendingDelete: false,
        row: {
          id: `${kind}:${rule.name}`,
          kind,
          name: rule.name,
          scope: { bucket: '' },
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

// ── Outcome meter (the calm-by-default run-result visual) ─────────────────
export type MeterState = 'in-sync' | 'copied' | 'errors' | 'mixed' | 'running' | 'idle';

export interface OutcomeMeterInput {
  scanned: number;
  copied: number;
  errors: number;
  skipped: number;
  status: string;
  /** Live percent for a running run; null/undefined = unknown (indeterminate). */
  percent?: number | null;
}

export interface MeterView {
  state: MeterState;
  greenPct: number;
  redPct: number;
  dot: 'green' | 'red' | 'amber' | 'muted';
  label: string;
  aria: string;
}

/** PURE: the whole meter is a function of a run's numbers + status. Color is an
 *  attention budget — the dominant all-skipped "in sync" case stays calm, and
 *  saturated green/red is spent only on work that happened or failed. */
export function deriveMeter(p: OutcomeMeterInput): MeterView {
  const { scanned, copied, errors, skipped, status, percent } = p;
  const acted = copied + errors;
  const running = status === 'running' || status === 'queued' || status === 'cancelling';

  const aria =
    `${scanned.toLocaleString()} scanned, ${copied.toLocaleString()} copied, ` +
    `${skipped.toLocaleString()} skipped (already in sync), ` +
    `${errors.toLocaleString()} error${errors === 1 ? '' : 's'}` +
    `; status ${jobStatusLabel({ status })}`;

  if (running) {
    const known = percent != null;
    return {
      state: 'running',
      greenPct: known ? Math.max(0, Math.min(100, percent)) : 0,
      redPct: 0,
      dot: 'amber',
      label: known
        ? `running · ${copied.toLocaleString()} copied${errors > 0 ? ` · ${errors.toLocaleString()} err` : ''}`
        : `running · ${copied.toLocaleString()} copied…`,
      aria,
    };
  }
  if (status === 'cancelled') {
    return { state: 'idle', greenPct: 0, redPct: 0, dot: 'muted', label: 'cancelled', aria };
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
      label: scanned === 0 ? 'no objects' : 'in sync',
      aria,
    };
  }
  const greenPct = (copied / acted) * 100;
  const redPct = (errors / acted) * 100;
  if (errors > 0 && copied > 0) {
    return {
      state: 'mixed',
      greenPct,
      redPct,
      dot: 'red',
      label: `${copied.toLocaleString()} copied · ${errors.toLocaleString()} err`,
      aria,
    };
  }
  if (errors > 0) {
    return {
      state: 'errors',
      greenPct,
      redPct,
      dot: 'red',
      label: `${errors.toLocaleString()} error${errors === 1 ? '' : 's'}`,
      aria,
    };
  }
  return { state: 'copied', greenPct, redPct, dot: 'green', label: `${copied.toLocaleString()} copied`, aria };
}

/**
 * Verify progress bar model. `total > 0` (after listing finishes) → determinate
 * percent; else indeterminate. Pure so it's testable. Percent is clamped and
 * never regresses past 100 (a late-arriving count can exceed the denominator).
 */
export function deriveVerifyProgress(
  scanned: number,
  total: number | undefined,
): { determinate: boolean; percent: number } {
  if (!total || total <= 0) return { determinate: false, percent: 0 };
  const pct = Math.round((scanned / total) * 100);
  return { determinate: true, percent: Math.max(0, Math.min(100, pct)) };
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
