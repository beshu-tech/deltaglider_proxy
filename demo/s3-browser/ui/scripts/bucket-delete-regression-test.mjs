import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/bucketDelete.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'bucketDelete.ts',
});
const { deleteMenuEntry, deleteConfirmText } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// Menu entry.
assert.deepEqual(deleteMenuEntry(undefined), { label: 'Delete bucket (checking contents…)', disabled: true });
assert.deepEqual(deleteMenuEntry({ state: 'error' }), { label: 'Delete bucket…', disabled: false });
assert.deepEqual(deleteMenuEntry({ state: 'known', count: 0, truncated: false }), { label: 'Delete bucket…', disabled: false });
assert.equal(deleteMenuEntry({ state: 'known', count: 1, truncated: false }).label, 'Delete bucket (not empty: 1 object)');
assert.equal(deleteMenuEntry({ state: 'known', count: 100, truncated: true }).label, 'Delete bucket (not empty: 100+ objects)');

// The confirm says "empty" only when the probe answered 0 objects.
assert.match(deleteConfirmText({ state: 'known', count: 0, truncated: false }), /is empty/);
for (const p of [undefined, { state: 'error' }, { state: 'checking' }]) {
  assert.doesNotMatch(deleteConfirmText(p), /is empty/, `probe ${JSON.stringify(p)}`);
}

console.log('bucket delete regression checks passed');
