/**
 * Pure folder expansion for the browser's bulk actions (delete / copy / move / ZIP).
 *
 * React-free and fetch-free (the lister is injected) so a plain Node regression
 * script can exercise it — see scripts/bulk-selection-regression-test.mjs.
 */

/** Shape of `GET /api/admin/objects/list`: the server stops at 10,000 keys. */
export type PrefixLister = (prefix: string) => Promise<{ keys: string[]; truncated: boolean }>;

export interface ExpandedItem {
  /** Absolute source key. */
  source: string;
  /**
   * Destination suffix: the path under the selected folder, or the basename for
   * a directly selected object. Empty for a folder's own marker key (`foo/`).
   */
  relative: string;
}

/**
 * Expand a selection (`folder:<prefix>` entries and plain object keys) into
 * absolute keys. Every folder is listed BEFORE the caller mutates anything:
 * when a listing is truncated this throws, so a bulk action never acts on the
 * first 10,000 keys of a folder and silently leaves the rest behind.
 *
 * Dedupes by source key; the FIRST relative suffix wins, so a later overlapping
 * folder selection cannot shorten a path an earlier folder established.
 */
export async function expandSelection(
  selectedKeys: Iterable<string>,
  listPrefix: PrefixLister,
): Promise<ExpandedItem[]> {
  const seen = new Map<string, string>();
  for (const k of selectedKeys) {
    if (k.startsWith('folder:')) {
      const pfx = k.slice('folder:'.length);
      if (!pfx) continue; // never expand the whole bucket
      const { keys, truncated } = await listPrefix(pfx);
      if (truncated) {
        throw new Error(
          `Folder ${pfx} has more than ${keys.length.toLocaleString('en-US')} objects; narrow the selection.`,
        );
      }
      for (const nk of keys) {
        if (seen.has(nk)) continue;
        // Defensive: a listing key outside `pfx` falls back to its basename.
        seen.set(nk, nk.startsWith(pfx) ? nk.slice(pfx.length) : (nk.split('/').pop() || nk));
      }
    } else if (!seen.has(k)) {
      seen.set(k, k.split('/').pop() || k);
    }
  }
  return Array.from(seen, ([source, relative]) => ({ source, relative }));
}
