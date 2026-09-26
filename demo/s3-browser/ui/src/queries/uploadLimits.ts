import { useQuery } from '@tanstack/react-query';
import { getBucketUsage } from '../adminApi';
import { bucketPolicyFor } from '../bucketPolicyLookup';
import type { UploadLimits } from '../uploadPrecheck';
import { useAdminConfig } from './config';
import { qk } from './keys';

/**
 * The server's write limits for uploads into `bucket`: max_object_size, the
 * bucket quota and its used bytes. Admin sessions only (the reads are admin
 * API); `enabled: false` returns all-unknown, and the server still enforces.
 */
export function useUploadLimits(bucket: string, enabled: boolean): UploadLimits {
  const { data: config } = useAdminConfig({ enabled });
  const quota = enabled ? bucketPolicyFor(config, bucket)?.quota_bytes ?? null : null;
  const { data: usage } = useQuery({
    queryKey: qk.bucketUsage(bucket),
    queryFn: () => getBucketUsage(bucket),
    enabled: enabled && quota != null && !!bucket,
    staleTime: 15_000,
  });
  return {
    maxObjectSize: enabled ? config?.max_object_size ?? null : null,
    quotaBytes: quota,
    // No counter row reads as zeros + never_scanned: unknown, not empty.
    usedBytes: usage && !('disabled' in usage) && !(usage.never_scanned && usage.object_count === 0) ? usage.stored_bytes : null,
  };
}
