/**
 * Single place for “what can this HTTP session do?” — keeps UI, s3client, and
 * docs aligned with `session_light` vs `require_admin_gui_session` on the server.
 */

type SessionCheck = { valid: boolean; admin_gui: boolean };

type SessionCapabilities = {
  /** Full admin GUI cookie (bootstrap / login-as / OAuth admin) — not browser-lift. */
  adminGui: boolean;
  /** Valid `dgp_session` but only S3 browser lift (IAM secret connect / open-mode). */
  browserLiftOnly: boolean;
  /** `GET /_/stats`, analytics buckets view, `useAdminConfig`, etc. */
  canReadStats: boolean;
  /** `/api/admin/objects/*` bulk copy/move/delete/zip/list. */
  canBulkOps: boolean;
  /** Usage scanner folder sizes (`/api/admin/usage`). */
  canFolderScan: boolean;
  /** `GET /api/admin/buckets` merged into `listBuckets()` origins. */
  canLoadBucketOrigins: boolean;
  /** `GET /api/admin/config` (Inspector bucket policy, etc.). */
  canFetchFullAdminConfig: boolean;
};

const NONE: SessionCapabilities = {
  adminGui: false,
  browserLiftOnly: false,
  canReadStats: false,
  canBulkOps: false,
  canFolderScan: false,
  canLoadBucketOrigins: false,
  canFetchFullAdminConfig: false,
};

export function deriveSessionCapabilities(s: SessionCheck): SessionCapabilities {
  if (!s.valid) return { ...NONE };
  const adminGui = s.admin_gui === true;
  const browserLiftOnly = !adminGui;
  return {
    adminGui,
    browserLiftOnly,
    canReadStats: adminGui,
    canBulkOps: adminGui,
    canFolderScan: adminGui,
    canLoadBucketOrigins: adminGui,
    canFetchFullAdminConfig: adminGui,
  };
}
