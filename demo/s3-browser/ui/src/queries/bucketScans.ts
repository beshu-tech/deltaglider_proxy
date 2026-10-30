/**
 * Persistent bucket-scan totals, shared by the dashboard card and the
 * analytics page (one cache entry, one 30 s poll for both).
 *
 * The poll catches scans that another tab started and that settled in the
 * background; the tab that runs a scan follows it over SSE
 * (useBucketScanQueue) and invalidates the status when a scan ends. Every
 * read is best-effort: a failure keeps what is on screen, so no retry.
 */
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { getAllBucketScans, getAllBucketUsage } from '../adminApi';
import { listBuckets } from '../s3client';
import { qk } from './keys';

export function useBucketScans() {
  return useQuery({
    queryKey: qk.bucketScans.status(),
    queryFn: getAllBucketScans,
    refetchInterval: 30_000,
    // Each mount reads the server's `running` map fresh: the analytics page
    // re-attaches to scans that kept running while it was unmounted.
    refetchOnMount: 'always',
    retry: false,
  });
}

export function useInvalidateBucketScans() {
  const qc = useQueryClient();
  return () => void qc.invalidateQueries({ queryKey: qk.bucketScans.status() });
}

/** Names of the buckets a scan can run on (an unavailable backend's are left out). */
export function useScannableBuckets() {
  return useQuery({
    queryKey: qk.bucketScans.buckets(),
    queryFn: async () => (await listBuckets()).filter((b) => !b.unavailable).map((b) => b.name),
    retry: false,
  });
}

/** O(1) usage counters of every bucket in one request (null without admin rights). */
export function useAllBucketUsage(enabled: boolean) {
  return useQuery({
    queryKey: qk.bucketScans.usage(),
    queryFn: getAllBucketUsage,
    enabled,
    retry: false,
  });
}
