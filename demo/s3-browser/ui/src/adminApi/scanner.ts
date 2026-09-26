// === Usage Scanner ===
import { ApiError, isSessionExpired } from '../errorHandling';
import { adminJson, adminRequest } from './core';

interface ChildUsage {
  /** Logical (original) bytes — what the folder "is", same as a file's size. */
  size: number;
  /** Bytes stored on the backend, delta baselines included. */
  stored_size: number;
  objects: number;
  /** Some original sizes in this folder are not known; `size` is approximate. */
  sizes_estimated: boolean;
}

interface UsageEntry {
  prefix: string;
  bucket: string;
  /** Logical (original) bytes. */
  total_size: number;
  /** Bytes stored on the backend, delta baselines included. */
  stored_size: number;
  total_objects: number;
  children: Record<string, ChildUsage>;
  computed_at: string;
  age_seconds: number;
  /** Seconds past the cache TTL; 0 while fresh. */
  stale_seconds: number;
  truncated: boolean;
  /** Some original sizes are not known to the proxy (it never sends a request
   *  per object to find out); total_size is approximate. */
  sizes_estimated: boolean;
}

/** Trigger a background usage scan for a bucket/prefix. */
export async function scanPrefixUsage(bucket: string, prefix: string): Promise<void> {
  await adminRequest('/api/admin/usage/scan', {
    method: 'POST',
    body: { bucket, prefix },
    context: 'Prefix usage scan',
  });
}

/** Get cached usage entry for a bucket/prefix, or null if not cached yet. */
export async function getPrefixUsage(bucket: string, prefix: string): Promise<UsageEntry | null> {
  const params = new URLSearchParams({ bucket, prefix });
  // The server returns 200 `{ cached: false }` for the not-yet-scanned case
  // (a benign state, not a 404 — keeps the console clean).
  const body = await adminJson<UsageEntry | { cached: false } | null>(`/api/admin/usage?${params}`, {
    context: 'Usage query',
  });
  if (!body || (body as { cached?: boolean }).cached === false) return null;
  return body as UsageEntry;
}

/**
 * 401/403 (no admin session) is expected on bootstrap-only S3 browsers: the
 * usage/savings chips silently degrade (stay hidden) instead of erroring.
 * Every other failure still throws.
 */
async function nullWithoutAdminSession<T>(request: Promise<T>): Promise<T | null> {
  try {
    return await request;
  } catch (e) {
    if (e instanceof ApiError && (isSessionExpired(e) || e.status === 403)) return null;
    throw e;
  }
}

/**
 * Per-prefix delta-compression savings, reference-aware.
 *
 * Backed by `src/api/admin/savings.rs` + the central `SavingsTotals`
 * accumulator. Unlike a client-side aggregation over `headCache`, this
 * includes the on-disk `reference.bin` cost in `stored_bytes`, so the
 * chip never displays "100% saved" while a reference still lives on
 * disk. Server-side cached for 30 s.
 */
interface PrefixSavingsTotals {
  original_bytes: number;
  stored_bytes: number;
  reference_bytes: number;
  delta_stored_bytes: number;
  passthrough_bytes: number;
  reference_count: number;
  delta_count: number;
  passthrough_count: number;
}

export interface PrefixSavingsResponse {
  bucket: string;
  prefix: string;
  totals: PrefixSavingsTotals;
  /** 0..=99.99, or null when there's nothing measurable under the prefix. */
  savings_percentage: number | null;
  truncated: boolean;
  computed_at: string;
}

export async function getPrefixSavings(
  bucket: string,
  prefix: string,
): Promise<PrefixSavingsResponse | null> {
  const params = new URLSearchParams({ bucket, prefix });
  return nullWithoutAdminSession(
    adminJson(`/api/admin/deltaspace/savings?${params}`, { context: 'Prefix savings query' }),
  );
}

// === Per-bucket running usage counter (Ceph-style O(1) size) ===

/**
 * O(1) per-bucket size from the running counter (`src/bucket_usage.rs`),
 * maintained inline on every PUT/DELETE — no scan. `last_scan_at` is when an
 * authoritative full scan last reconciled it (null = never; the inline running
 * total is still shown). Returns null on 401/403 (no admin session) so callers
 * can silently degrade.
 */
export interface BucketUsage {
  bucket: string;
  object_count: number;
  logical_bytes: number;
  stored_bytes: number;
  savings_percentage: number | null;
  last_scan_at: number | null;
  never_scanned: boolean;
}

export async function getBucketUsage(bucket: string): Promise<BucketUsage | null> {
  return nullWithoutAdminSession(
    adminJson(`/api/admin/usage/bucket/${encodeURIComponent(bucket)}`, {
      context: 'Bucket usage query',
    }),
  );
}

/** Force an authoritative full scan and overwrite the counter; returns the
 *  reconciled row. The only O(n) path — the Refresh button. */
export async function refreshBucketUsage(bucket: string): Promise<BucketUsage | null> {
  const params = new URLSearchParams({ bucket });
  return nullWithoutAdminSession(
    adminJson(`/api/admin/usage/refresh?${params}`, { method: 'POST', context: 'Bucket usage refresh' }),
  );
}
