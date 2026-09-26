/**
 * What this browser session is allowed to do (UI + s3client), aligned with the server’s
 * session checks (file-browser vs full administrator sign-in).
 */

type SessionCheck = { valid: boolean; admin_gui: boolean };

type SessionCapabilities = {
  /** Signed in through Admin (bootstrap, OAuth, or admin IAM login-as). */
  adminGui: boolean;
  /** Signed in with an access key on the connect screen — files only until Admin is opened. */
  signedInForFilesOnly: boolean;
  /** `GET /api/admin/buckets` merged into `listBuckets()` origins. */
  canLoadBucketOrigins: boolean;
  /** `GET /api/admin/config` (Inspector bucket policy, etc.). */
  canFetchFullAdminConfig: boolean;
  /**
   * Bulk copy / move / delete / ZIP (`/api/admin/objects/*`). Any signed-in
   * session: for a files-only session the server checks every key against
   * the user's own permissions and reports denied keys one by one.
   */
  canUseBulkActions: boolean;
};

const NONE: SessionCapabilities = {
  adminGui: false,
  signedInForFilesOnly: false,
  canLoadBucketOrigins: false,
  canFetchFullAdminConfig: false,
  canUseBulkActions: false,
};

export function deriveSessionCapabilities(s: SessionCheck): SessionCapabilities {
  if (!s.valid) return { ...NONE };
  const adminGui = s.admin_gui === true;
  const signedInForFilesOnly = s.valid && !adminGui;
  return {
    adminGui,
    signedInForFilesOnly,
    canLoadBucketOrigins: adminGui,
    canFetchFullAdminConfig: adminGui,
    canUseBulkActions: true,
  };
}
