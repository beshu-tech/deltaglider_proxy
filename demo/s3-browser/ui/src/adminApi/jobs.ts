// === Unified jobs API (replication / lifecycle / reencrypt / migrate) ===
import { adminJson, adminRequest, safeJson } from './core';
import type { JobAction, JobRow } from '../jobsView';

interface JobsOverview {
  workers: {
    replication: { enabled: boolean; tick_interval: string; last_event_applied_at?: number | null };
    lifecycle: { enabled: boolean; tick_interval: string };
  };
  jobs: JobRow[];
}

interface JobRunEntry {
  id: number;
  triggered_by: string;
  started_at: number;
  finished_at?: number | null;
  status: string;
  status_raw: string;
  objects_scanned: number;
  objects_processed: number;
  objects_skipped: number;
  objects_deleted?: number | null;
  /** LOGICAL bytes the run moved. */
  bytes: number;
  errors: number;
  // Strategy mix (replication only; 0 elsewhere). Straight passthrough =
  // objects_processed − delta_passthrough − reconstructed.
  delta_passthrough?: number;
  bytes_egress_saved?: number;
  reconstructed?: number;
  // Lifecycle only: what the run did with the objects it acted on, and why
  // it skipped the others.
  objects_moved?: number;
  objects_copied?: number;
  objects_excluded?: number;
  objects_not_due?: number;
}
export type { JobRunEntry };

interface JobFailureEntry {
  id: number;
  run_id?: number | null;
  occurred_at: number;
  object_key: string;
  bucket?: string | null;
  source_key?: string | null;
  dest_key?: string | null;
  error: string;
}

// === Replication parity audit (the "Verify" tab) ===
export type Verifier = 'sha256' | 'etag_size' | 'size_only';
type FindingKind = 'match' | 'checksum_mismatch' | 'missing_on_dest' | 'orphan_on_dest';

/** The rule's conflict policy (kebab-case on the wire — see ConflictPolicy). */
export type ConflictPolicy = 'newer-wins' | 'content-diff' | 'skip-if-dest-exists';

/** The diagnosed cause of a finding (mirrors backend `ReasonCode`). */
export type ReasonCode =
  | 'never_copied'
  | 'copy_failing'
  | 'source_modified_after_copy'
  | 'dest_modified_after_copy'
  | 'diverged_same_timestamp'
  | 'diverged_unknown_age'
  | 'rule_owned_orphan_source_deleted';

/** Tri-state "will a re-run fix this?" verdict (never a bool). Discriminated on `verdict`. */
export type RerunVerdict =
  | { verdict: 'yes' }
  | {
      verdict: 'no';
      why:
        | 'policy_skips_existing_dest'
        | 'dest_newer_than_source'
        | 'tied_timestamps_no_winner'
        | 'orphan_needs_delete'
        | 'copy_keeps_failing';
    }
  | {
      verdict: 'conditional';
      why: 'newer_wins_depends_on_timestamps' | 'transient_copy_error_may_clear';
    };

/** The guided fix. Discriminated on `action`; only `run_now` is executable. */
export type FixAction =
  | { action: 'run_now' }
  | { action: 'copy_overwrite' }
  | { action: 'delete_from_dest'; foreign: boolean }
  | { action: 'change_conflict_policy'; to: ConflictPolicy }
  | { action: 'enable_replicate_deletes' }
  | { action: 'resolve_copy_failure' }
  | { action: 'manual_review' };

/** Cause + policy-aware re-run verdict + guided fix, per finding. */
export interface Remediation {
  reason: ReasonCode;
  rerun_helps: RerunVerdict;
  fix: FixAction;
  /** Human, ≤1 line — backend already wrote good copy; prefer rendering this. */
  reason_detail: string;
  fix_detail: string;
}

export interface ParityFinding {
  key: string;
  kind: FindingKind;
  verifier?: Verifier;
  unverifiable: boolean;
  detail: string;
  /** Cause + "will re-run help?" + guided fix. Absent until the backend annotates. */
  remediation?: Remediation;
}

/** Sample-scoped tally of remediation verdicts (NOT exact totals — see count fields). */
export interface ActionableSummary {
  rerun_fixes: number;
  rerun_conditional: number;
  needs_manual: number;
  copy_failing: number;
}

export interface ParityOutcome {
  rule_name: string;
  source_bucket: string;
  dest_bucket: string;
  source_objects: number;
  dest_objects: number;
  matched: number;
  missing_on_dest: number;
  orphan_on_dest: number;
  checksum_mismatch: number;
  unverifiable: number;
  truncated: boolean;
  /** WHY the scan is partial: the listing cap was actually reached (raise
   *  `DGP_PARITY_MAX_OBJECTS`) — vs `unresolved` reads (re-run the audit).
   *  Optional: absent on rows persisted before these fields existed. */
  cap_hit?: boolean;
  /** Objects dropped from the compare because their read kept failing
   *  transiently (throttling). A re-check of these objects usually clears it. */
  unresolved?: number;
  /** The objects behind `unresolved` (at most 1,000), sorted by destination
   *  key. A re-check (`startVerifyParity(rule, 'unresolved')`) reads only these.
   *  Absent on rows persisted before the field existed. */
  unresolved_keys?: UnresolvedKey[];
  /** True when `unresolved` is larger than `unresolved_keys` (the list hit
   *  its cap): only a full audit can read the objects the list leaves out. */
  unresolved_keys_capped?: boolean;
  /** When a re-check last merged into this result (unix SECONDS).
   *  `scanned_at` stays the time of the full scan. */
  rechecked_at?: number;
  /** THE signal: true iff !truncated && missing/orphan/mismatch/unverifiable all 0. */
  in_sync: boolean;
  scanned_at: number; // unix SECONDS
  /** How logical facts were sourced: 'pure_mirror' (from the listing — zero
   *  per-object reads) vs 'transforming' (HEAD-resolved). Same compare. */
  regime?: 'pure_mirror' | 'transforming';
  /** The rule's conflict policy — sets up WHY the verdicts read as they do. */
  conflict_policy: ConflictPolicy;
  /** Whether the rule mirrors source deletes to the destination. */
  replicate_deletes: boolean;
  /** Sample-scoped remediation tally (see ActionableSummary). */
  actionable: ActionableSummary;
  missing_samples: ParityFinding[];
  orphan_samples: ParityFinding[];
  mismatch_samples: ParityFinding[];
  /** THE authoritative conclusion, derived server-side. The UI renders this
   *  verdict + its summary instead of re-deriving tone from the raw counts, so
   *  the halo and the headline can never disagree. Optional for pre-field rows. */
  verdict?: Verdict;
  /** Plain-language sentence matching `verdict` — shown verbatim as the body. */
  verdict_summary?: string;
}

/** One object a parity audit could not read. */
export interface UnresolvedKey {
  dest_key: string;
  /** Absent for a destination-only object. */
  source_key?: string;
  /** The error of its last failed read, "source: …" or "destination: …". */
  error?: string;
}

/** What a running parity audit does now. Each phase counts objects against
 *  its own total; a listing phase starts from the previous audit's count. */
export type ParityPhase = 'listing_source' | 'listing_destination' | 'reading' | 'comparing';

/** The single server-derived conclusion of a parity audit. Internal to the
 *  outcome shape (consumed via `ParityOutcome.verdict`); not re-exported. */
type Verdict = 'safe' | 'incomplete' | 'at_risk';

export async function getJobs(): Promise<JobsOverview> {
  return adminJson('/api/admin/jobs', { context: 'Jobs' });
}

/**
 * On-demand source-vs-dest parity audit for a replication rule.
 * Server-side background job. POST kicks one off (202 + running status); the
 * result is persisted server-side so it survives navigation + restart. Poll
 * `getVerifyStatus` for progress + the final verdict.
 */
export interface ParityStatus {
  status: 'idle' | 'running' | 'cancelling' | 'done' | 'failed' | 'cancelled';
  /** Objects done in the current `phase`. */
  progress_scanned: number;
  /** Objects of the current `phase` (0/absent = unknown → indeterminate bar). */
  progress_total?: number;
  /** What a running audit does now (absent when none runs). */
  phase?: ParityPhase;
  scanned_at?: number;
  outcome?: ParityOutcome;
  /** While an audit runs: the result of the previous one (shown dimmed). */
  previous_outcome?: ParityOutcome;
  previous_scanned_at?: number;
  error?: string;
}

/** What a verify reads: the whole mirror, or only the objects the last audit
 *  could not read. */
export type VerifyScope = 'all' | 'unresolved';

/** POST: start (or report) the background parity audit. `unresolved` re-checks
 *  only the objects the last audit could not read (409 when there are none). */
export async function startVerifyParity(
  ruleName: string,
  scope: VerifyScope = 'all',
): Promise<ParityStatus> {
  const query = scope === 'unresolved' ? '?scope=unresolved' : '';
  return adminJson(`/api/admin/jobs/replication:${encodeURIComponent(ruleName)}/verify${query}`, {
    method: 'POST',
    context: scope === 'unresolved' ? 'Re-check unread objects' : 'Verify replication parity',
  });
}

/** GET: poll the current parity audit status / last result (no scan started). */
export async function getVerifyStatus(ruleName: string): Promise<ParityStatus> {
  return adminJson(`/api/admin/jobs/replication:${encodeURIComponent(ruleName)}/verify`, {
    context: 'Verify status',
  });
}

/** POST: cancel a running parity audit. */
export async function cancelVerifyParity(ruleName: string): Promise<ParityStatus> {
  return adminJson(`/api/admin/jobs/replication:${encodeURIComponent(ruleName)}/verify/cancel`, {
    method: 'POST',
    context: 'Cancel verification',
  });
}

/** One job row by id (also a one-off that the list no longer carries). */
export async function getJob(id: string): Promise<JobRow> {
  return adminJson(`/api/admin/jobs/${encodeURIComponent(id)}`, { context: 'Job' });
}

export async function getJobRuns(id: string): Promise<{ runs: JobRunEntry[] }> {
  return adminJson(`/api/admin/jobs/${encodeURIComponent(id)}/runs`, { context: 'Job runs' });
}

/** `total`: all failures recorded (the list holds the most recent ones). */
export async function getJobFailures(id: string): Promise<{ failures: JobFailureEntry[]; total?: number | null }> {
  return adminJson(`/api/admin/jobs/${encodeURIComponent(id)}/failures`, {
    context: 'Job failures',
  });
}

/** Uniform action dispatch; returns the action's JSON payload (if any). */
export async function runJobAction(id: string, action: JobAction, query = ''): Promise<unknown> {
  const res = await adminRequest(`/api/admin/jobs/${encodeURIComponent(id)}/${action}${query}`, {
    method: 'POST',
    context: `Job ${action}`,
  });
  if (res.status === 204) return null;
  return safeJson(res);
}

/** One object a lifecycle rule would act on (mirrors `PreviewObject`). */
interface LifecycleCandidate {
  bucket: string;
  key: string;
  action: 'delete' | 'transition' | string;
  destination_bucket?: string;
  destination_key?: string;
  delete_source_after_success: boolean;
  created_at: string;
  /** Logical (uncompressed) size. */
  size: number;
  /** Bytes on the backend (a delta is much smaller than `size`). */
  stored_size?: number;
}

/** `POST /jobs/lifecycle:<rule>/preview` (mirrors `LifecycleRunOutcome`).
 *  `candidates` is capped server-side; the counts are the full totals. */
export interface LifecyclePreview {
  objects_scanned: number;
  objects_affected: number;
  objects_skipped?: number;
  /** Of the skipped: kept out of the rule by a glob (or an internal key). */
  objects_excluded?: number;
  /** Of the skipped: not old enough yet. */
  objects_not_due?: number;
  /** Of the affected: deleted / moved (source deleted) / copied (source kept). */
  objects_deleted?: number;
  objects_moved?: number;
  objects_copied?: number;
  /** Logical size of the affected objects. */
  bytes_affected: number;
  /** Their bytes on the backend. */
  stored_bytes_affected?: number;
  candidates: LifecycleCandidate[];
  /** When the proxy's walk ended (RFC 3339): a preview can answer from an earlier walk. */
  computed_at?: string;
  /** When the first scanned object that does not match now will match (RFC 3339):
   *  its age reference plus `expire_after`. Absent when none will, and for retain-newest. */
  next_due_at?: string;
}

/** `GET /jobs/lifecycle:<rule>/preview`: the progress of the rule's running preview walk. */
export interface LifecyclePreviewStatus {
  running: boolean;
  started_at?: string;
  objects_scanned: number;
  /** Matches so far (0 for retain-newest until its walk ends). */
  objects_affected: number;
}

export async function getLifecyclePreviewStatus(id: string): Promise<LifecyclePreviewStatus> {
  return adminJson(`/api/admin/jobs/${encodeURIComponent(id)}/preview`, { context: 'Preview progress' });
}

/** `fresh`: never answer from a walk that already ended ("Refresh preview"). */
export async function previewLifecycleJob(id: string, fresh = false): Promise<LifecyclePreview> {
  return (await runJobAction(id, 'preview', fresh ? '?fresh=true' : '')) as LifecyclePreview;
}

/** Queue re-encryption jobs for the given buckets. */
export async function startReencrypt(buckets: string[]): Promise<{
  started: Array<{ bucket: string; job_id: number }>;
  errors: Array<{ bucket: string; error: string }>;
}> {
  return adminJson('/api/admin/jobs/reencrypt', {
    method: 'POST',
    body: { buckets },
    context: 'Start re-encryption',
  });
}

/** What the migrate does with objects already in the destination bucket:
 *  `empty` refuses a non-empty destination; `mirror` deletes the extras. */
export type MigrateTargetMode = 'empty' | 'mirror';
/**
 * Queue metadata-backfill jobs (stamp DG metadata onto objects written
 * without the proxy). `refreshLastModified: false` keeps each object's
 * served Last-Modified.
 */
export async function startBackfillMetadata(
  buckets: string[],
  refreshLastModified: boolean,
): Promise<{
  started: Array<{ bucket: string; job_id: number }>;
  errors: Array<{ bucket: string; error: string }>;
}> {
  return adminJson('/api/admin/jobs/backfill-metadata', {
    method: 'POST',
    body: { buckets, refresh_last_modified: refreshLastModified },
    context: 'Start metadata backfill',
  });
}

/** Create a durable migrate job; returns 202 with the job id. */
export async function createMigrateJob(
  bucket: string,
  targetBackend: string,
  deleteSource: boolean,
  target: MigrateTargetMode = 'empty'
): Promise<{ job_id: number; id: string; bucket: string; from_backend: string; to_backend: string }> {
  return adminJson(`/api/admin/buckets/${encodeURIComponent(bucket)}/migrate`, {
    method: 'POST',
    body: { target_backend: targetBackend, delete_source: deleteSource, target },
    context: `Migrate ${bucket}`,
  });
}
