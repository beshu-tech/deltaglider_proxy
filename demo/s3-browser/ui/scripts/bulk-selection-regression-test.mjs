import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Regression guard for BULK-TRUNCATED-EXPANSION: the server's folder listing
// (`GET /api/admin/objects/list`) stops at 10,000 keys and returns
// truncated:true. Bulk delete/copy/move/ZIP used to read only `keys`, so they
// acted on the first 10,000 objects and left the rest behind without a word.
// expandSelection must throw on a truncated folder BEFORE any mutation starts.

const source = await readFile(new URL('../src/bulkSelection.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'bulkSelection.ts',
});
const { expandSelection, bulkDeleteConfirmText, objectDeleteConfirmText } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

const tree = {
  'a/': ['a/', 'a/x.txt', 'a/sub/y.txt'],
  'a/sub/': ['a/sub/y.txt'],
  'big/': Array.from({ length: 10_000 }, (_, i) => `big/${i}`),
};
const calls = [];
const lister = async (pfx) => {
  calls.push(pfx);
  return { keys: tree[pfx] ?? [], truncated: pfx === 'big/' };
};

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

// --- overlapping selections dedupe; the FIRST relative suffix wins ------------
assert.deepEqual(await expandSelection(['folder:a/', 'folder:a/sub/', 'a/x.txt'], lister), [
  { source: 'a/', relative: 'a/' },
  { source: 'a/x.txt', relative: 'a/x.txt' },
  { source: 'a/sub/y.txt', relative: 'a/sub/y.txt' },
]);

// --- a nested folder keeps only its own name, not its parents ----------------
assert.deepEqual(await expandSelection(['folder:a/sub/'], lister), [
  { source: 'a/sub/y.txt', relative: 'sub/y.txt' },
]);

// --- the empty prefix is never listed (no whole-bucket expansion) ------------
calls.length = 0;
assert.deepEqual(await expandSelection(['folder:'], lister), []);
assert.deepEqual(calls, [], 'empty folder prefix must not reach the lister');

// --- a truncated folder aborts the whole expansion with a clear message ------
await assert.rejects(
  expandSelection(['folder:a/', 'folder:big/'], lister),
  (e) => e instanceof Error && /Folder big\/ has more than 10,000 objects; narrow the selection/.test(e.message),
  'truncated listing must throw',
);

// --- the delete confirmation names folders (their contents go too) ---------
assert.equal(bulkDeleteConfirmText(2, 0), 'Delete 2 selected items? This cannot be undone.');
assert.equal(bulkDeleteConfirmText(1, 1), 'Delete 1 selected item? It is a folder: everything inside is deleted too. This cannot be undone.');
assert.equal(bulkDeleteConfirmText(3, 1), 'Delete 3 selected items? 1 of them is a folder: everything inside is deleted too. This cannot be undone.');

// The inspector's single-object delete confirms too (it used to delete on the
// first click), naming the object.
assert.equal(objectDeleteConfirmText('builds/v1/app.zip'), 'Delete "builds/v1/app.zip"? This cannot be undone.');

console.log('bulk-selection regression checks passed');
