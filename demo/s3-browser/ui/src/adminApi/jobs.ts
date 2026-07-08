// === Unified jobs API (replication / lifecycle / reencrypt / migrate) ===
import { throwApiError } from '../errorHandling';
import { adminFetch, fetchJson, safeJson } from './core';
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
  bytes: number;
  errors: number;
}

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
  | 'rule_owned_orphan_source_deleted'
  | 'foreign_orphan';

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
        | 'foreign_not_ours'
        | 'copy_keeps_failing';
    }
  | { verdict: 'conditional'; why: 'newer_wins_depends_on_timestamps' };

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
  foreign_orphans: number;
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
}

export async function getJobs(): Promise<JobsOverview> {
  return fetchJson('/api/admin/jobs', 'Jobs');
}

/**
 * On-demand source-vs-dest parity audit for a replication rule.
 * Server-side background job. POST kicks one off (202 + running status); the
 * result is persisted server-side so it survives navigation + restart. Poll
 * `getVerifyStatus` for progress + the final verdict.
 */
export interface ParityStatus {
  status: 'idle' | 'running' | 'cancelling' | 'done' | 'failed' | 'cancelled';
  progress_scanned: number;
  /** Compare-phase denominator (0/absent = unknown → indeterminate bar). */
  progress_total?: number;
  scanned_at?: number;
  outcome?: ParityOutcome;
  error?: string;
}

/** POST: start (or report) the background parity audit. */
export async function startVerifyParity(ruleName: string): Promise<ParityStatus> {
  const res = await adminFetch(
    `/api/admin/jobs/replication:${encodeURIComponent(ruleName)}/verify`,
    'POST'
  );
  if (!res.ok) await throwApiError(res, 'Verify replication parity');
  return safeJson(res);
}

/** GET: poll the current parity audit status / last result (no scan started). */
export async function getVerifyStatus(ruleName: string): Promise<ParityStatus> {
  return fetchJson(
    `/api/admin/jobs/replication:${encodeURIComponent(ruleName)}/verify`,
    'Verify status'
  );
}

/** POST: cancel a running parity audit. */
export async function cancelVerifyParity(ruleName: string): Promise<ParityStatus> {
  const res = await adminFetch(
    `/api/admin/jobs/replication:${encodeURIComponent(ruleName)}/verify/cancel`,
    'POST'
  );
  if (!res.ok) await throwApiError(res, 'Cancel verification');
  return safeJson(res);
}

export async function getJobRuns(id: string): Promise<{ runs: JobRunEntry[] }> {
  return fetchJson(`/api/admin/jobs/${encodeURIComponent(id)}/runs`, 'Job runs');
}

export async function getJobFailures(id: string): Promise<{ failures: JobFailureEntry[] }> {
  return fetchJson(`/api/admin/jobs/${encodeURIComponent(id)}/failures`, 'Job failures');
}

/** Uniform action dispatch; returns the action's JSON payload (if any). */
export async function runJobAction(id: string, action: JobAction): Promise<unknown> {
  const res = await adminFetch(`/api/admin/jobs/${encodeURIComponent(id)}/${action}`, 'POST');
  if (!res.ok) await throwApiError(res, `Job ${action}`);
  if (res.status === 204) return null;
  return safeJson(res);
}

/** Queue re-encryption jobs for the given buckets. */
export async function startReencrypt(buckets: string[]): Promise<{
  started: Array<{ bucket: string; job_id: number }>;
  errors: Array<{ bucket: string; error: string }>;
}> {
  const res = await adminFetch('/api/admin/jobs/reencrypt', 'POST', { buckets });
  if (!res.ok) await throwApiError(res, 'Start re-encryption');
  return safeJson(res);
}

/** Create a durable migrate job; returns 202 with the job id. */
export async function createMigrateJob(
  bucket: string,
  targetBackend: string,
  deleteSource: boolean
): Promise<{ job_id: number; id: string; bucket: string; from_backend: string; to_backend: string }> {
  const res = await adminFetch(`/api/admin/buckets/${encodeURIComponent(bucket)}/migrate`, 'POST', {
    target_backend: targetBackend,
    delete_source: deleteSource,
  });
  if (!res.ok) await throwApiError(res, `Migrate ${bucket}`);
  return safeJson(res);
}
