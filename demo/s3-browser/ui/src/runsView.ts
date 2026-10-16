/**
 * Pure view logic of the Jobs page: the runs list (`GET /jobs/runs`).
 */
import type { RunEntry, RunFilters } from './adminApi';
import type { JobRow } from './jobsView';
import { isActiveJobStatus, kindLabel, parseJobId } from './jobsView';

/** The page filters from the URL query (unknown values are dropped). */
export function runFiltersFromQuery(q: Record<string, string | undefined>): RunFilters {
  const f: RunFilters = {};
  if (q.status === 'active' || q.status === 'failed') f.status = q.status;
  if (q.kind) f.kind = q.kind;
  if (q.rule) f.rule = q.rule;
  if (q.bucket) f.bucket = q.bucket;
  if (q.noop === 'true') f.noop = true;
  return f;
}

/** The URL query of the page filters (and the open drawer). */
export function runFiltersToQuery(f: RunFilters, job?: string | null): Record<string, string> {
  const q: Record<string, string> = {};
  if (f.status) q.status = f.status;
  if (f.kind) q.kind = f.kind;
  if (f.rule) q.rule = f.rule;
  if (f.bucket) q.bucket = f.bucket;
  if (f.noop) q.noop = 'true';
  if (job) q.job = job;
  return q;
}

/** A rule run (it has a rule), not a one-off. */
export function isRuleRun(run: Pick<RunEntry, 'kind'>): boolean {
  return run.kind === 'replication' || run.kind === 'lifecycle';
}

/** The folders and objects a delete job removes. */
function deleteTargets(run: RunEntry): { prefixes: string[]; objects: number } {
  const prefixes = Array.isArray(run.detail?.prefixes) ? (run.detail.prefixes as string[]) : [];
  const objects = typeof run.detail?.objects === 'number' ? run.detail.objects : 0;
  return { prefixes, objects };
}

/**
 * The title of the Job cell, which tells one run from the next: the rule and
 * its run number, the first folder that a delete removes, or the bucket of
 * another one-off.
 */
export function runTitle(run: RunEntry): string {
  if (isRuleRun(run)) return run.run_id !== undefined ? `${runName(run)} #${run.run_id}` : runName(run);
  if (run.kind === 'delete') {
    const { prefixes, objects } = deleteTargets(run);
    const more = prefixes.length - 1 + (objects > 0 ? 1 : 0);
    if (prefixes.length > 0) return more > 0 ? `${prefixes[0]} +${more}` : prefixes[0];
    if (objects > 0) return `${objects.toLocaleString()} selected object${objects === 1 ? '' : 's'}`;
  }
  return runName(run);
}

/** The second line of the Job cell: the rest of what a delete removes. */
export function runSubject(run: RunEntry): string {
  if (run.kind !== 'delete') return '';
  const { prefixes, objects } = deleteTargets(run);
  if (prefixes.length === 0) return '';
  const parts: string[] = [];
  if (prefixes.length > 1) parts.push(`also ${prefixes.slice(1).join(', ')}`);
  if (objects > 0) parts.push(`${objects.toLocaleString()} selected object${objects === 1 ? '' : 's'}`);
  return parts.join(' · ');
}

/** The name of a run's rule, or the bucket of a one-off. */
export function runName(run: RunEntry): string {
  return isRuleRun(run) ? (parseJobId(run.job_id)?.key ?? run.name) : run.scope.bucket;
}

/** How the run started, and who started it. */
export function runOriginLabel(run: Pick<RunEntry, 'origin' | 'started_by'>): string {
  switch (run.origin) {
    case 'schedule':
      return 'Schedule';
    case 'run-now':
      return 'Run now';
    case 'file-browser':
      return run.started_by ? `File browser · ${run.started_by}` : 'File browser';
    case 'admin':
      return run.started_by && run.started_by !== 'admin' ? `Admin · ${run.started_by}` : 'Admin';
    default:
      return run.origin;
  }
}

/** The actions of a run: cancel or kill while it runs, remove a finished one-off. */
export type RunAction = 'cancel' | 'kill' | 'remove';

export function runActions(run: Pick<RunEntry, 'kind' | 'status'>): RunAction[] {
  const active = isActiveJobStatus(run.status);
  if (isRuleRun(run)) return run.status === 'running' ? ['kill'] : [];
  if (!active) return ['remove'];
  return run.status === 'cancelling' ? [] : ['cancel'];
}

/** The kinds the Kind filter offers. */
export const RUN_KIND_OPTIONS: { value: string; label: string }[] = [
  'replication',
  'lifecycle',
  'delete',
  'reencrypt',
  'migrate',
  'backfill-metadata',
].map((k) => ({ value: k, label: kindLabel(k) }));

/** A rule that reads or writes a bucket, and how. */
export interface BucketRule {
  id: string;
  kind: 'replication' | 'lifecycle';
  name: string;
  /** `source`: the rule copies from the bucket; `destination`: into it;
   *  `bucket`: a lifecycle rule acts on it; `archive`: a lifecycle rule
   *  moves objects into it. */
  role: 'source' | 'destination' | 'bucket' | 'archive';
}

/** The replication and lifecycle rules that read or write `bucket`, by name. */
export function rulesTouchingBucket(rows: readonly JobRow[], bucket: string): BucketRule[] {
  const out: BucketRule[] = [];
  for (const r of rows) {
    if (r.kind !== 'replication' && r.kind !== 'lifecycle') continue;
    const here = r.scope.bucket === bucket;
    const there = r.scope.target === bucket;
    if (!here && !there) continue;
    const role: BucketRule['role'] =
      r.kind === 'replication' ? (here ? 'source' : 'destination') : here ? 'bucket' : 'archive';
    out.push({ id: r.id, kind: r.kind, name: r.name, role });
  }
  return out.sort((a, b) => a.kind.localeCompare(b.kind) || a.name.localeCompare(b.name));
}
