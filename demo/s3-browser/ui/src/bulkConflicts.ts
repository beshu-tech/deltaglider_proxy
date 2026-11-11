/**
 * Destination conflicts of a bulk copy or move (F17, epic #146): the objects
 * that already exist where the run would write. The browser finds them
 * BEFORE the first write and asks: skip them, overwrite them, or cancel.
 * The listing is only the preview for that question. The server keeps the
 * promise: every write the user did not allow to replace an object is sent
 * with `no_overwrite`, and the server judges it under the object's lock.
 *
 * React-free and fetch-free (the lister is injected) so the node unit
 * project tests it — see src/__tests__/bulkConflicts.test.ts.
 */
import type { ExpandedItem, PrefixLister } from './bulkSelection';
import { count } from './bulkBatches';
import { noun } from './utils';

/** What the user answers when objects already exist at the destination. */
export type ConflictChoice = 'skip' | 'overwrite' | 'cancel';

/** An item whose destination key already holds an object. */
export interface TransferConflict extends ExpandedItem {
  /** The existing destination key. */
  dest: string;
}

export interface TransferConflicts {
  action: 'copy' | 'move';
  destBucket: string;
  /** Items of the whole plan. */
  total: number;
  conflicts: TransferConflict[];
}

/** Asked once, before the first write, when destination objects exist. */
export type ConflictResolver = (c: TransferConflicts) => Promise<ConflictChoice>;

/** Destination folders listed at most per check; the items of the others are written conditionally, unchecked. */
export const MAX_DEST_FOLDERS = 200;
/** Pages read from one destination folder before it counts as unchecked. */
const MAX_DEST_PAGES = 50;
/** Requests at the same time. */
const CHECK_CONCURRENCY = 4;

interface CheckProgress {
  checked: number;
  total: number;
}

interface CheckOptions {
  signal?: AbortSignal;
  onProgress?: (p: CheckProgress) => void;
}

/** The destination key of an item. `destPrefix` is `''` or ends with `/`. */
const destKeyOf = (destPrefix: string, item: ExpandedItem): string => destPrefix + item.relative;

/** The folder a destination key sits in: `''` at the bucket root, else ends with `/`. */
const folderOf = (destKey: string): string => destKey.slice(0, destKey.lastIndexOf('/') + 1);

/**
 * The destination folders to look into: one per distinct folder that a
 * destination key sits in, each listed once whatever the number of items.
 */
export function destinationFolders(items: ExpandedItem[], destPrefix: string): string[] {
  return [...new Set(items.map((item) => folderOf(destKeyOf(destPrefix, item))))];
}

/** The objects directly inside `folder`, page by page. Throws when the lister fails or the folder is too large. */
async function listDirect(folder: string, listFolder: PrefixLister, signal?: AbortSignal): Promise<string[]> {
  const keys: string[] = [];
  let startAfter: string | undefined;
  for (let page = 0; page < MAX_DEST_PAGES; page++) {
    signal?.throwIfAborted();
    const answer = await listFolder(folder, startAfter);
    for (const k of answer.keys) keys.push(k);
    if (!answer.truncated) return keys;
    if (answer.next_start_after === undefined) break;
    startAfter = answer.next_start_after;
  }
  throw new Error(`The destination folder ${folder} is too large to check for existing files.`);
}

/** What the check found: the existing destinations, and how many items it could not look up. */
export interface ConflictCheck {
  conflicts: TransferConflict[];
  /**
   * Items whose destination folder could not be listed (the caller may not
   * list it, the listing failed, or there are more folders than
   * MAX_DEST_FOLDERS). The check does not fail on them: the write is
   * conditional (`no_overwrite`), so the server decides per item, under its
   * own lock, and reports what it skipped.
   */
  unchecked: number;
}

/**
 * The items whose destination key already holds an object, found by listing
 * each destination folder once (direct children only: a deeper key cannot be
 * a destination). The listing is a preview for the question the user is
 * asked; it is not what keeps the run from overwriting (the conditional
 * write is). An item that is its own destination (same bucket, same key) is
 * no conflict: the server leaves it where it is. A folder that cannot be
 * listed does not fail the run. Only an abort of `signal` throws.
 */
export async function findConflicts(
  items: ExpandedItem[],
  sameBucket: boolean,
  destPrefix: string,
  listFolder: PrefixLister,
  { signal, onProgress }: CheckOptions = {},
): Promise<ConflictCheck> {
  const allFolders = destinationFolders(items, destPrefix);
  const folders = allFolders.slice(0, MAX_DEST_FOLDERS);
  const existing = new Set<string>();
  const unlisted = new Set<string>(allFolders.slice(MAX_DEST_FOLDERS));
  let next = 0;
  let checked = 0;
  onProgress?.({ checked, total: folders.length });
  const worker = async () => {
    while (!signal?.aborted && next < folders.length) {
      const folder = folders[next++];
      try {
        for (const k of await listDirect(folder, listFolder, signal)) existing.add(k);
      } catch (error) {
        if (signal?.aborted) throw error;
        unlisted.add(folder);
      }
      onProgress?.({ checked: ++checked, total: folders.length });
    }
  };
  await Promise.all(Array.from({ length: Math.min(CHECK_CONCURRENCY, folders.length) }, worker));
  signal?.throwIfAborted();
  const conflicts: TransferConflict[] = [];
  let unchecked = 0;
  for (const item of items) {
    const dest = destKeyOf(destPrefix, item);
    if (unlisted.has(folderOf(dest))) unchecked++;
    else if (existing.has(dest) && !(sameBucket && dest === item.source)) conflicts.push({ ...item, dest });
  }
  return { conflicts, unchecked };
}

/**
 * The sources whose copy may replace an existing destination object: the
 * conflicts when the user chose "overwrite", none otherwise. Every other
 * item is written conditionally, so "skip" and an unchecked destination
 * never overwrite.
 */
export function overwriteSources(
  conflicts: TransferConflict[],
  choice: Exclude<ConflictChoice, 'cancel'>,
): Set<string> {
  return choice === 'overwrite' ? new Set(conflicts.map((c) => c.source)) : new Set();
}

/** The words of the question: what exists, three examples, and what each answer does. */
export function conflictConfirm(c: TransferConflicts): { title: string; body: string[] } {
  const n = c.conflicts.length;
  const verb = c.action === 'copy' ? 'copy' : 'move';
  const examples = c.conflicts.slice(0, 3).map((x) => x.dest);
  const more = n > examples.length ? ` and ${count(n - examples.length)} more` : '';
  return {
    title: `${count(n)} ${noun(n, 'object')} already ${n === 1 ? 'exists' : 'exist'} at the destination`,
    body: [
      `In ${c.destBucket}: ${examples.join(', ')}${more}.`,
      `Skip existing: the ${count(n)} ${noun(n, 'object')} stay as they are${c.action === 'move' ? ' and their sources are not deleted' : ''}; the other ${count(Math.max(0, c.total - n))} are ${c.action === 'copy' ? 'copied' : 'moved'}.`,
      `Overwrite: the ${count(n)} existing ${noun(n, 'object')} are replaced and cannot be restored${c.action === 'move' ? '; their sources are deleted' : ''}.`,
      `Cancel: nothing is ${verb === 'copy' ? 'copied' : 'moved'}.`,
    ],
  };
}
