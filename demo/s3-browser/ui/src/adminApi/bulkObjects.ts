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
//   semantics (NoSuchKey counts as deleted). Streams a progress line per key.
// - listAllUnderPrefix: replaces in-browser folder expansion that would
//   spin a recursive listObjectsV2.
// - bulkZipDownloadUrl: returns a same-origin URL the browser can use
//   directly with `<a href download>` — server streams the archive.
// =============================================================================
import { NDJSON, readNdjson } from '../ndjson';
import { BASE, adminJson, adminRequest, safeJson } from './core';

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

/** The running totals of a streamed bulk delete after one more key. */
export interface BulkDeleteProgress {
  /** Keys finished so far, deleted or failed. */
  done: number;
  deleted: number;
  failed: number;
}

/**
 * Delete `req.keys`. The request asks for the streamed answer: one JSON
 * line of totals per finished key, which go to `onProgress` as they
 * arrive, then a line with the answer. An older proxy answers once, in
 * JSON, and `onProgress` is not called. A failure before the batch starts
 * (a bad bucket, a maintenance job, an expired session) keeps its status,
 * as with `adminJson`. Admin GUI session required.
 */
export async function bulkDeleteObjects(
  req: BulkDeleteRequest,
  onProgress?: (progress: BulkDeleteProgress) => void,
): Promise<BulkDeleteResponse> {
  const context = 'Bulk delete';
  const res = await adminRequest('/api/admin/objects/delete', {
    method: 'POST',
    body: req,
    context,
    headers: { Accept: NDJSON },
  });
  if (!res.body || !(res.headers.get('content-type') ?? '').includes(NDJSON)) return safeJson(res);
  let answer: BulkDeleteResponse | undefined;
  await readNdjson(res.body, (line) => {
    const l = (line ?? {}) as Record<string, unknown>;
    // The status line is gone when the proxy fails mid-batch: it says so in a line.
    if (typeof l.error === 'string') throw new Error(`${context} failed: ${l.error}`);
    if (Array.isArray(l.failures)) answer = l as unknown as BulkDeleteResponse;
    else if (typeof l.done === 'number') onProgress?.(l as unknown as BulkDeleteProgress);
    else throw new Error(`${context}: unexpected line in the answer: ${JSON.stringify(line).slice(0, 120)}`);
  });
  if (!answer) {
    throw new Error(
      `${context}: the answer ended before the last line. The proxy finishes the delete on its own; reload the folder to see what is left.`,
    );
  }
  return answer;
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
 * For a user without admin rights the server lists only the visible keys,
 * and a folder of too many hidden keys answers 400 (an ApiError).
 */
export async function listAllUnderPrefix(bucket: string, prefix: string): Promise<ListAllResponse> {
  if (!prefix) throw new Error('listAllUnderPrefix: prefix must be non-empty');
  const qs = new URLSearchParams({ bucket, prefix });
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
