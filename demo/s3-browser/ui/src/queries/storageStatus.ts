import { useQuery } from '@tanstack/react-query';
import { getStorageStatus } from '../adminApi';
import { qk } from './keys';

/** The cadence of the capacity poll. A background tab does not poll (client default). */
const POLL_MS = 30_000;

/**
 * The disk-capacity status. Any valid session may read it: an admin gets the
 * full view, every other session the browser view (`isAdminStatus` tells them
 * apart). No session answers 401: no retry, the data stays unknown.
 */
export function useStorageStatus(enabled = true) {
  return useQuery({
    queryKey: qk.storageStatus(),
    queryFn: getStorageStatus,
    enabled,
    retry: false,
    refetchInterval: POLL_MS,
    staleTime: POLL_MS / 2,
  });
}
