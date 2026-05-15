/**
 * Pure validation helpers for `usePersistedPageSize`. Kept React-free
 * so they can be exercised from a plain Node regression test
 * (`scripts/page-size-regression-test.mjs`).
 */

/**
 * Take a raw localStorage string (or null) and either return a valid
 * page size from `allowedSizes` or fall back to `defaultSize`.
 *
 * Returns `defaultSize` when:
 *   - the storage entry is absent (`raw == null`)
 *   - the value isn't a finite number (NaN, Infinity, gibberish)
 *   - the value is finite but not in the operator-facing allow-list
 *     (a tampered localStorage value the dropdown can't render would
 *     otherwise stick the size picker in a "no selection" state).
 */
export function coerceStoredPageSize(
  raw: string | null,
  defaultSize: number,
  allowedSizes: readonly number[],
): number {
  if (raw == null) return defaultSize;
  const parsed = Number(raw);
  if (!Number.isFinite(parsed) || !allowedSizes.includes(parsed)) {
    return defaultSize;
  }
  return parsed;
}
