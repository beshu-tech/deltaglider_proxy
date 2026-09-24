// === Bucket maintenance status (session-light browser banner) ===
import { adminJson } from './core';
import type { MaintenanceJobView } from '../maintenanceStatus';

/**
 * The bucket's active maintenance job (re-encrypt or migrate), if any.
 * Session-light: works for non-admin browser sessions too.
 */
export async function getBucketMaintenance(
  bucket: string
): Promise<{ active: MaintenanceJobView | null }> {
  return adminJson(`/api/admin/jobs/bucket/${encodeURIComponent(bucket)}`, {
    context: 'Bucket maintenance status',
  });
}
