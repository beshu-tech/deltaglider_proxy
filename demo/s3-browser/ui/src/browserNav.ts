/**
 * Pure helpers for object-browser keyboard navigation.
 *
 * Kept React-free (no imports beyond the S3Object type) so the cursor math is
 * unit-testable in a plain Node regression script — same split rationale as
 * `adminNavTree.ts`. The stateful wiring lives in `useBrowserKeyboardNav.ts`.
 */
import type { S3Object } from './types';

/** The ordered rowKeys ObjectTable renders: folders first, then objects. */
export function rowKeysFor(folders: string[], objects: S3Object[]): string[] {
  return [...folders.map((f) => `folder:${f}`), ...objects.map((o) => o.key)];
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
