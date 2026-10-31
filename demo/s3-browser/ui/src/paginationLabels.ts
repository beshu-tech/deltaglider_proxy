/**
 * Pure helpers for paginated-table labels. Kept in plain `.ts` (no
 * React imports) so they're trivially unit-testable from
 * src/__tests__/pageSize.test.ts.
 */

const fmt = (n: number) => n.toLocaleString();
const plural = (n: number, one: string, many: string) => `${fmt(n)} ${n === 1 ? one : many}`;

/**
 * What the listing holds, with the browser's nouns: folders and objects
 * ("objects", never "items"). Pure.
 *
 *   countPhrase(0, 0)  → "No objects"
 *   countPhrase(10, 0) → "10 objects"
 *   countPhrase(12, 2) → "2 folders and 10 objects"
 */
function countPhrase(total: number, folders: number): string {
  const objects = total - folders;
  if (total === 0) return 'No objects';
  if (folders === 0) return plural(objects, 'object', 'objects');
  if (objects === 0) return plural(folders, 'folder', 'folders');
  return `${plural(folders, 'folder', 'folders')} and ${plural(objects, 'object', 'objects')}`;
}

/**
 * Operator-facing description of the visible row range. `total` counts
 * every row; `folders` is how many of them are folders.
 *
 * Examples:
 *   describeVisibleRange(0, 1, 100)       → "No objects"
 *   describeVisibleRange(75, 1, 100)      → "75 objects"
 *   describeVisibleRange(1500, 2, 100, 12) → "Showing 101–200 of 12 folders and 1,488 objects · Page 2 of 15"
 */
export function describeVisibleRange(total: number, page: number, size: number, folders = 0): string {
  const phrase = countPhrase(total, folders);
  const totalPages = Math.max(1, Math.ceil(total / size));
  if (totalPages === 1) return phrase;
  const start = (page - 1) * size + 1;
  const end = Math.min(page * size, total);
  return `Showing ${fmt(start)}–${fmt(end)} of ${phrase} · Page ${page} of ${totalPages}`;
}
