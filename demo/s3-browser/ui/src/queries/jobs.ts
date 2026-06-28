/**
 * Unified jobs queries. The list polls fast (2s) while anything is live
 * — a running one-off or a running rule — and goes quiet otherwise.
 */
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import {
  cancelVerifyParity,
  getJobFailures,
  getJobRuns,
  getJobs,
  getVerifyStatus,
  runJobAction,
  startVerifyParity,
} from '../adminApi';
import type { ParityStatus } from '../adminApi';
import { isActiveJobStatus } from '../jobsView';
import { qk } from './keys';

const POLL_MS = 2000;

export function useJobs(opts?: { enabled?: boolean }) {
  return useQuery({
    queryKey: qk.jobs.list(),
    queryFn: getJobs,
    enabled: opts?.enabled ?? true,
    refetchInterval: (query) => {
      const jobs = query.state.data?.jobs ?? [];
      return jobs.some((j) => isActiveJobStatus(j.status)) ? POLL_MS : false;
    },
  });
}

export function useJobRuns(id: string | null) {
  return useQuery({
    queryKey: qk.jobs.runs(id ?? ''),
    queryFn: () => getJobRuns(id as string),
    enabled: !!id,
  });
}

export function useJobFailures(id: string | null) {
  return useQuery({
    queryKey: qk.jobs.failures(id ?? ''),
    queryFn: () => getJobFailures(id as string),
    enabled: !!id,
  });
}

/**
 * Server-side parity verification status for a replication rule. The audit is a
 * BACKGROUND job: the result is persisted server-side, so this query gives an
 * instant cached verdict on mount (survives navigation + restart) and polls
 * (2s) while a scan is running. `useStartVerify` POSTs to kick one off.
 */
export function useParityStatus(ruleName: string) {
  return useQuery<ParityStatus, Error>({
    queryKey: qk.jobs.verify(ruleName),
    queryFn: () => getVerifyStatus(ruleName),
    enabled: !!ruleName,
    refetchInterval: (q) => {
      const st = q.state.data?.status;
      return st === 'running' || st === 'cancelling' ? POLL_MS : false;
    },
  });
}

/** Kick off the background parity audit, then refetch its status. */
export function useStartVerify(ruleName: string) {
  const qc = useQueryClient();
  return useMutation<ParityStatus, Error, void>({
    mutationFn: () => startVerifyParity(ruleName),
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
 * Invalidates the jobs list so the row's status reflects the run.
 */
export function useRunReplicationNow() {
  const qc = useQueryClient();
  return useMutation<unknown, Error, string>({
    mutationFn: (ruleName: string) =>
      runJobAction(`replication:${ruleName}`, 'run-now'),
    onSuccess: () => {
      qc.invalidateQueries({ queryKey: qk.jobs.list() });
    },
  });
}
