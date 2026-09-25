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
   * Destination suffix: the selected folder's name plus the path under it
   * (`v1/x.tar`), or the basename for a directly selected object.
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
      // Keep the folder's own name: "firmware/v1.0.1/x" relative to the
      // folder's PARENT is "v1.0.1/x", so copying it into dest/ gives
      // dest/v1.0.1/x rather than flattening every folder into dest/.
      const parent = pfx.slice(0, pfx.slice(0, -1).lastIndexOf('/') + 1);
      for (const nk of keys) {
        if (seen.has(nk)) continue;
        // Defensive: a listing key outside `pfx` falls back to its basename.
        seen.set(nk, nk.startsWith(pfx) ? nk.slice(parent.length) : (nk.split('/').pop() || nk));
      }
    } else if (!seen.has(k)) {
      seen.set(k, k.split('/').pop() || k);
    }
  }
  return Array.from(seen, ([source, relative]) => ({ source, relative }));
}

/**
 * The bulk-delete confirmation text. Deleting used to fire on the first click
 * with no confirmation at all — a selected folder took everything under it.
 */
export function bulkDeleteConfirmText(selectedCount: number, folderCount: number): string {
  const items = `${selectedCount} selected ${selectedCount === 1 ? 'item' : 'items'}`;
  const folders =
    folderCount === 0
      ? ''
      : folderCount === selectedCount
        ? ` ${folderCount === 1 ? 'It is a folder' : 'They are folders'}: everything inside is deleted too.`
        : ` ${folderCount} of them ${folderCount === 1 ? 'is a folder' : 'are folders'}: everything inside is deleted too.`;
  return `Delete ${items}?${folders} This cannot be undone.`;
}

/** The inspector's single-object delete confirmation text. */
export function objectDeleteConfirmText(key: string): string {
  return `Delete "${key}"? This cannot be undone.`;
}
