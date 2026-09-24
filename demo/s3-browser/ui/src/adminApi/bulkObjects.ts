// =============================================================================
// Server-side bulk object operations (Phase B of the SDK-removal migration).
//
// Routes: `POST|GET /_/api/admin/objects/{copy,move,delete,zip,list}`.
// **Trust model:** `require_admin_gui_session` only (not access-key file-browser
// sign-in). Handlers call the engine directly; there is no per-key IAM
// inside these endpoints. The UI gates them on `deriveSessionCapabilities(...)
// .adminGui` — never call these without a full admin session.
//
// - bulkCopyObjects / bulkMoveObjects: previously per-key for-loops with
//   silent partial-failure recovery. Now atomic on the server.
// - bulkDeleteObjects: replaces the SDK's batch delete; same idempotent
//   semantics (NoSuchKey counts as deleted).
// - listAllUnderPrefix: replaces in-browser folder expansion that would
//   spin a recursive listObjectsV2.
// - bulkZipDownloadUrl: returns a same-origin URL the browser can use
//   directly with `<a href download>` — server streams the archive.
// =============================================================================
import { BASE, adminJson } from './core';

interface BulkCopyItem {
  source_key: string;
  /** Suffix appended to dest_prefix to form the destination key. */
  relative: string;
}

interface BulkCopyRequest {
  source_bucket: string;
  dest_bucket: string;
  dest_prefix: string;
  items: BulkCopyItem[];
}

interface BulkCopyFailure {
  source_key: string;
  dest_key: string;
  error: string;
}

interface BulkCopyResponse {
  succeeded: number;
  failed: number;
  failures: BulkCopyFailure[];
}

interface BulkMoveResponse extends BulkCopyResponse {
  deleted: number;
}

/** Requires administrator sign-in in Settings (`403 admin_session_required` otherwise). */
export async function bulkCopyObjects(req: BulkCopyRequest): Promise<BulkCopyResponse> {
  return adminJson('/api/admin/objects/copy', { method: 'POST', body: req, context: 'Bulk copy' });
}

/** Admin GUI session required. */
export async function bulkMoveObjects(req: BulkCopyRequest): Promise<BulkMoveResponse> {
  return adminJson('/api/admin/objects/move', { method: 'POST', body: req, context: 'Bulk move' });
}

interface BulkDeleteRequest {
  bucket: string;
  keys: string[];
}

interface BulkDeleteResponse {
  deleted: number;
  failed: number;
  failures: { key: string; error: string }[];
}

/** Admin GUI session required. */
export async function bulkDeleteObjects(req: BulkDeleteRequest): Promise<BulkDeleteResponse> {
  return adminJson('/api/admin/objects/delete', { method: 'POST', body: req, context: 'Bulk delete' });
}

interface ListAllResponse {
  keys: string[];
  truncated: boolean;
}

/**
 * Recursively expand `prefix` to its absolute key list. Server-side
 * equivalent of the previous browser-side `listAllKeys`.
 * The server stops at 10,000 keys and sets `truncated`; callers go through
 * `expandSelection` (bulkSelection.ts), which refuses a truncated folder.
 * Admin GUI session required.
 */
export async function listAllUnderPrefix(bucket: string, prefix: string): Promise<ListAllResponse> {
  if (!prefix) throw new Error('listAllUnderPrefix: prefix must be non-empty');
  const qs = new URLSearchParams({ bucket, prefix });
  return adminJson(`/api/admin/objects/list?${qs.toString()}`, { context: 'List under prefix' });
}

/**
 * Build the same-origin URL for a server-streamed zip download. Used
 * by the browser as an `<a href download>` target — no JS-side body
 * assembly. Pass `bucketKeys` as `["bucket/key1", "bucket/key2"]`.
 * Admin GUI session required when the URL is fetched.
 */
export function bulkZipDownloadUrl(bucketKeys: string[]): string {
  const qs = new URLSearchParams({ keys: bucketKeys.join(',') });
  return `${BASE}/api/admin/objects/zip?${qs.toString()}`;
}
