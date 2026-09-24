import type { AdminConfig } from './adminApi';

type BucketPolicy = AdminConfig['bucket_policies'][string];

/** A bucket's policy entry: exact name first, then the lower-cased name
 *  (policy keys are stored lower-case). The one lookup rule for the UI. */
export function bucketPolicyFor(
  config: Pick<AdminConfig, 'bucket_policies'> | null | undefined,
  bucket: string,
): BucketPolicy | undefined {
  const policies = config?.bucket_policies;
  return policies?.[bucket] ?? policies?.[bucket.toLowerCase()];
}
