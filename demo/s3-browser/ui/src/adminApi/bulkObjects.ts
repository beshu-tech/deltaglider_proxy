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
// - startDeleteJob / cancelDeleteJob: a delete runs on the server as a job
//   (it survives closing the tab and a restart); the bucket status shows it.
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

interface DeleteJobRequest {
  bucket: string;
  /** Folders (each ends with `/`): everything under each is deleted. */
  prefixes: string[];
  /** Objects selected directly. */
  keys: string[];
  /** The browser's count of the objects to delete: the job's total until it has done more. */
  expected: number;
}

/**
 * Start a delete job: the server deletes the folders and objects with the
 * session's permissions, page by page. Closing the tab does not stop it;
 * the bucket's status (`getBucketMaintenance`) shows its progress.
 */
export async function startDeleteJob(req: DeleteJobRequest): Promise<{ job_id: number }> {
  return adminJson('/api/admin/objects/delete-jobs', { method: 'POST', body: req, context: 'Start delete' });
}

/** Cancel a delete job: the keys deleted before it stops stay deleted. */
export async function cancelDeleteJob(id: number): Promise<{ status: string }> {
  return adminJson(`/api/admin/objects/delete-jobs/${id}/cancel`, { method: 'POST', context: 'Cancel delete' });
}

interface ListAllResponse {
  keys: string[];
  truncated: boolean;
  /** With `truncated`: the key the next page starts after. */
  next_start_after?: string;
}

/**
 * One page (at most 10,000 keys) of the absolute keys under `prefix`, after
 * `startAfter`. Server-side equivalent of the previous browser-side
 * `listAllKeys`. Callers go through `expandSelection` (bulkSelection.ts),
 * which reads every page of a folder.
 * For a user without admin rights the server lists only the visible keys,
 * and a folder of too many hidden keys answers 400 (an ApiError).
 */
export async function listAllUnderPrefix(
  bucket: string,
  prefix: string,
  startAfter?: string,
): Promise<ListAllResponse> {
  if (!prefix) throw new Error('listAllUnderPrefix: prefix must be non-empty');
  const qs = new URLSearchParams({ bucket, prefix });
  if (startAfter !== undefined) qs.set('start_after', startAfter);
  return adminJson(`/api/admin/objects/list?${qs.toString()}`, { context: 'List under prefix' });
}

/** The deepest folder (`a/b/`) that every key is under, or `''`. */
function sharedFolder(keys: string[]): string {
  let folder = keys.length ? keys[0].slice(0, keys[0].lastIndexOf('/') + 1) : '';
  for (const key of keys) {
    while (folder && !key.startsWith(folder)) {
      folder = folder.slice(0, folder.slice(0, -1).lastIndexOf('/') + 1);
    }
  }
  return folder;
}

/**
 * Build the same-origin URL for a server-streamed zip download of `keys`
 * in `bucket`. Used by the browser as an `<a href download>` target — no
 * JS-side body assembly. The bucket and the shared folder go once, and
 * the keys below that folder as a JSON array (a key may contain a comma).
 * That fits more keys in one URL than a list of full `bucket/key` names
 * (review A12). A session of the file browser is required when the URL is
 * fetched.
 */
export function bulkZipDownloadUrl(bucket: string, keys: string[]): string {
  const prefix = sharedFolder(keys);
  const qs = new URLSearchParams({
    bucket,
    prefix,
    keys: JSON.stringify(keys.map((k) => k.slice(prefix.length))),
  });
  return `${BASE}/api/admin/objects/zip?${qs.toString()}`;
}
