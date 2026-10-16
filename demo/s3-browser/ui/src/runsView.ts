/**
 * Pure view logic of the Jobs page: the runs list (`GET /jobs/runs`).
 */
import type { RunEntry, RunFilters } from './adminApi';
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

/** The second line of the Job cell: what the run did, in words. */
export function runSubject(run: RunEntry): string {
  if (isRuleRun(run)) return run.run_id !== undefined ? `run #${run.run_id}` : '';
  if (run.kind === 'delete') {
    const prefixes = Array.isArray(run.detail?.prefixes) ? (run.detail.prefixes as string[]) : [];
    const objects = typeof run.detail?.objects === 'number' ? run.detail.objects : 0;
    const parts: string[] = [];
    if (prefixes.length === 1) parts.push(prefixes[0]);
    else if (prefixes.length > 1) parts.push(`${prefixes[0]} and ${prefixes.length - 1} more folder${prefixes.length > 2 ? 's' : ''}`);
    if (objects > 0) parts.push(`${objects.toLocaleString()} selected object${objects === 1 ? '' : 's'}`);
    return parts.join(' · ');
  }
  return '';
}

/** The name of the Job cell: the rule, or the bucket of a one-off. */
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
