import { useQuery } from '@tanstack/react-query';
import { getBucketUsage } from '../adminApi';
import { bucketPolicyFor } from '../bucketPolicyLookup';
import type { UploadLimits } from '../uploadPrecheck';
import { useAdminConfig } from './config';
import { qk } from './keys';
import { useStorageStatus } from './storageStatus';

/**
 * The server's write limits for uploads into `bucket`: the largest file (every
 * session reads it from the storage status), the bucket quota and its used
 * bytes (admin sessions only: those reads are admin API; `enabled: false`
 * leaves them unknown). The server still enforces. A paused upload is never
 * a limit here: only a real refusal says so.
 */
export function useUploadLimits(bucket: string, enabled: boolean): UploadLimits {
  const { data: config } = useAdminConfig({ enabled });
  const { data: status } = useStorageStatus();
  const quota = enabled ? bucketPolicyFor(config, bucket)?.quota_bytes ?? null : null;
  const { data: usage } = useQuery({
    queryKey: qk.bucketUsage(bucket),
    queryFn: () => getBucketUsage(bucket),
    enabled: enabled && quota != null && !!bucket,
    staleTime: 15_000,
  });
  return {
    maxObjectSize: status?.largest_file ?? null,
    quotaBytes: quota,
    // No counter row reads as zeros + never_scanned: unknown, not empty.
    usedBytes: usage && !('disabled' in usage) && !(usage.never_scanned && usage.object_count === 0) ? usage.stored_bytes : null,
  };
}
