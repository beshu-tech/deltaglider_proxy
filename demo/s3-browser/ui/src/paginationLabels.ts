/**
 * Pure helpers for paginated-table labels. Kept in plain `.ts` (no
 * React imports) so they're trivially unit-testable from
 * src/__tests__/pageSize.test.ts.
 */

/**
 * Operator-facing description of the visible row range. Singular/
 * plural correct; thousands-grouped; degrades cleanly to "0 items"
 * when the listing is empty and "{n} items" when a single page
 * trivially fits everything.
 *
 * Examples:
 *   describeVisibleRange(0, 1, 100)   → "0 items"
 *   describeVisibleRange(1, 1, 100)   → "1 item"
 *   describeVisibleRange(75, 1, 100)  → "75 items"
 *   describeVisibleRange(1500, 2, 100) → "Showing 101–200 of 1,500 items · Page 2 of 15"
 */
/**
 * Clamp a 1-based page number to the range actually backed by data.
 * When a search/filter shrinks the row count, a page selected against
 * the old (larger) listing can fall past the last real page; this
 * returns the highest in-range page (≥ 1) so the table never renders
 * an empty slice or asks the parent to enrich out-of-range keys.
 *
 * Examples:
 *   clampPageToData(3, 50, 100)  → 1   (50 rows → only 1 page)
 *   clampPageToData(2, 250, 100) → 2   (250 rows → 3 pages, in range)
 *   clampPageToData(9, 250, 100) → 3   (past the last page → clamp)
 *   clampPageToData(1, 0, 100)   → 1   (empty listing → page 1)
 */
export function clampPageToData(
  page: number,
  totalRows: number,
  size: number,
): number {
  const totalPages = Math.max(1, Math.ceil(totalRows / size));
  if (!Number.isFinite(page) || page < 1) return 1;
  return Math.min(Math.floor(page), totalPages);
}

export function describeVisibleRange(
  total: number,
  page: number,
  size: number,
): string {
  if (total === 0) return '0 items';
  const start = (page - 1) * size + 1;
  const end = Math.min(page * size, total);
  const totalPages = Math.max(1, Math.ceil(total / size));
  const noun = total === 1 ? 'item' : 'items';
  const fmt = (n: number) => n.toLocaleString();
  if (totalPages === 1) {
    return `${fmt(total)} ${noun}`;
  }
  return `Showing ${fmt(start)}–${fmt(end)} of ${fmt(total)} ${noun} · Page ${page} of ${totalPages}`;
}
