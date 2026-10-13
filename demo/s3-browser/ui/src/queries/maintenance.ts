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
import { ACTIVE_POLL_MS, IDLE_POLL_MS } from '../jobsView';
import { bucketStatusPollMs } from '../maintenanceStatus';
import { qk } from './keys';

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
    // No session (anonymous browser, or one that expired) → the endpoint
    // 401s. Do not retry at once; poll on, slower, so the banner returns
    // after the user signs in again.
    retry: false,
    // Idle floor (10 s): catches a job that starts while the user browses
    // before an upload hits the 503.
    refetchInterval: (query) =>
      bucketStatusPollMs(!!query.state.data?.active, !!query.state.error, ACTIVE_POLL_MS, IDLE_POLL_MS),
  });
}
