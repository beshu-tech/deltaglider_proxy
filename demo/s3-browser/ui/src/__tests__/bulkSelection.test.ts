import assert from 'node:assert/strict';
import { test } from 'vitest';
import {
  expandSelection,
  bulkDeleteConfirmText,
  objectDeleteConfirmText,
  FOLDER_LIST_CONCURRENCY,
  type PrefixLister,
} from '../bulkSelection';

// Regression guard for BULK-TRUNCATED-EXPANSION: the server's folder listing
// (`GET /api/admin/objects/list`) stops at 10,000 keys and returns
// truncated:true. Bulk delete/copy/move/ZIP used to read only `keys`, so they
// acted on the first 10,000 objects and left the rest behind without a word.
// expandSelection must throw on a truncated folder BEFORE any mutation starts.

const tree: Record<string, string[]> = {
  'a/': ['a/', 'a/x.txt', 'a/sub/y.txt'],
  'a/sub/': ['a/sub/y.txt'],
  'big/': Array.from({ length: 10_000 }, (_, i) => `big/${i}`),
};
const calls: string[] = [];
const lister: PrefixLister = async (pfx) => {
  calls.push(pfx);
  return { keys: tree[pfx] ?? [], truncated: pfx === 'big/' };
};

test('folders expand and KEEP their own name', async () => {
  // --- folders expand and KEEP their own name: copying folder a/ into dest/
  //     yields dest/a/..., like any file manager (it used to flatten into dest/,
  //     so two selected folders' same-named files overwrote each other) ---------
  assert.deepEqual(await expandSelection(['folder:a/', 'top.bin', 'd/e/f.txt'], lister), [
    { source: 'a/', relative: 'a/' },
    { source: 'a/x.txt', relative: 'a/x.txt' },
    { source: 'a/sub/y.txt', relative: 'a/sub/y.txt' },
    { source: 'top.bin', relative: 'top.bin' },
    { source: 'd/e/f.txt', relative: 'f.txt' },
  ]);
});

test('overlapping selections dedupe; the FIRST relative suffix wins', async () => {
  assert.deepEqual(await expandSelection(['folder:a/', 'folder:a/sub/', 'a/x.txt'], lister), [
    { source: 'a/', relative: 'a/' },
    { source: 'a/x.txt', relative: 'a/x.txt' },
    { source: 'a/sub/y.txt', relative: 'a/sub/y.txt' },
  ]);
});

test('a nested folder keeps only its own name, not its parents', async () => {
  assert.deepEqual(await expandSelection(['folder:a/sub/'], lister), [
    { source: 'a/sub/y.txt', relative: 'sub/y.txt' },
  ]);
});

test('the empty prefix is never listed (no whole-bucket expansion)', async () => {
  calls.length = 0;
  assert.deepEqual(await expandSelection(['folder:'], lister), []);
  assert.deepEqual(calls, [], 'empty folder prefix must not reach the lister');
});

test('a truncated folder aborts the whole expansion with a clear message', async () => {
  await assert.rejects(
    expandSelection(['folder:a/', 'folder:big/'], lister),
    (e: unknown) => e instanceof Error && /Folder big\/ has more than 10,000 objects; narrow the selection/.test(e.message),
    'truncated listing must throw',
  );
});

test('the delete confirmation names folders (their contents go too)', () => {
  assert.equal(bulkDeleteConfirmText(2, 0), 'Delete 2 selected items? This cannot be undone.');
  assert.equal(bulkDeleteConfirmText(1, 1), 'Delete 1 selected item? It is a folder: everything inside is deleted too. This cannot be undone.');
  assert.equal(bulkDeleteConfirmText(3, 1), 'Delete 3 selected items? 1 of them is a folder: everything inside is deleted too. This cannot be undone.');

  // The inspector's single-object delete confirms too (it used to delete on the
  // first click), naming the object.
  assert.equal(objectDeleteConfirmText('builds/v1/app.zip'), 'Delete "builds/v1/app.zip"? This cannot be undone.');
});

// ── Concurrent folder listing (bulk delete of a page of folders) ──────────
// Selecting ~50 folders used to list them ONE AT A TIME with no feedback, so
// the page sat still for minutes. The expansion now lists a few folders at
// once and reports progress; the result must stay in SELECTION order.

/** A lister whose answers the test releases by hand; it counts requests in flight. */
function manualLister(answer: (pfx: string) => { keys: string[]; truncated: boolean }) {
  const pending = new Map<string, () => void>();
  const started: string[] = [];
  let inFlight = 0;
  let maxInFlight = 0;
  const list: PrefixLister = (pfx) =>
    new Promise((resolve) => {
      started.push(pfx);
      inFlight++;
      maxInFlight = Math.max(maxInFlight, inFlight);
      pending.set(pfx, () => {
        inFlight--;
        pending.delete(pfx);
        resolve(answer(pfx));
      });
    });
  return {
    list,
    started,
    pending,
    get maxInFlight() {
      return maxInFlight;
    },
  };
}

/** Let queued promise callbacks run (each await in the expansion takes a turn). */
const settle = () => new Promise((r) => setTimeout(r, 0));

const tenFolders = Array.from({ length: 10 }, (_, i) => `folder:f${i}/`);

test(`folders are listed ${FOLDER_LIST_CONCURRENCY} at a time, never more`, async () => {
  assert.equal(FOLDER_LIST_CONCURRENCY, 4);
  const l = manualLister((pfx) => ({ keys: [`${pfx}k`], truncated: false }));
  const run = expandSelection(tenFolders, l.list);
  await settle();
  assert.equal(l.started.length, 4, 'four listings start at once');
  // Release in reverse order: each completion lets exactly one more start.
  while (l.pending.size > 0) {
    const last = [...l.pending.keys()].pop()!;
    l.pending.get(last)!();
    await settle();
  }
  const items = await run;
  assert.equal(l.maxInFlight, 4, 'never more than four requests in flight');
  assert.equal(l.started.length, 10, 'every folder is listed once');
  // Merged in selection order, not completion order.
  assert.deepEqual(items.map((i) => i.source), tenFolders.map((f) => `${f.slice('folder:'.length)}k`));
});

test('progress: one call before the first listing, then one per listed folder', async () => {
  const seen: { listed: number; folders: number; keysFound: number }[] = [];
  await expandSelection(['folder:a/', 'top.bin', 'folder:a/sub/', 'folder:'], lister, {
    concurrency: 1,
    onProgress: (p) => seen.push({ ...p }),
  });
  // `folder:` (empty prefix) is not a folder to list; a plain key is not either.
  assert.deepEqual(seen, [
    { listed: 0, folders: 2, keysFound: 0 },
    { listed: 1, folders: 2, keysFound: 3 },
    { listed: 2, folders: 2, keysFound: 4 },
  ]);
});

test('out-of-order completion: dedupe still keeps the FIRST suffix in selection order', async () => {
  const finished: string[] = [];
  // a/ answers last, a/sub/ first: both must be in flight at once.
  const slow: PrefixLister = (pfx) =>
    new Promise((resolve) =>
      setTimeout(() => {
        finished.push(pfx);
        resolve({ keys: tree[pfx] ?? [], truncated: false });
      }, pfx === 'a/' ? 30 : 1),
    );
  const items = await expandSelection(['folder:a/', 'folder:a/sub/'], slow);
  assert.deepEqual(finished, ['a/sub/', 'a/'], 'a/sub/ completed first');
  assert.deepEqual(items, [
    { source: 'a/', relative: 'a/' },
    { source: 'a/x.txt', relative: 'a/x.txt' },
    // From a/ (first in the selection), not sub/y.txt from a/sub/.
    { source: 'a/sub/y.txt', relative: 'a/sub/y.txt' },
  ]);
});

test('a truncated folder stops further listing and rejects: nothing is returned to delete', async () => {
  const l = manualLister((pfx) => ({ keys: [`${pfx}k`], truncated: pfx === 'f1/' }));
  const run = expandSelection(tenFolders, l.list);
  const outcome = run.then(
    () => 'resolved',
    (e: Error) => e.message,
  );
  await settle();
  l.pending.get('f1/')!(); // the truncated answer arrives first
  await settle();
  // In-flight listings may finish, but no new folder starts.
  for (const release of [...l.pending.values()]) release();
  await settle();
  assert.equal(await outcome, 'Folder f1/ has more than 1 objects; narrow the selection.');
  assert.deepEqual(l.started, ['f0/', 'f1/', 'f2/', 'f3/'], 'no folder after the truncation is listed');
});

test('a failed listing stops further listing and rejects with its error', async () => {
  const l = manualLister((pfx) => ({ keys: [`${pfx}k`], truncated: false }));
  const failing: PrefixLister = (pfx) => (pfx === 'f0/' ? Promise.reject(new Error('list failed')) : l.list(pfx));
  const outcome = expandSelection(tenFolders, failing).then(
    () => 'resolved',
    (e: Error) => e.message,
  );
  await settle();
  for (const release of [...l.pending.values()]) release();
  await settle();
  assert.equal(await outcome, 'list failed');
  assert.deepEqual(l.started, ['f1/', 'f2/', 'f3/'], 'no new folder starts after the failure');
});

test('abort: no new folder is listed and the expansion rejects with the abort reason', async () => {
  const l = manualLister((pfx) => ({ keys: [`${pfx}k`], truncated: false }));
  const ctl = new AbortController();
  const outcome = expandSelection(tenFolders, l.list, { signal: ctl.signal }).then(
    () => 'resolved',
    (e: unknown) => e,
  );
  await settle();
  ctl.abort();
  for (const release of [...l.pending.values()]) release();
  await settle();
  assert.equal(await outcome, ctl.signal.reason);
  assert.equal(l.started.length, 4, 'only the listings already in flight ran');
});
