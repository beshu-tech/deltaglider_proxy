/**
 * Pure helpers for object-browser keyboard navigation.
 *
 * Kept React-free (imports only the zero-dep `utils.ts` and the S3Object type)
 * so the row ordering and cursor math are unit-testable in a plain Node
 * regression script — same split rationale as `adminNavTree.ts`. The stateful
 * wiring lives in `useBrowserKeyboardNav.ts` and `ObjectTable.tsx`.
 */
import type { S3Object } from './types';
import { displayName, numericCompare } from './utils';

/** One ObjectTable row: a folder (`folder:<prefix>` key) or an object. */
export type BrowserRow =
  | { _isFolder: true; key: string; name: string }
  | (S3Object & { _isFolder: false; name: string });

export type SortColumn = 'name' | 'size' | 'modified';

/** Controlled ObjectTable sort (AntD cycle: ascend → descend → none). */
export interface SortState {
  column: SortColumn;
  order: 'ascend' | 'descend';
}

/** The unsorted rowKeys ObjectTable renders: folders first, then objects. */
export function rowKeysFor(folders: string[], objects: S3Object[]): string[] {
  return [...folders.map((f) => `folder:${f}`), ...objects.map((o) => o.key)];
}

/** Unsorted rows: folders first, then objects (same order as `rowKeysFor`). */
export function buildRows(folders: string[], objects: S3Object[], prefix: string): BrowserRow[] {
  return [
    ...folders.map((f) => ({ _isFolder: true as const, key: `folder:${f}`, name: displayName(f, prefix) })),
    ...objects.map((o) => ({ ...o, _isFolder: false as const, name: displayName(o.key, prefix) })),
  ];
}

function compareBy(column: SortColumn, folderSize: (folderPrefix: string) => number) {
  switch (column) {
    case 'name':
      return (a: BrowserRow, b: BrowserRow) => numericCompare(a.name, b.name);
    case 'size': {
      // Scanned folder size when known, so the order matches the rendered
      // cell; unscanned folders sort as 0.
      const size = (r: BrowserRow) => (r._isFolder ? folderSize(r.key.slice('folder:'.length)) : r.size);
      return (a: BrowserRow, b: BrowserRow) => size(a) - size(b);
    }
    case 'modified': {
      const ts = (r: BrowserRow) => (r._isFolder ? '' : r.lastModified || '');
      return (a: BrowserRow, b: BrowserRow) => numericCompare(ts(a), ts(b));
    }
  }
}

/**
 * The displayed row order — THE one ordering that the Table, HEAD enrichment,
 * the cursor page and keyboard navigation all share. Mirrors AntD's own
 * client-side sort exactly: a stable sort where `descend` negates the
 * comparator (ties keep their unsorted, folders-first order; they are NOT
 * reversed). Folders and objects interleave under every sorter, as before.
 */
export function sortRows(
  rows: BrowserRow[],
  sort: SortState | null,
  folderSize: (folderPrefix: string) => number,
): BrowserRow[] {
  if (!sort) return rows;
  const cmp = compareBy(sort.column, folderSize);
  const sign = sort.order === 'descend' ? -1 : 1;
  return rows.slice().sort((a, b) => sign * cmp(a, b));
}

/**
 * Pure cursor-movement: given the ordered keys, the current cursor, and a move,
 * return the next cursor key (or null when the list is empty).
 *
 *  - `'first'` / `'last'` jump to an end.
 *  - From "no cursor" (`null`), a forward step lands on the first row and a
 *    backward step on the last (so the first ↓ or ↑ always selects something).
 *  - Otherwise clamps to the list bounds (no wraparound).
 */
export function nextCursor(
  keys: string[],
  current: string | null,
  move: number | 'first' | 'last',
): string | null {
  if (keys.length === 0) return null;
  if (move === 'first') return keys[0];
  if (move === 'last') return keys[keys.length - 1];
  const idx = current === null ? -1 : keys.indexOf(current);
  if (idx === -1) return move > 0 ? keys[0] : keys[keys.length - 1];
  const next = Math.min(Math.max(idx + move, 0), keys.length - 1);
  return keys[next];
}

/**
 * The keys keyboard navigation walks: the order ObjectTable last reported
 * (its sorted rows) when that is still a permutation of the current rows,
 * else the unsorted `baseKeys`. The fallback covers the render between a
 * listing change and ObjectTable's next report, so a stale order never makes
 * the cursor land on a row that no longer exists.
 */
/**
 * True when two reported row orders are identical. The table reports a fresh
 * array on every render; storing an equal copy would re-render the browser,
 * rebuild the rows and report again — an update loop (React error #185).
 */
export function sameKeyOrder(a: readonly string[] | null, b: readonly string[]): boolean {
  return a !== null && a.length === b.length && a.every((k, i) => k === b[i]);
}

export function orderedRowKeys(baseKeys: string[], reported: string[] | null): string[] {
  if (!reported || reported.length !== baseKeys.length) return baseKeys;
  const current = new Set(baseKeys);
  return reported.every((k) => current.has(k)) ? reported : baseKeys;
}
