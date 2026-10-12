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
