// ─── Bucket-wide object scan ──────────────────────────────────────────
//
// Backs the dashboard headline numbers. The scan walks every object in
// a bucket (paginated via list_objects) and produces an honest
// `total_objects`/`total_original_bytes`/`total_stored_bytes`. Results
// persist to disk on the server and survive restarts; there is no TTL.
import { throwApiError } from '../errorHandling';
import { adminFetch, fetchJson, safeJson } from './core';

/** Completed scan record returned by /scan/status (state="done"). */
export interface BucketScanResult {
  bucket: string;
  total_objects: number;
  total_original_bytes: number;
  total_stored_bytes: number;
  /**
   * Bytes occupied by `reference.bin` files across the bucket. Always
   * a subset of `total_stored_bytes`. Optional on the wire for backwards
   * compatibility with v1 scan results (those existed before the
   * consolidation, undercounted by this amount, and are dropped on load
   * — but a stale Rust process could in theory still emit them).
   */
  total_reference_bytes?: number;
  savings_percentage: number;
  started_at: string; // ISO-8601
  completed_at: string; // ISO-8601
  duration_ms: number;
  version: number;
}

/**
 * Live progress frame, also used as the SSE payload. The terminal
 * frame is the one where `finished === true`.
 */
export interface BucketScanProgress {
  bucket: string;
  objects: number;
  original_bytes: number;
  stored_bytes: number;
  pages_done: number;
  has_more: boolean;
  finished: boolean;
  error: string | null;
  started_at: string;
}

/**
 * Map of every bucket the server has a cached scan for, plus any scans
 * currently in flight (`running`). The dashboard reads `running` on mount to
 * re-attach its live view to scans that kept going server-side while the
 * operator was on another page — so navigating away mid-scan no longer loses it.
 */
export async function getAllBucketScans(): Promise<{
  buckets: Record<string, BucketScanResult>;
  running?: Record<string, BucketScanProgress>;
}> {
  return fetchJson('/api/admin/diagnostics/scan/status', 'Bucket scan status (all)');
}

/**
 * Kick off a scan (idempotent — returns the in-flight job if one is
 * already running). Returns the current progress frame so the caller
 * can render immediately without opening SSE.
 */
export async function startBucketScan(
  bucket: string,
): Promise<BucketScanProgress> {
  const res = await adminFetch(
    `/api/admin/diagnostics/scan/start?bucket=${encodeURIComponent(bucket)}`,
    'POST',
  );
  if (!res.ok) await throwApiError(res, 'Bucket scan start');
  return safeJson(res);
}

/** Cancel a running scan. No-op if nothing is running. */
export async function stopBucketScan(
  bucket: string,
): Promise<{ cancelled: boolean }> {
  const res = await adminFetch(
    `/api/admin/diagnostics/scan/stop?bucket=${encodeURIComponent(bucket)}`,
    'POST',
  );
  if (!res.ok) await throwApiError(res, 'Bucket scan stop');
  return safeJson(res);
}

/**
 * Subscribe to the SSE progress stream for a bucket. Opens an
 * EventSource against `/scan/stream?bucket=X`; the server starts a
 * scan if none is running. `onProgress` fires on every frame
 * (including the terminal `done` event); `onError` fires on transport
 * errors. Returns a cleanup function that closes the EventSource.
 *
 * NOTE: EventSource sends cookies cross-origin only when
 * `withCredentials: true`. We rely on session cookies for admin auth
 * so that flag is required.
 */
export function subscribeBucketScan(
  bucket: string,
  onProgress: (frame: BucketScanProgress) => void,
  onError?: (err: Event) => void,
): () => void {
  const url = `/_/api/admin/diagnostics/scan/stream?bucket=${encodeURIComponent(bucket)}`;
  const source = new EventSource(url, { withCredentials: true });
  const parseAndDispatch = (msg: MessageEvent) => {
    try {
      const frame = JSON.parse(msg.data) as BucketScanProgress;
      onProgress(frame);
    } catch {
      // Bad payload — surface as a transport error so the UI can
      // show "stream broken" rather than silently drop frames.
      if (onError) onError(new Event('parse-error'));
    }
  };
  source.addEventListener('progress', parseAndDispatch);
  source.addEventListener('done', (msg) => {
    parseAndDispatch(msg);
    source.close();
  });
  if (onError) source.addEventListener('error', onError);
  return () => source.close();
}
