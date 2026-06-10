/**
 * Storage-backend list read query.
 *
 * BackendsPanel keeps its create/delete/encryption mutations inline (they're
 * compound section PUTs with bespoke result messaging), but the READ path moves
 * here so the list is keyed by `qk.backends.list()` and shared with anything
 * else that reads backends. Mutations invalidate this key + `qk.config()` after
 * a write so the list and the cached config both refresh.
 */
import { useQuery } from '@tanstack/react-query';
import { getBackends, getBucketOrigins } from '../adminApi';
import { qk } from './keys';

export function useBackends() {
  return useQuery({
    queryKey: qk.backends.list(),
    queryFn: getBackends,
  });
}

/**
 * Bucket→backend origin map (admin-only). Authoritative virtual→backend
 * mapping for BOTH filesystem and S3 backends — used to count buckets per
 * backend and to badge a bucket's origin. Cached so the count chip and the
 * header badge don't each re-fetch.
 */
export function useBucketOrigins(opts?: { enabled?: boolean }) {
  return useQuery({
    queryKey: qk.backends.origins(),
    queryFn: getBucketOrigins,
    enabled: opts?.enabled ?? true,
  });
}
