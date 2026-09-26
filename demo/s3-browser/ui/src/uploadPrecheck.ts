import { formatBytes } from './utils';

/** What the page knows of the server's write limits; null = unknown. */
export interface UploadLimits {
  /** `advanced.max_object_size` (bytes). */
  maxObjectSize: number | null;
  /** The bucket's `quota_bytes` policy. */
  quotaBytes: number | null;
  /** The bucket's stored bytes (the counter the server's quota gate reads). */
  usedBytes: number | null;
}

interface Sized {
  name: string;
  size: number;
}

/**
 * Split a batch into the files the server would take and the ones it would
 * refuse, BEFORE any byte goes out (the same rules as the server's gates).
 * The quota counts the files before each one in the batch. Unknown limits
 * pass everything: the server stays the authority.
 */
export function precheckUpload<F extends Sized>(files: F[], limits: UploadLimits): {
  accepted: F[];
  refused: { file: F; reason: string }[];
} {
  const accepted: F[] = [];
  const refused: { file: F; reason: string }[] = [];
  let used = limits.usedBytes;
  for (const file of files) {
    if (limits.maxObjectSize != null && file.size > limits.maxObjectSize) {
      refused.push({
        file,
        reason: `${formatBytes(file.size)} is over the ${formatBytes(limits.maxObjectSize)} object size limit (max_object_size)`,
      });
      continue;
    }
    if (limits.quotaBytes === 0) {
      refused.push({ file, reason: 'the bucket is frozen (quota 0)' });
      continue;
    }
    if (limits.quotaBytes != null && used != null) {
      const left = Math.max(0, limits.quotaBytes - used);
      if (file.size > left) {
        refused.push({
          file,
          reason: `${formatBytes(file.size)} does not fit in the bucket quota: ${formatBytes(left)} of ${formatBytes(limits.quotaBytes)} left`,
        });
        continue;
      }
      used += file.size;
    }
    accepted.push(file);
  }
  return { accepted, refused };
}
