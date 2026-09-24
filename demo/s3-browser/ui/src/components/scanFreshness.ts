/**
 * Pure helpers for bucket-scan freshness.
 *
 * Bucket scans are cached server-side indefinitely (no TTL, no auto-delete).
 * The dashboard treats a scan as STALE once it's older than 6 hours and nudges
 * the operator to re-scan — but never auto-rescans or deletes anything. All of
 * that is a pure derivation off the scan's `completed_at` timestamp, so there's
 * no server-side TTL machinery: this module is the single source of the "is it
 * stale / how old is it" logic. React-free and unit-tested.
 */

/** Default staleness threshold: 6 hours, in milliseconds. */
export const SCAN_STALE_MS = 6 * 60 * 60 * 1000;

/**
 * Age of a scan in milliseconds, relative to `now` (default: Date.now()).
 * Returns null for a missing/unparseable timestamp. Never negative (a
 * clock-skew future timestamp clamps to 0).
 */
export function scanAgeMs(completedAt: string | null | undefined, now: number = Date.now()): number | null {
  if (!completedAt) return null;
  const t = Date.parse(completedAt);
  if (Number.isNaN(t)) return null;
  return Math.max(0, now - t);
}

/** True when the scan is older than `ttlMs` (default 6h). Missing/unparseable → not stale. */
export function isScanStale(
  completedAt: string | null | undefined,
  ttlMs: number = SCAN_STALE_MS,
  now: number = Date.now(),
): boolean {
  const age = scanAgeMs(completedAt, now);
  return age !== null && age > ttlMs;
}

