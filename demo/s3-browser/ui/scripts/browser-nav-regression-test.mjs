import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

// Transpile browserNav.ts (type-only import, no runtime deps) to a data: URL.
const source = await readFile(new URL('../src/browserNav.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ES2020,
    target: ts.ScriptTarget.ES2020,
    importsNotUsedAsValues: ts.ImportsNotUsedAsValues.Remove,
  },
  fileName: 'browserNav.ts',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`;
const { rowKeysFor, nextCursor } = await import(moduleUrl);

// --- rowKeysFor: folders first (prefixed), then object keys -------------------
const folders = ['a/', 'b/'];
const objects = [{ key: 'a/x.txt' }, { key: 'a/y.txt' }];
assert.deepEqual(
  rowKeysFor(folders, objects),
  ['folder:a/', 'folder:b/', 'a/x.txt', 'a/y.txt'],
  'folders (prefixed) precede object keys, in order',
);
assert.deepEqual(rowKeysFor([], []), [], 'empty in, empty out');

const keys = rowKeysFor(folders, objects); // 4 rows

// --- nextCursor: empty list -> null ------------------------------------------
assert.equal(nextCursor([], null, 1), null, 'empty list yields null');
assert.equal(nextCursor([], 'x', 1), null, 'empty list yields null even with a stale cursor');

// --- first move from "no cursor" ---------------------------------------------
assert.equal(nextCursor(keys, null, 1), 'folder:a/', '↓ from nothing → first row');
assert.equal(nextCursor(keys, null, -1), 'a/y.txt', '↑ from nothing → last row');

// --- stepping + clamping (no wraparound) -------------------------------------
assert.equal(nextCursor(keys, 'folder:a/', 1), 'folder:b/', '↓ advances one');
assert.equal(nextCursor(keys, 'folder:b/', 1), 'a/x.txt', '↓ crosses folder→object boundary');
assert.equal(nextCursor(keys, 'folder:a/', -1), 'folder:a/', '↑ at top clamps (no wrap)');
assert.equal(nextCursor(keys, 'a/y.txt', 1), 'a/y.txt', '↓ at bottom clamps (no wrap)');
assert.equal(nextCursor(keys, 'a/x.txt', -1), 'folder:b/', '↑ crosses object→folder boundary');

// --- Home / End --------------------------------------------------------------
assert.equal(nextCursor(keys, 'a/x.txt', 'first'), 'folder:a/', 'Home → first row');
assert.equal(nextCursor(keys, 'folder:a/', 'last'), 'a/y.txt', 'End → last row');

// --- stale cursor (key no longer present) behaves like "no cursor" -----------
assert.equal(nextCursor(keys, 'gone', 1), 'folder:a/', 'stale cursor + ↓ → first row');
assert.equal(nextCursor(keys, 'gone', -1), 'a/y.txt', 'stale cursor + ↑ → last row');

console.log('browser-nav regression checks passed');
