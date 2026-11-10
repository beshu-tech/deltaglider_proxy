import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'vitest';
import {
  FILE_PROBE_LIMIT,
  conflictConfirm,
  findConflicts,
  itemsAfterChoice,
  planChecks,
  type TransferConflicts,
} from '../bulkConflicts';
import type { ExpandedItem, PrefixLister } from '../bulkSelection';
import { bulkTransferOutcomeMessage, type BulkTransferOutcome } from '../bulkTransfer';
import { bulkProgressPercent, bulkProgressText } from '../bulkBatches';

const item = (source: string, relative: string): ExpandedItem => ({ source, relative });
/** A destination bucket that holds `keys`; records the prefixes asked. */
function bucketWith(keys: string[]) {
  const asked: string[] = [];
  const list: PrefixLister = async (prefix, startAfter) => {
    asked.push(prefix);
    const all = keys.filter((k) => k.startsWith(prefix) && (startAfter === undefined || k > startAfter)).sort();
    return { keys: all, truncated: false };
  };
  return { list, asked };
}

test('planChecks: a folder per selected folder, a probe per file', () => {
  const plan = planChecks([item('a/x', 'dir/x'), item('a/y', 'dir/sub/y'), item('b/f', 'f.zip')], 'out/');
  assert.deepEqual(plan, { folders: ['out/dir/'], files: ['out/f.zip'], wholeDestination: false });
});

test('planChecks: many files are checked with one listing of the destination folder', () => {
  const many = Array.from({ length: FILE_PROBE_LIMIT + 1 }, (_, i) => item(`s/${i}`, `${i}.zip`));
  assert.deepEqual(planChecks(many, 'out/'), { folders: [], files: [], wholeDestination: true });
  // The bucket root cannot be listed as a prefix: probe each file.
  assert.equal(planChecks(many, '').files.length, many.length);
});

test('findConflicts: only the exact destination keys that exist, not their neighbours', async () => {
  const { list } = bucketWith(['out/f.zip.bak', 'out/dir/x', 'out/dir/other']);
  const items = [item('a/x', 'dir/x'), item('a/y', 'dir/y'), item('b/f.zip', 'f.zip')];
  const conflicts = await findConflicts(items, false, 'out/', list);
  assert.deepEqual(conflicts.map((c) => c.dest), ['out/dir/x']);
});

test('findConflicts: a file is a conflict only when its exact key exists', async () => {
  const { list } = bucketWith(['out/f.zip']);
  const conflicts = await findConflicts([item('b/f.zip', 'f.zip'), item('b/g.zip', 'g.zip')], false, 'out/', list);
  assert.deepEqual(conflicts.map((c) => c.source), ['b/f.zip']);
});

test('findConflicts: an item that is its own destination is no conflict', async () => {
  const { list } = bucketWith(['out/f.zip']);
  assert.deepEqual(await findConflicts([item('out/f.zip', 'f.zip')], true, 'out/', list), []);
  assert.equal((await findConflicts([item('out/f.zip', 'f.zip')], false, 'out/', list)).length, 1);
});

test('findConflicts: reads every page of a destination folder', async () => {
  const pages: PrefixLister = async (_p, after) =>
    after === undefined
      ? { keys: ['out/d/a'], truncated: true, next_start_after: 'out/d/a' }
      : { keys: ['out/d/z'], truncated: false };
  const conflicts = await findConflicts([item('s/z', 'd/z')], false, 'out/', pages);
  assert.deepEqual(conflicts.map((c) => c.dest), ['out/d/z']);
});

test('findConflicts: a failed lookup rejects, so no write follows on an unchecked destination', async () => {
  const failing: PrefixLister = async () => {
    throw new Error('boom');
  };
  await assert.rejects(findConflicts([item('s/z', 'd/z')], false, 'out/', failing), /boom/);
});

test('findConflicts: a truncated page without a next position rejects', async () => {
  const stuck: PrefixLister = async () => ({ keys: ['out/d/a'], truncated: true });
  await assert.rejects(findConflicts([item('s/z', 'd/z')], false, 'out/', stuck), /too many to check/);
});

test('findConflicts: reports progress per lookup', async () => {
  const { list } = bucketWith([]);
  const seen: number[] = [];
  await findConflicts([item('a/x', 'd1/x'), item('a/y', 'd2/y')], false, 'out/', list, { onProgress: (p) => seen.push(p.checked) });
  assert.deepEqual(seen, [0, 1, 2]);
});

test('itemsAfterChoice: skip drops the conflicting items, overwrite keeps all', () => {
  const items = [item('a', 'a'), item('b', 'b')];
  const conflicts = [{ ...items[0], dest: 'out/a' }];
  assert.deepEqual(itemsAfterChoice(items, conflicts, 'skip'), [items[1]]);
  assert.deepEqual(itemsAfterChoice(items, conflicts, 'overwrite'), items);
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
  const base: BulkTransferOutcome = { action: 'copy', total: 3, succeeded: 3, failed: 0, deleted: 0, failures: [], cancelled: false };
  assert.equal(bulkTransferOutcomeMessage({ ...base, skipped: 2 }).text, '3 objects copied. 2 existing objects at the destination were skipped.');
  assert.equal(bulkTransferOutcomeMessage({ ...base, total: 0, succeeded: 0, skipped: 1 }).text, 'Nothing was copied. 1 existing object at the destination was skipped.');
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
});
