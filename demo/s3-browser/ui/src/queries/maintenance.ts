/**
 * Maintenance (re-encryption) job queries.
 *
 * Polling model: while any job is ACTIVE the queries refetch every 2s so
 * progress bars move; once everything is terminal they go quiet (no
 * interval). `refetchInterval` receives the latest data, which is how the
 * conditional cadence is expressed in react-query v5.
 */
import { useQuery } from '@tanstack/react-query';
import { getBucketMaintenance } from '../adminApi';
import { qk } from './keys';

const POLL_MS = 2000;


/**
 * One bucket's active job (session-light — works for non-admin browser
 * sessions). Polls fast while a job is active (progress + self-dismiss),
 * and slowly otherwise so a job STARTED while the user is browsing is
 * still discovered without a reload.
 */
export function useBucketMaintenance(bucket: string | null) {
  return useQuery({
    queryKey: qk.maintenance.bucket(bucket ?? ''),
    queryFn: () => getBucketMaintenance(bucket as string),
    enabled: !!bucket,
    // No session (anonymous browser) → the endpoint 401s; stop polling
    // rather than hammering it. A signed-in session re-mounts the query.
    retry: false,
    // Idle floor 60s: with no job running this poll only needs to catch a
    // maintenance job STARTING (rare, operator-initiated) — 15s was chatty
    // for a permanently-mounted browser-view query.
    refetchInterval: (query) =>
      query.state.error ? false : query.state.data?.active ? POLL_MS : 60_000,
  });
}
