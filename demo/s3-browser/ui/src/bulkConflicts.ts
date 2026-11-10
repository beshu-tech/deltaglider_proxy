/**
 * Destination conflicts of a bulk copy or move (F17, epic #146): the objects
 * that already exist where the run would write. The browser finds them
 * BEFORE the first write and asks: skip them, overwrite them, or cancel.
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

/** More direct files than this are checked by listing the destination folder once, not one request each. */
export const FILE_PROBE_LIMIT = 20;
/** Pages read from a destination folder before the check gives up (it then fails: nothing is written unchecked). */
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

/**
 * The destination keys that must be looked up: a folder per selected folder
 * (`dest/name/`: everything the run writes under it), and for the files
 * written straight into `destPrefix` either one exact-key probe each or, when
 * there are many, one listing of `destPrefix` itself.
 */
export function planChecks(
  items: ExpandedItem[],
  destPrefix: string,
): { folders: string[]; files: string[]; wholeDestination: boolean } {
  const folders = new Set<string>();
  const files: string[] = [];
  for (const item of items) {
    const slash = item.relative.indexOf('/');
    if (slash >= 0) folders.add(destPrefix + item.relative.slice(0, slash + 1));
    else files.push(destKeyOf(destPrefix, item));
  }
  const wholeDestination = files.length > FILE_PROBE_LIMIT && destPrefix !== '';
  return { folders: [...folders], files: wholeDestination ? [] : files, wholeDestination };
}

/** Every key under `prefix`, page by page. Throws when the folder is too large to read or the lister fails. */
async function listEvery(prefix: string, listPrefix: PrefixLister, signal?: AbortSignal): Promise<string[]> {
  const keys: string[] = [];
  let startAfter: string | undefined;
  for (let page = 0; page < MAX_DEST_PAGES; page++) {
    signal?.throwIfAborted();
    const answer = await listPrefix(prefix, startAfter);
    for (const k of answer.keys) keys.push(k);
    if (!answer.truncated) return keys;
    if (answer.next_start_after === undefined) break;
    startAfter = answer.next_start_after;
  }
  throw new Error(
    `The destination folder ${prefix} has more than ${count(keys.length)} objects, too many to check for existing files; choose a smaller destination.`,
  );
}

/**
 * The items whose destination key already holds an object. An item that
 * is its own destination (same bucket, same key) is no conflict: the server
 * leaves it where it is. Throws when any lookup fails: a write is never sent
 * on an unchecked destination.
 */
export async function findConflicts(
  items: ExpandedItem[],
  sameBucket: boolean,
  destPrefix: string,
  listPrefix: PrefixLister,
  { signal, onProgress }: CheckOptions = {},
): Promise<TransferConflict[]> {
  const { folders, files, wholeDestination } = planChecks(items, destPrefix);
  const existing = new Set<string>();
  // One lookup = one unit of work for the progress bar.
  const work: (() => Promise<void>)[] = [
    ...folders.map((folder) => async () => {
      for (const k of await listEvery(folder, listPrefix, signal)) existing.add(k);
    }),
    ...files.map((key) => async () => {
      // The exact key sorts first among the keys that start with it, so one page holds it.
      if ((await listPrefix(key)).keys.includes(key)) existing.add(key);
    }),
    ...(wholeDestination
      ? [
          async () => {
            for (const k of await listEvery(destPrefix, listPrefix, signal)) existing.add(k);
          },
        ]
      : []),
  ];
  let next = 0;
  let checked = 0;
  let failure: { error: unknown } | undefined;
  onProgress?.({ checked, total: work.length });
  const worker = async () => {
    while (!failure && !signal?.aborted && next < work.length) {
      try {
        await work[next++]();
        onProgress?.({ checked: ++checked, total: work.length });
      } catch (error) {
        failure ??= { error };
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(CHECK_CONCURRENCY, work.length) }, worker));
  if (failure) throw failure.error;
  signal?.throwIfAborted();
  return items.flatMap((item) => {
    const dest = destKeyOf(destPrefix, item);
    return existing.has(dest) && !(sameBucket && dest === item.source) ? [{ ...item, dest }] : [];
  });
}

/** The items to send after the user's answer: all of them, or without the ones whose destination exists. */
export function itemsAfterChoice(
  items: ExpandedItem[],
  conflicts: TransferConflict[],
  choice: Exclude<ConflictChoice, 'cancel'>,
): ExpandedItem[] {
  if (choice === 'overwrite') return items;
  const skip = new Set(conflicts.map((c) => c.source));
  return items.filter((i) => !skip.has(i.source));
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
