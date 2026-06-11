/**
 * Unified jobs queries. The list polls fast (2s) while anything is live
 * — a running one-off or a running rule — and goes quiet otherwise.
 */
import { useQuery } from '@tanstack/react-query';
import { getJobFailures, getJobRuns, getJobs } from '../adminApi';
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
