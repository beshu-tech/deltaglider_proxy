import { clamp } from './utils';

export const DEFAULT_UPLOAD_PART_SIZE = 16 * 1024 * 1024; // 16 MiB
export const DEFAULT_UPLOAD_QUEUE_SIZE = 4;
const SPEED_WINDOW_MS = 5000;

export type UploadStatus =
  | 'queued'
  | 'uploading'
  | 'completing'
  | 'success'
  | 'error'
  | 'cancelled';

export interface ThroughputSample {
  atMs: number;
  loadedBytes: number;
}

/**
 * What the queue offers after a failed upload:
 * - `retry`: a transient failure; Retry is the next step.
 * - `after-fix`: fails again until someone acts (a full quota); Retry stays
 *   available as a secondary action next to the reason.
 * - `never`: the same request fails every time (permission, bad name, size).
 */
export type UploadRetryAdvice = 'retry' | 'after-fix' | 'never';

// Decided on the S3 error CODE first: S3 sends several transient codes with
// status 400, so the status alone misclassifies them.
const RETRYABLE_UPLOAD_CODES = new Set([
  // Multipart state lives in one proxy node's memory; a retry starts a new upload.
  'NoSuchUpload',
  'RequestTimeout',
  'IncompleteBody',
  'BadDigest',
  'SlowDown',
  'InternalError',
  'ServiceUnavailable',
]);
const PERMANENT_UPLOAD_CODES = new Set([
  'AccessDenied',
  'EntityTooLarge',
  'InvalidArgument',
  'InvalidBucketName',
  'NoSuchBucket',
]);
// Fallback when there is no known code.
const PERMANENT_UPLOAD_STATUSES = new Set([400, 403, 404, 405, 411, 413]);

/**
 * `status` 0 or undefined = no HTTP answer (network error). `detail` is the
 * server's message: the proxy rejects a full quota as 403 AccessDenied with a
 * message that names the quota.
 */
export function uploadRetryAdvice(status?: number, code?: string, detail?: string): UploadRetryAdvice {
  if (code === 'QuotaExceeded' || (status === 403 && /quota/i.test(detail ?? ''))) return 'after-fix';
  if (code && RETRYABLE_UPLOAD_CODES.has(code)) return 'retry';
  if (code && PERMANENT_UPLOAD_CODES.has(code)) return 'never';
  if (status && PERMANENT_UPLOAD_STATUSES.has(status)) return 'never';
  return 'retry';
}

/** The queue label: the path under the destination folder, so two README.md
 *  files from different subfolders of a folder upload can be told apart. */
export function uploadDisplayPath(key: string, destination: string): string {
  return destination && key.startsWith(`${destination}/`) ? key.slice(destination.length + 1) : key;
}

export function clampPercent(value: number): number {
  return clamp(value, 0, 100);
}

// Merge an incoming telemetry totalBytes against the queue item's known size.
// A legitimate 0 (0-byte upload) must be preserved, so use nullish coalescing
// rather than `||` (which would treat 0 as "no value" and fall back). The
// fallback still covers telemetry events that arrive before the size is known
// (undefined/null totalBytes).
export function mergeTotalBytes(
  telemetryTotal: number | null | undefined,
  itemTotal: number,
): number {
  return telemetryTotal ?? itemTotal;
}

export function estimateTotalParts(totalBytes: number, partSize: number): number {
  if (partSize <= 0 || totalBytes <= 0) return 0;
  return Math.max(1, Math.ceil(totalBytes / partSize));
}

export function estimateCompletedParts(
  loadedBytes: number,
  totalBytes: number,
  partSize: number,
): number {
  const totalParts = estimateTotalParts(totalBytes, partSize);
  if (totalParts === 0) return 0;
  if (loadedBytes >= totalBytes && totalBytes > 0) return totalParts;
  return Math.min(totalParts, Math.floor(Math.max(0, loadedBytes) / partSize));
}

export function estimateInFlightParts(
  status: UploadStatus,
  totalParts: number,
  completedParts: number,
  queueSize: number,
): number {
  if (status !== 'uploading' && status !== 'completing') return 0;
  const remaining = Math.max(0, totalParts - completedParts);
  if (remaining === 0) return 0;
  return Math.min(Math.max(1, queueSize), remaining);
}

export function appendThroughputSample(
  samples: ThroughputSample[],
  sample: ThroughputSample,
  windowMs = SPEED_WINDOW_MS,
): ThroughputSample[] {
  const threshold = sample.atMs - windowMs;
  const pruned = samples.filter((s) => s.atMs >= threshold);
  return [...pruned, sample];
}

export function movingAverageSpeedBps(samples: ThroughputSample[]): number {
  if (samples.length < 2) return 0;
  const first = samples[0];
  const last = samples[samples.length - 1];
  const elapsedMs = last.atMs - first.atMs;
  const bytes = last.loadedBytes - first.loadedBytes;
  if (elapsedMs <= 0 || bytes <= 0) return 0;
  return (bytes / elapsedMs) * 1000;
}
