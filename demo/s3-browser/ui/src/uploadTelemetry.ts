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

// Failures that come back the same on every attempt: permission and quota
// rejections (403), a malformed request, a missing bucket, an object above the
// size limit. Offering "Retry" for these only invites a second failure.
const PERMANENT_UPLOAD_STATUSES = new Set([400, 403, 404, 405, 411, 413]);
const PERMANENT_UPLOAD_CODES = new Set([
  'AccessDenied',
  'EntityTooLarge',
  'InvalidArgument',
  'InvalidBucketName',
  'NoSuchBucket',
  'QuotaExceeded',
]);

/** True when retrying a failed upload can succeed: network errors, timeouts,
 *  throttling and server errors. `status` undefined = no HTTP answer. */
export function isRetryableUploadFailure(status?: number, code?: string): boolean {
  if (code && PERMANENT_UPLOAD_CODES.has(code)) return false;
  if (status !== undefined && PERMANENT_UPLOAD_STATUSES.has(status)) return false;
  return true;
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
