/**
 * Pure folder expansion for the browser's bulk actions (delete / copy / move / ZIP).
 *
 * React-free and fetch-free (the lister is injected) so a plain Node regression
 * script can exercise it — see src/__tests__/bulkSelection.test.ts.
 */

/** Shape of `GET /api/admin/objects/list`: the server stops at 10,000 keys. */
export type PrefixLister = (prefix: string) => Promise<{ keys: string[]; truncated: boolean }>;

type Listing = Awaited<ReturnType<PrefixLister>>;

/**
 * Folders listed at the same time. One at a time made a page of 50 selected
 * folders wait for 50 sequential requests; a small bound keeps the load on
 * the proxy low.
 */
export const FOLDER_LIST_CONCURRENCY = 4;

export interface ExpandedItem {
  /** Absolute source key. */
  source: string;
  /**
   * Destination suffix: the selected folder's name plus the path under it
   * (`v1/x.tar`), or the basename for a directly selected object.
   */
  relative: string;
}

export interface ListingProgress {
  /** Folders listed so far. */
  listed: number;
  /** Folders to list: the selected `folder:` entries with a non-empty prefix. */
  folders: number;
  /** Keys in the folders listed so far, before dedupe. */
  keysFound: number;
}

interface ExpandOptions {
  /** Folders listed at the same time (default `FOLDER_LIST_CONCURRENCY`). */
  concurrency?: number;
  /** Called once before the first listing, then after each listed folder. */
  onProgress?: (progress: ListingProgress) => void;
  /** When aborted, no new folder is listed and the expansion rejects with `signal.reason`. */
  signal?: AbortSignal;
}

function truncatedFolderError(prefix: string, listed: number): Error {
  return new Error(
    `Folder ${prefix} has more than ${listed.toLocaleString('en-US')} objects; narrow the selection.`,
  );
}

/**
 * List `prefixes` with at most `concurrency` requests in flight. A truncated
 * folder or a failed request stops new listings; the requests in flight
 * finish, then this rejects. Several truncated folders: the first in
 * `prefixes` order names the error, so the message does not depend on timing.
 */
async function listFolders(
  prefixes: string[],
  listPrefix: PrefixLister,
  { concurrency = FOLDER_LIST_CONCURRENCY, onProgress, signal }: ExpandOptions,
): Promise<Map<string, Listing>> {
  const listed = new Map<string, Listing>();
  let next = 0;
  let keysFound = 0;
  let stop = false;
  let failure: { error: unknown } | undefined;
  onProgress?.({ listed: 0, folders: prefixes.length, keysFound: 0 });
  const worker = async () => {
    while (!stop && !signal?.aborted && next < prefixes.length) {
      const prefix = prefixes[next++];
      try {
        const listing = await listPrefix(prefix);
        listed.set(prefix, listing);
        keysFound += listing.keys.length;
        onProgress?.({ listed: listed.size, folders: prefixes.length, keysFound });
        if (listing.truncated) stop = true;
      } catch (error) {
        failure ??= { error };
        stop = true;
      }
    }
  };
  const workers = Math.min(Math.max(1, concurrency), prefixes.length);
  await Promise.all(Array.from({ length: workers }, worker));
  const truncated = prefixes.find((p) => listed.get(p)?.truncated);
  if (truncated) throw truncatedFolderError(truncated, listed.get(truncated)!.keys.length);
  if (failure) throw failure.error;
  signal?.throwIfAborted();
  return listed;
}

/**
 * Expand a selection (`folder:<prefix>` entries and plain object keys) into
 * absolute keys. Every folder is listed BEFORE the caller mutates anything:
 * when a listing is truncated this throws, so a bulk action never acts on the
 * first 10,000 keys of a folder and silently leaves the rest behind.
 *
 * Folders are listed a few at a time (`listFolders`), but the result is built
 * in SELECTION order, never in completion order: it dedupes by source key and
 * the FIRST relative suffix wins, so a later overlapping folder selection
 * cannot shorten a path an earlier folder established.
 */
export async function expandSelection(
  selectedKeys: Iterable<string>,
  listPrefix: PrefixLister,
  options: ExpandOptions = {},
): Promise<ExpandedItem[]> {
  const selection = [...selectedKeys];
  // Never expand the whole bucket: `folder:` with an empty prefix is skipped.
  const prefixes = [
    ...new Set(selection.filter((k) => k.startsWith('folder:')).map((k) => k.slice('folder:'.length)).filter(Boolean)),
  ];
  const listings = await listFolders(prefixes, listPrefix, options);
  const seen = new Map<string, string>();
  for (const k of selection) {
    if (k.startsWith('folder:')) {
      const pfx = k.slice('folder:'.length);
      const listing = listings.get(pfx);
      if (!listing) continue; // the empty prefix
      // Keep the folder's own name: "firmware/v1.0.1/x" relative to the
      // folder's PARENT is "v1.0.1/x", so copying it into dest/ gives
      // dest/v1.0.1/x rather than flattening every folder into dest/.
      const parent = pfx.slice(0, pfx.slice(0, -1).lastIndexOf('/') + 1);
      for (const nk of listing.keys) {
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
