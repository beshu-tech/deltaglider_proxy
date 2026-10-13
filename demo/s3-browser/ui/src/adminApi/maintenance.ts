// === Bucket maintenance status (session-light browser banner) ===
import { adminJson } from './core';
import type { MaintenanceJobView } from '../maintenanceStatus';

/**
 * The bucket's active maintenance job (re-encrypt, migrate or backfill), if
 * any, and `writes_blocked`: whether the write gate refuses uploads and deletes now.
 * They differ: a cancelling migrate releases the gate while its row is
 * still active. Session-light: works for non-admin browser sessions too.
 */
export async function getBucketMaintenance(
  bucket: string
): Promise<{ active: MaintenanceJobView | null; writes_blocked: boolean }> {
  return adminJson(`/api/admin/jobs/bucket/${encodeURIComponent(bucket)}`, {
    context: 'Bucket maintenance status',
  });
}
