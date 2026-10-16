/**
 * Unified jobs queries. The list polls fast (2s) while anything is live
 * — a running one-off or a running rule — or while the page watches a job
 * after an action (see `JobWatch`), and every 10 s otherwise, so a
 * scheduled run or a job started in another tab still shows up.
 */
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  cancelVerifyParity,
  getJob,
  getJobFailures,
  getJobRuns,
  getJobs,
  getLifecyclePreviewStatus,
  getRunList,
  getVerifyStatus,
  previewLifecycleJob,
  runJobAction,
  startVerifyParity,
} from '../adminApi';
import type { ParityStatus, RunFilters, VerifyScope } from '../adminApi';
import { isActiveJobStatus, jobsPollInterval, runsPollInterval, ACTIVE_POLL_MS } from '../jobsView';
import type { JobWatch } from '../jobsView';
import { qk } from './keys';

/**
 * Faster poll for the parity-verify progress bar (800ms vs 2s generic).
 * The server's wall-clock flush throttle (250ms) writes intermediate counts
 * fast enough that 800ms captures them; each poll takes the same no-WAL DB
 * mutex, so going below 800ms risks contention without UI benefit.
 */
const VERIFY_POLL_MS = 800;

export function useJobs(opts?: { enabled?: boolean; watches?: readonly JobWatch[] }) {
  const watches = opts?.watches ?? [];
  return useQuery({
    queryKey: qk.jobs.list(),
    queryFn: getJobs,
    enabled: opts?.enabled ?? true,
    refetchInterval: (query) => jobsPollInterval(query.state.data?.jobs ?? [], watches),
  });
}

/**
 * One job by id, for a deep link to a job the list does not carry (an old
 * one-off). Polls while the job is live.
 */
export function useJob(id: string | null) {
  return useQuery({
    queryKey: qk.jobs.one(id ?? ''),
    queryFn: () => getJob(id as string),
    enabled: !!id,
    retry: false,
    refetchInterval: (query) => (query.state.data && isActiveJobStatus(query.state.data.status) ? ACTIVE_POLL_MS : false),
  });
}

export function useJobRuns(id: string | null) {
  return useQuery({
    queryKey: qk.jobs.runs(id ?? ''),
    queryFn: () => getJobRuns(id as string),
    enabled: !!id,
    // run-now is async: keep the Runs tab live until the new run settles.
    refetchInterval: (query) => runsPollInterval(query.state.data?.runs ?? []),
  });
}

export function useJobFailures(id: string | null, runId?: number) {
  return useQuery({
    queryKey: runId === undefined ? qk.jobs.failures(id ?? '') : [...qk.jobs.failures(id ?? ''), runId],
    queryFn: () => getJobFailures(id as string, runId),
    enabled: !!id,
  });
}

/**
 * The runs list of the Jobs page, page by page. It polls fast while a run
 * in it is live, and every 10 s otherwise, so a scheduled run or a job
 * started in another tab shows up.
 */
export function useRunList(filters: RunFilters) {
  return useInfiniteQuery({
    queryKey: qk.jobs.runList(JSON.stringify(filters)),
    queryFn: ({ pageParam }) => getRunList(filters, pageParam),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.next_cursor,
    refetchInterval: (query) =>
      jobsPollInterval((query.state.data?.pages ?? []).flatMap((p) => p.runs)),
  });
}

/**
 * How long a lifecycle preview is reused. The proxy computes one by walking
 * the rule's whole scope with a HEAD per object, so each open of the Preview
 * tab or of the run-now confirmation must not compute a new one.
 */
const LIFECYCLE_PREVIEW_STALE_MS = 5 * 60_000;

/**
 * What a lifecycle rule would do now (a dry run: POST, but no writes). Shared
 * by the run-now confirmation and the drawer's Preview tab, so the
 * confirmation reuses the preview the tab computed. No polling and no
 * background refetch. A preview stays fresh for LIFECYCLE_PREVIEW_STALE_MS,
 * also while no view shows it; "Refresh preview" recomputes it, and a run
 * or an apply of the storage section drops it.
 */
export function useLifecyclePreview(id: string | null) {
  return useQuery({
    queryKey: qk.jobs.preview(id ?? ''),
    queryFn: () => previewLifecycleJob(id as string),
    enabled: !!id,
    staleTime: LIFECYCLE_PREVIEW_STALE_MS,
    gcTime: LIFECYCLE_PREVIEW_STALE_MS,
    refetchOnWindowFocus: false,
  });
}

/**
 * "Refresh preview": a new walk of the rule's scope. The proxy also keeps
 * a finished preview for a minute, so a plain refetch could answer from it;
 * this asks for a fresh walk and puts its answer in the shared query.
 */
export function useRefreshLifecyclePreview(id: string | null) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: () => previewLifecycleJob(id as string, true),
    onSuccess: (data) => qc.setQueryData(qk.jobs.preview(id ?? ''), data),
  });
}

/** How often the progress of a running preview walk is read. */
const PREVIEW_PROGRESS_POLL_MS = 1000;

/**
 * The counters of the rule's running preview walk, read every second while
 * `active` (a preview request waits), so the GUI shows the walk advance
 * instead of a bare spinner. Reading it starts no walk.
 */
export function useLifecyclePreviewProgress(id: string | null, active: boolean) {
  return useQuery({
    queryKey: qk.jobs.previewProgress(id ?? ''),
    queryFn: () => getLifecyclePreviewStatus(id as string),
    enabled: !!id && active,
    refetchInterval: active ? PREVIEW_PROGRESS_POLL_MS : false,
    gcTime: 0,
    refetchOnWindowFocus: false,
  });
}

/**
 * Server-side parity verification status for a replication rule. The audit is a
 * BACKGROUND job: the result is persisted server-side, so this query gives an
 * instant cached verdict on mount (survives navigation + restart) and polls
 * (800ms while running — faster than the 2s generic jobs poll) so the progress
 * bar animates smoothly. `useStartVerify` POSTs to kick one off.
 */
export function useParityStatus(ruleName: string) {
  return useQuery<ParityStatus, Error>({
    queryKey: qk.jobs.verify(ruleName),
    queryFn: () => getVerifyStatus(ruleName),
    enabled: !!ruleName,
    refetchInterval: (q) => {
      const st = q.state.data?.status;
      return st === 'running' || st === 'cancelling' ? VERIFY_POLL_MS : false;
    },
  });
}

/** Kick off the background parity audit, then refetch its status.
 *  `mutate('unresolved')` re-checks only the objects the last audit could
 *  not read; `mutate()` audits the whole mirror. */
export function useStartVerify(ruleName: string) {
  const qc = useQueryClient();
  return useMutation<ParityStatus, Error, VerifyScope | void>({
    mutationFn: (scope) => startVerifyParity(ruleName, scope ?? 'all'),
    onSuccess: (data) => {
      qc.setQueryData(qk.jobs.verify(ruleName), data);
      qc.invalidateQueries({ queryKey: qk.jobs.verify(ruleName) });
    },
  });
}

/** Cancel a running parity audit; refetch status so the UI settles. */
export function useCancelVerify(ruleName: string) {
  const qc = useQueryClient();
  return useMutation<ParityStatus, Error, void>({
    mutationFn: () => cancelVerifyParity(ruleName),
    onSuccess: (data) => {
      qc.setQueryData(qk.jobs.verify(ruleName), data);
      qc.invalidateQueries({ queryKey: qk.jobs.verify(ruleName) });
    },
  });
}

/**
 * Run a replication rule now — the ONLY executable per-finding fix in the
 * Verify tab (re-uses the shared run-now action; everything else is guidance).
 * Invalidates every jobs query (list, runs, failures, verify) so the row's
 * status AND the drawer's Runs tab reflect the run.
 */
export function useRunReplicationNow() {
  const qc = useQueryClient();
  return useMutation<unknown, Error, string>({
    mutationFn: (ruleName: string) =>
      runJobAction(`replication:${ruleName}`, 'run-now'),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.jobs.all() });
    },
  });
}
