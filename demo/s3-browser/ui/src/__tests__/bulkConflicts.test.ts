import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'vitest';
import {
  MAX_DEST_FOLDERS,
  conflictConfirm,
  destinationFolders,
  findConflicts,
  overwriteSources,
  type TransferConflicts,
} from '../bulkConflicts';
import type { ExpandedItem, PrefixLister } from '../bulkSelection';
import { bulkTransferOutcomeMessage, mergeBatchResults, type BulkTransferOutcome } from '../bulkTransfer';
import { bulkProgressPercent, bulkProgressText } from '../bulkBatches';

const item = (source: string, relative: string): ExpandedItem => ({ source, relative });
/** A destination bucket that holds `keys`; lists direct children only (a delimiter), records the folders asked. */
function bucketWith(keys: string[]) {
  const asked: string[] = [];
  const list: PrefixLister = async (folder, startAfter) => {
    asked.push(folder);
    const all = keys
      .filter((k) => k.startsWith(folder) && !k.slice(folder.length).includes('/') && (startAfter === undefined || k > startAfter))
      .sort();
    return { keys: all, truncated: false };
  };
  return { list, asked };
}

test('destinationFolders: one listing per distinct destination folder', () => {
  const items = [item('a/x', 'dir/x'), item('a/y', 'dir/y'), item('a/z', 'dir/sub/z'), item('b/f', 'f.zip')];
  assert.deepEqual(destinationFolders(items, 'out/').sort(), ['out/', 'out/dir/', 'out/dir/sub/']);
  // The bucket root is a folder too.
  assert.deepEqual(destinationFolders([item('b/f', 'f.zip')], ''), ['']);
});

test('findConflicts: 10,000 files into the bucket root cost one listing, not 10,000', async () => {
  const items = Array.from({ length: 10_000 }, (_, i) => item(`s/${i}`, `${i}.zip`));
  const { list, asked } = bucketWith(['7.zip']);
  const { conflicts } = await findConflicts(items, false, '', list);
  assert.deepEqual(asked, ['']);
  assert.deepEqual(conflicts.map((c) => c.dest), ['7.zip']);
});

test('findConflicts: more than 20 files into a deep folder list its direct children only, once', async () => {
  const items = Array.from({ length: 30 }, (_, i) => item(`s/${i}`, `${i}.zip`));
  const { list, asked } = bucketWith(['out/3.zip', 'out/deep/a/b/c/d.zip']);
  const { conflicts } = await findConflicts(items, false, 'out/', list);
  assert.deepEqual(asked, ['out/']);
  assert.deepEqual(conflicts.map((c) => c.dest), ['out/3.zip']);
});

test('findConflicts: a selected folder is listed once, not twice', async () => {
  const items = Array.from({ length: 25 }, (_, i) => item(`d/${i}`, `d/${i}.zip`));
  const { list, asked } = bucketWith(['out/d/4.zip']);
  await findConflicts(items, false, 'out/', list);
  assert.deepEqual(asked, ['out/d/']);
});

test('findConflicts: only the exact destination keys that exist, not their neighbours', async () => {
  const { list } = bucketWith(['out/f.zip.bak', 'out/dir/x', 'out/dir/other']);
  const items = [item('a/x', 'dir/x'), item('a/y', 'dir/y'), item('b/f.zip', 'f.zip')];
  const { conflicts } = await findConflicts(items, false, 'out/', list);
  assert.deepEqual(conflicts.map((c) => c.dest), ['out/dir/x']);
});

test('findConflicts: an item that is its own destination is no conflict', async () => {
  const { list } = bucketWith(['out/f.zip']);
  assert.deepEqual((await findConflicts([item('out/f.zip', 'f.zip')], true, 'out/', list)).conflicts, []);
  assert.equal((await findConflicts([item('out/f.zip', 'f.zip')], false, 'out/', list)).conflicts.length, 1);
});

test('findConflicts: reads every page of a destination folder', async () => {
  const pages: PrefixLister = async (_p, after) =>
    after === undefined
      ? { keys: ['out/d/a'], truncated: true, next_start_after: 'out/d/a' }
      : { keys: ['out/d/z'], truncated: false };
  const { conflicts } = await findConflicts([item('s/z', 'd/z')], false, 'out/', pages);
  assert.deepEqual(conflicts.map((c) => c.dest), ['out/d/z']);
});

test('findConflicts: a folder that cannot be listed (403) does not fail the run; its items are unchecked', async () => {
  const denied: PrefixLister = async (folder) => {
    if (folder === 'out/drop/') throw new Error('AccessDenied');
    return { keys: ['out/ok/a'], truncated: false };
  };
  const items = [item('s/a', 'ok/a'), item('s/b', 'drop/b'), item('s/c', 'drop/c')];
  const check = await findConflicts(items, false, 'out/', denied);
  assert.deepEqual(check.conflicts.map((c) => c.dest), ['out/ok/a']);
  assert.equal(check.unchecked, 2);
});

test('findConflicts: a folder too large to read is unchecked, not a failure', async () => {
  const stuck: PrefixLister = async () => ({ keys: ['out/d/a'], truncated: true });
  const check = await findConflicts([item('s/z', 'd/z')], false, 'out/', stuck);
  assert.deepEqual(check, { conflicts: [], unchecked: 1 });
});

test('findConflicts: more folders than MAX_DEST_FOLDERS list only that many; the rest are unchecked', async () => {
  const items = Array.from({ length: MAX_DEST_FOLDERS + 5 }, (_, i) => item(`s/${i}`, `d${i}/f`));
  const { list, asked } = bucketWith([]);
  const check = await findConflicts(items, false, 'out/', list);
  assert.equal(asked.length, MAX_DEST_FOLDERS);
  assert.equal(check.unchecked, 5);
});

test('findConflicts: an abort rejects', async () => {
  const controller = new AbortController();
  const slow: PrefixLister = async () => {
    controller.abort();
    return { keys: [], truncated: false };
  };
  await assert.rejects(findConflicts([item('s/z', 'd/z')], false, 'out/', slow, { signal: controller.signal }));
});

test('findConflicts: reports progress per folder', async () => {
  const { list } = bucketWith([]);
  const seen: number[] = [];
  await findConflicts([item('a/x', 'd1/x'), item('a/y', 'd2/y')], false, 'out/', list, { onProgress: (p) => seen.push(p.checked) });
  assert.deepEqual(seen, [0, 1, 2]);
});

test('overwriteSources: only "overwrite" lets the conflicting items replace; skip and unchecked never do', () => {
  const conflicts = [{ ...item('a', 'a'), dest: 'out/a' }];
  assert.deepEqual([...overwriteSources(conflicts, 'overwrite')], ['a']);
  assert.deepEqual([...overwriteSources(conflicts, 'skip')], []);
});

test('mergeBatchResults adds the answers of the requests of one batch', () => {
  const a = { succeeded: 2, failed: 1, skipped: 1, deleted: 1, failures: [{ source_key: 's', dest_key: 'd', error: 'e' }] };
  const b = { succeeded: 1, failed: 0, failures: [] };
  assert.deepEqual(mergeBatchResults([a, b]), { succeeded: 3, failed: 1, skipped: 1, deleted: 1, failures: a.failures });
});

test('conflictConfirm: names the existing keys and what each answer does', () => {
  const c: TransferConflicts = {
    action: 'move',
    destBucket: 'archive',
    total: 10,
    conflicts: ['a', 'b', 'c', 'd'].map((n) => ({ source: `s/${n}`, relative: n, dest: `out/${n}` })),
  };
  const { title, body } = conflictConfirm(c);
  assert.equal(title, '4 objects already exist at the destination');
  assert.match(body[0], /archive: out\/a, out\/b, out\/c and 1 more\./);
  assert.match(body[1], /their sources are not deleted.*other 6 are moved/);
  assert.match(body[2], /replaced and cannot be restored; their sources are deleted/);
  assert.match(body[3], /nothing is moved/);
});

test('the outcome message says how many existing objects were skipped', () => {
  // `total` counts every item sent; the skipped ones are part of it.
  const base: BulkTransferOutcome = { action: 'copy', total: 3, succeeded: 3, failed: 0, deleted: 0, failures: [], cancelled: false };
  assert.equal(bulkTransferOutcomeMessage({ ...base, total: 5, skipped: 2 }).text, '3 objects copied. 2 existing objects at the destination were skipped.');
  assert.equal(bulkTransferOutcomeMessage({ ...base, total: 1, succeeded: 0, skipped: 1 }).text, 'Nothing was copied. 1 existing object at the destination was skipped.');
  assert.equal(bulkTransferOutcomeMessage(base).text, '3 objects copied');
});

test('the checking phase has its own words and percent', () => {
  const p = { action: 'copy', phase: 'checking', stopping: false, checked: 1, total: 4 } as const;
  assert.match(bulkProgressText(p), /Checking the destination.*1 of 4.*Nothing is written yet/);
  assert.equal(bulkProgressPercent(p), 25);
});

/** Source guard (F17): a copy or move request is sent only by the run that checks the destination first. */
test('only the bulk transfer run sends copy / move requests, and it checks the destination first', () => {
  const root = join(__dirname, '..');
  const users: string[] = [];
  const walk = (dir: string) => {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name);
      if (statSync(path).isDirectory()) {
        if (name !== '__tests__' && name !== 'test') walk(path);
      } else if (/\.tsx?$/.test(name) && /\b(bulkCopyObjects|bulkMoveObjects)\b/.test(readFileSync(path, 'utf8'))) {
        users.push(path.slice(root.length + 1));
      }
    }
  };
  walk(root);
  // The API module defines them; the run is the one caller.
  assert.deepEqual(users.sort(), ['adminApi/bulkObjects.ts', 'browser/useBulkActions.ts']);
  const run = readFileSync(join(root, 'browser/useBulkActions.ts'), 'utf8');
  const transfer = run.slice(run.indexOf('const bulkTransfer = useCallback'), run.indexOf('const bulkCopy = useCallback'));
  assert.match(transfer, /findConflicts\(/, 'bulkTransfer must look for existing destination objects');
  assert.match(transfer, /prepare:/, 'bulkTransfer must settle the conflicts before it sends');
  // The promise is the server's: every request says whether it may replace.
  assert.match(transfer, /no_overwrite: noOverwrite/, 'bulkTransfer must send no_overwrite');
  assert.doesNotMatch(transfer, /listAllUnderPrefix/, 'the destination check lists direct children, never a whole subtree');
});
