// ─────────────────────────────────────────────────────────────
// Compression health diagnostics
// ─────────────────────────────────────────────────────────────
import { throwApiError } from '../errorHandling';
import { adminFetch, safeJson } from './core';

/**
 * Coarse health classification for a single deltaspace, mirroring the
 * server-side `Efficiency` enum in `src/api/admin/delta_efficiency.rs`.
 */
export type DeltaEfficiency = 'excellent' | 'good' | 'fair' | 'poor' | 'no_reference';

export interface DeltaspaceEfficiencyReport {
  bucket: string;
  prefix: string;
  deltas: number;
  passthrough: number;
  reference_bytes: number | null;
  total_delta_bytes: number;
  /**
   * Under the HEAD-free scan path (S3 backend), this is a **lower
   * bound** — only passthrough sizes contribute, because delta original
   * sizes require a HEAD call to recover. Check `original_size_estimated`
   * before displaying as a real "original" total.
   */
  total_original_bytes: number;
  median_delta_bytes: number;
  max_delta_bytes: number;
  /**
   * `total_original - (reference + total_delta)`. `0` when
   * `original_size_estimated` is true (we have no honest original to
   * subtract from). UIs MUST gate on `original_size_estimated` before
   * rendering this as a savings figure — otherwise healthy S3
   * deltaspaces appear to have "negative savings".
   *
   * Wire type is `i64` server-side; JS `number` is precise only to
   * 2^53 (≈9 PB). Past that ceiling, the displayed value is an
   * approximation. The Rust side saturates at `i64::MIN`/`i64::MAX`
   * to keep the cast well-defined. No bucket today gets close to
   * either limit; documented here so a future report scanning an
   * EB-scale archive doesn't surprise the next operator.
   */
  savings_bytes: number;
  efficiency: DeltaEfficiency;
  /**
   * `median_delta_bytes / reference_bytes`. `null` when there's no
   * reference. Server-computed so the timeline view (redesigned panel)
   * can plot this directly without repeating the division.
   */
  ratio_median: number | null;
  /**
   * True when the report was built from a HEAD-free scan. In that
   * case `total_original_bytes` is a lower bound and `savings_bytes`
   * is `0` (sentinel for "unknown"). The efficiency verdict and
   * `ratio_median` are unaffected.
   */
  original_size_estimated: boolean;
  explanation: string;
}

export interface DeltaEfficiencyResponse {
  bucket: string;
  scanned_deltaspaces: number;
  reported_deltaspaces: number;
  min_deltas: number;
  reports: DeltaspaceEfficiencyReport[];
  computed_at: string; // ISO 8601
  cached: boolean;
}

/**
 * 202 Accepted shape — server has enqueued (or already running) a
 * background scan. Caller should poll `fetchDeltaEfficiency` until it
 * returns the full `DeltaEfficiencyResponse`.
 *
 * Not exported: callers see this shape only via the function return
 * types (`fetchDeltaEfficiency`, `triggerDeltaEfficiencyScan`) and use
 * the `'scanning' in r` discriminator to branch.
 */
interface DeltaEfficiencyScanning {
  scanning: true;
  bucket: string;
  min_deltas: number;
  status: 'scan_started' | 'scan_already_running';
}

/**
 * Discriminated union: either a fresh/cached result, or a 202 saying
 * "we're working on it, poll again". Callers switch on `'scanning' in r`.
 */
type DeltaEfficiencyFetchResult = DeltaEfficiencyResponse | DeltaEfficiencyScanning;

/**
 * Scan one bucket's deltaspaces and surface those whose reference
 * baseline produces too-large deltas. Same shape as the usage scanner:
 *   - 200 OK with the result if a fresh cached scan exists (5-min TTL).
 *   - 202 Accepted with `{ scanning: true }` when a background scan
 *     was just enqueued (or one is already running). Caller polls.
 */
export async function fetchDeltaEfficiency(
  bucket: string,
  minDeltas = 3,
): Promise<DeltaEfficiencyFetchResult> {
  const qs = new URLSearchParams({
    bucket,
    min_deltas: String(minDeltas),
  });
  const res = await adminFetch(`/api/admin/diagnostics/delta-efficiency?${qs.toString()}`);
  if (res.status === 202) {
    return safeJson(res) as Promise<DeltaEfficiencyScanning>;
  }
  if (!res.ok) await throwApiError(res, 'Compression health fetch');
  return safeJson(res);
}

/**
 * Force a re-scan even when a fresh cached result exists. Returns
 * 202 immediately; the caller should then poll `fetchDeltaEfficiency`.
 */
export async function triggerDeltaEfficiencyScan(
  bucket: string,
  minDeltas = 3,
): Promise<DeltaEfficiencyScanning> {
  const res = await adminFetch(
    '/api/admin/diagnostics/delta-efficiency/scan',
    'POST',
    { bucket, min_deltas: minDeltas },
  );
  if (!res.ok && res.status !== 202) await throwApiError(res, 'Compression health scan trigger');
  return safeJson(res);
}

/**
 * Per-delta entry in a verified scan. `ratio = delta_size /
 * original_size` — the TRUE per-file compression ratio (where lower
 * is better, > 1 means xdelta3 made the file bigger).
 *
 * Sorted ascending by `ratio` on the server so the UI gets
 * percentile picks for free.
 */
interface VerifiedDelta {
  key: string;
  original_size: number;
  delta_size: number;
  ratio: number;
}

/**
 * Result of an opt-in HEAD-based deep dive on one prefix. Sizes are
 * exact (HEAD recovers each delta's original-file size from S3 user
 * metadata) so `true_savings_bytes` is the honest budget number —
 * positive means DG is saving storage on this prefix, negative means
 * the originals would be cheaper to store as-is.
 *
 * `compression_ratio` is `1 − stored/original` as a fraction.
 */
export interface VerifyDeltaEfficiencyResponse {
  bucket: string;
  prefix: string;
  reference_bytes: number | null;
  deltas: number;
  passthrough_count: number;
  total_original_bytes: number;
  total_stored_bytes: number;
  /**
   * Wire type is `i64` server-side. JS `number` is precise to 2^53
   * (≈9 PB); past that ceiling the displayed value is an approximation.
   * Rust saturates at i64::MIN/MAX rather than wrapping. See
   * `savings_bytes` doc on `DeltaspaceEfficiencyReport` for context.
   */
  true_savings_bytes: number;
  compression_ratio: number | null;
  per_delta: VerifiedDelta[];
}

/**
 * POST /diagnostics/delta-efficiency/verify — opt-in deep dive that
 * fires one HEAD per delta to recover per-file original sizes, then
 * returns the true savings + per-delta breakdown.
 *
 * Cost: one prefix-scoped LIST + N HEADs (N = delta count). At
 * 64-way concurrency this is ~1-2 s for 700 deltas.
 */
export async function verifyDeltaEfficiency(
  bucket: string,
  prefix: string,
): Promise<VerifyDeltaEfficiencyResponse> {
  const res = await adminFetch(
    '/api/admin/diagnostics/delta-efficiency/verify',
    'POST',
    { bucket, prefix },
  );
  if (!res.ok) await throwApiError(res, 'Compression health verify');
  return safeJson(res);
}
